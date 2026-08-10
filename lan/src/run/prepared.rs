//! Driving a session that is already built.
//!
//! Splitting this from [`run`](super::run) gives two things. A host that
//! already owns a mentra [`Runtime`](mentra::Runtime) — with its own provider,
//! store, or custom tools — can still use lan's context discovery and event
//! stream instead of reimplementing them. And lan's own tests can drive the
//! whole pipeline against a scripted runtime, so the event contract is checked
//! without a network call.

use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

use mentra::{ContentBlock, Session, SessionEvent, SessionEventReceiver, runtime::RunOptions};
use tokio::sync::{
    broadcast::error::{RecvError, TryRecvError},
    oneshot,
};

use mentra::{
    SessionPermissionHandle,
    runtime::CancellationToken,
    session::{PermissionDecision, PermissionRuleScope},
};

use super::{EventSink, RunError, RunReport};
use crate::{
    approval::{AllowAll, ApprovalDecision, ApprovalRequest, Approver},
    context::WorkspaceContext,
    event::{ContextFile, EVENT_SCHEMA_VERSION, Event, NoticeSeverity, RunOutcome, SkillSummary},
};

/// What a run is about, once the runtime questions are settled.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub workspace: PathBuf,
    pub prompt: String,
    pub provider: String,
    pub model: String,
    pub context: WorkspaceContext,
    /// Skills directories registered on the runtime, most specific first.
    pub skills_dirs: Vec<PathBuf>,
    /// The skills those directories actually produced, after layering.
    pub skills: Vec<LoadedSkill>,
}

/// A skill available to the run, without its body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Limits and stop signals for a single turn.
///
/// lan's own type rather than a re-export of mentra's `RunOptions`: the same
/// reasoning as [`Event`] — lan owns its surface so mentra's internals can move
/// without breaking lan's callers. Only the knobs a harness actually needs are
/// exposed; the rest stay at mentra's defaults.
#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    /// Trips to abandon the turn. The turn fails and is rolled back — what a
    /// client's stop button means.
    pub cancel: Option<CancellationToken>,
    /// Trips to end the turn gracefully at the next round boundary, keeping
    /// what the model has already committed.
    pub stop: Option<CancellationToken>,
    /// Gives up on the turn after this long.
    pub deadline: Option<Duration>,
    /// Caps how many tool calls one turn may make.
    pub tool_budget: Option<usize>,
}

impl TurnOptions {
    /// A turn that can be cancelled through the returned token.
    pub fn cancellable() -> (Self, CancellationToken) {
        let token = CancellationToken::default();
        (
            Self {
                cancel: Some(token.clone()),
                ..Self::default()
            },
            token,
        )
    }

    pub fn with_deadline(self, deadline: Duration) -> Self {
        Self {
            deadline: Some(deadline),
            ..self
        }
    }

    pub fn with_tool_budget(self, tool_budget: usize) -> Self {
        Self {
            tool_budget: Some(tool_budget),
            ..self
        }
    }

    fn into_run_options(self) -> RunOptions {
        RunOptions {
            cancellation: self.cancel,
            stop: self.stop,
            deadline: self.deadline.map(|after| SystemTime::now() + after),
            tool_budget: self.tool_budget,
            ..RunOptions::default()
        }
    }
}

/// A session and the prompt to send it. Nothing has been sent yet.
pub struct PreparedRun {
    session: Session,
    run: RunContext,
}

/// Hand-written because mentra's `Session` is not `Debug`, and because the
/// context documents hold whole files — a derived impl would dump them.
impl std::fmt::Debug for PreparedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRun")
            .field("session_id", &self.session.id().to_string())
            .field("workspace", &self.run.workspace)
            .field("provider", &self.run.provider)
            .field("model", &self.run.model)
            .field("context_files", &self.run.context.documents().len())
            .field("skills", &self.run.skills.len())
            .finish_non_exhaustive()
    }
}

impl PreparedRun {
    pub fn new(session: Session, run: RunContext) -> Self {
        Self { session, run }
    }

    /// The header line this run will open with, before anything is sent.
    pub fn header(&self) -> Event {
        header_for(&self.session.id().to_string(), &self.run)
    }

    /// The session this run drives, for a host that wants mentra's own surface
    /// — branching, the transcript tree, subagents — alongside lan's.
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Gives the session back, ending lan's involvement.
    pub fn into_session(self) -> Session {
        self.session
    }

    /// The session's id, which changes every time a session is created —
    /// including on resume.
    pub fn session_id(&self) -> String {
        self.session.id().to_string()
    }

    /// The persisted agent id: the handle [`resume`](super::resume) takes.
    ///
    /// Unlike the session id this survives the process, because it names the
    /// row in mentra's store rather than this run of it.
    pub fn agent_id(&self) -> &str {
        self.session.agent_id()
    }

    /// The committed conversation so far, oldest first.
    pub fn history(&self) -> &[mentra::Message] {
        self.session.history()
    }

    /// What this run is about, minus the session.
    pub fn context(&self) -> &RunContext {
        &self.run
    }

    /// Sends the configured prompt and streams the turn into `sink`.
    ///
    /// The stream always opens with [`Event::RunStarted`] and always closes
    /// with [`Event::RunFinished`], including when the turn fails: by then the
    /// stream has content a client needs to be able to finish reading.
    ///
    /// The session survives, so this can be called again — see
    /// [`send`](Self::send) for a turn with a different prompt.
    pub async fn execute<S: EventSink>(&mut self, sink: S) -> Result<RunReport<S>, RunError> {
        self.execute_with_approver(sink, AllowAll).await
    }

    /// Sends the configured prompt, streaming into `sink` and routing any
    /// approval request to `approver`.
    ///
    /// The approver runs on the forwarding task while the turn is blocked
    /// waiting on it, which is what makes an interactive answer possible at
    /// all — and what means an approver must answer rather than defer.
    pub async fn execute_with_approver<S: EventSink, A: Approver>(
        &mut self,
        sink: S,
        approver: A,
    ) -> Result<RunReport<S>, RunError> {
        let prompt = self.run.prompt.clone();
        self.turn(prompt, sink, approver, TurnOptions::default())
            .await
    }

    /// Sends a further prompt on the same conversation.
    ///
    /// This is what separates a session from a one-shot: the model sees every
    /// earlier turn, because the session was never thrown away.
    pub async fn send<S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        sink: S,
        approver: A,
    ) -> Result<RunReport<S>, RunError> {
        self.turn(prompt.into(), sink, approver, TurnOptions::default())
            .await
    }

    /// Sends a prompt with explicit run options — a cancellation token, a
    /// deadline, a tool budget.
    ///
    /// This is what a protocol server's stop button needs: ACP's
    /// `session/cancel` trips the token, and the turn ends rather than running
    /// to completion unheard.
    pub async fn send_with_options<S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        self.turn(prompt.into(), sink, approver, options).await
    }

    /// One turn, start to finish: header, forwarded events, outcome.
    async fn turn<S: EventSink, A: Approver>(
        &mut self,
        prompt: String,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        if prompt.trim().is_empty() {
            return Err(RunError::EmptyPrompt);
        }

        let permissions = self.session.permission_handle();
        let session_id = self.session.id().to_string();
        let receiver = self.session.subscribe();

        let mut sink = sink;
        sink.emit(header_for(&session_id, &self.run))?;

        let (done_tx, done_rx) = oneshot::channel();
        let forwarder = tokio::spawn(forward_events(
            receiver,
            sink,
            done_rx,
            approver,
            permissions,
        ));

        let turn = self
            .session
            .append_turn_with_options(vec![ContentBlock::text(prompt)], options.into_run_options())
            .await;

        // The forwarder stops on this signal rather than on the channel
        // closing, so a sender clone held elsewhere in the runtime cannot
        // strand the task.
        let _ = done_tx.send(());
        let mut sink = forwarder.await?;

        let (final_message, outcome) = match turn {
            Ok(message) => (Some(message.text()), RunOutcome::Ok),
            Err(error) => (
                None,
                RunOutcome::Error {
                    message: error.to_string(),
                },
            ),
        };

        sink.emit(Event::RunFinished {
            outcome: outcome.clone(),
        })?;

        Ok(RunReport {
            session_id,
            model: self.run.model.clone(),
            provider: self.run.provider.clone(),
            final_message,
            outcome,
            sink,
        })
    }
}

/// Builds the opening line. Kept separate so [`PreparedRun::header`] and the
/// line actually emitted can never drift apart.
fn header_for(session_id: &str, run: &RunContext) -> Event {
    Event::RunStarted {
        schema: EVENT_SCHEMA_VERSION,
        lan: env!("CARGO_PKG_VERSION").to_string(),
        session_id: session_id.to_string(),
        workspace: run.workspace.clone(),
        model: run.model.clone(),
        provider: run.provider.clone(),
        context_files: run
            .context
            .documents()
            .iter()
            .map(|document| ContextFile {
                path: document.path.clone(),
                scope: document.scope.label(),
            })
            .collect(),
        skills_dirs: run.skills_dirs.clone(),
        skills: run
            .skills
            .iter()
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect(),
    }
}

/// Drains the session's event stream into the sink until the turn is done,
/// then drains whatever is still queued and hands the sink back.
async fn forward_events<S: EventSink, A: Approver>(
    mut receiver: SessionEventReceiver,
    mut sink: S,
    done: oneshot::Receiver<()>,
    mut approver: A,
    permissions: SessionPermissionHandle,
) -> S {
    tokio::pin!(done);

    loop {
        tokio::select! {
            // Biased so queued events always win over the shutdown signal:
            // the turn finishing must not truncate the stream.
            biased;

            received = receiver.recv() => {
                match received {
                    Ok(event) => {
                        resolve_if_permission(&event, &mut approver, &permissions).await;
                        if !emit_session_event(&mut sink, &event) {
                            return sink;
                        }
                    }
                    // Lagging is recoverable — the receiver keeps working, it
                    // just skipped ahead. Say so and carry on.
                    Err(RecvError::Lagged(dropped)) => {
                        if !emit(&mut sink, lag_notice(dropped)) {
                            return sink;
                        }
                    }
                    // A closed channel means the session is gone; nothing more
                    // can arrive, so stop without waiting for the signal.
                    Err(RecvError::Closed) => return sink,
                }
            }
            _ = &mut done => {
                drain(&mut receiver, &mut sink, &mut approver, &permissions).await;
                return sink;
            }
        }
    }
}

/// Empties whatever the broadcast channel still holds.
async fn drain<S: EventSink, A: Approver>(
    receiver: &mut SessionEventReceiver,
    sink: &mut S,
    approver: &mut A,
    permissions: &SessionPermissionHandle,
) {
    loop {
        match receiver.try_recv() {
            Ok(event) => {
                resolve_if_permission(&event, approver, permissions).await;
                if !emit_session_event(sink, &event) {
                    return;
                }
            }
            Err(TryRecvError::Lagged(dropped)) => {
                if !emit(sink, lag_notice(dropped)) {
                    return;
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => return,
        }
    }
}

/// Answers a pending permission request.
///
/// The turn is blocked inside mentra waiting for this, so failing to resolve
/// would hang the run — which is what happened before lan answered at all.
async fn resolve_if_permission<A: Approver>(
    event: &SessionEvent,
    approver: &mut A,
    permissions: &SessionPermissionHandle,
) {
    let SessionEvent::PermissionRequested {
        request_id,
        tool_call_id,
        tool_name,
        description,
        preview,
    } = event
    else {
        return;
    };

    let decision = approver
        .approve(&ApprovalRequest {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            description: description.clone(),
            input: serde_json::from_str(preview)
                .unwrap_or_else(|_| serde_json::Value::String(preview.clone())),
        })
        .await;

    // A failure here means the request was already resolved or withdrawn;
    // there is nothing useful left to do about it.
    let _ = permissions.resolve_permission(request_id, permission_decision(decision));
}

fn permission_decision(decision: ApprovalDecision) -> PermissionDecision {
    match decision {
        ApprovalDecision::Allow => PermissionDecision::allow(),
        ApprovalDecision::Deny => PermissionDecision::deny(),
        ApprovalDecision::AllowForSession => {
            PermissionDecision::allow_and_remember(PermissionRuleScope::Session)
        }
        ApprovalDecision::DenyForSession => {
            PermissionDecision::deny_and_remember(PermissionRuleScope::Session)
        }
    }
}

/// Maps and emits one session event. Returns `false` when the sink has failed
/// and forwarding should stop.
fn emit_session_event<S: EventSink>(sink: &mut S, event: &SessionEvent) -> bool {
    match Event::from_session_event(event) {
        Some(mapped) => emit(sink, mapped),
        None => true,
    }
}

fn emit<S: EventSink>(sink: &mut S, event: Event) -> bool {
    sink.emit(event).is_ok()
}

/// A dropped-event notice. The alternative — staying quiet — would leave a
/// client with a stream that silently disagrees with what happened.
fn lag_notice(dropped: u64) -> Event {
    Event::Notice {
        severity: NoticeSeverity::Warning,
        message: format!("event stream lagged; {dropped} event(s) dropped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextDocument, ContextScope};

    #[test]
    fn a_lag_notice_says_how_many_were_lost() {
        let Event::Notice { severity, message } = lag_notice(12) else {
            panic!("expected a notice");
        };

        assert_eq!(severity, NoticeSeverity::Warning);
        assert!(message.contains("12"));
    }

    #[test]
    fn the_header_lists_context_files_weakest_first() {
        let context = WorkspaceContext::from_documents(vec![
            ContextDocument {
                path: PathBuf::from("/AGENTS.md"),
                scope: ContextScope::Ancestor { depth: 2 },
                content: "outer".to_string(),
            },
            ContextDocument {
                path: PathBuf::from("/repo/AGENTS.md"),
                scope: ContextScope::Workspace,
                content: "inner".to_string(),
            },
        ]);

        let files: Vec<ContextFile> = context
            .documents()
            .iter()
            .map(|document| ContextFile {
                path: document.path.clone(),
                scope: document.scope.label(),
            })
            .collect();

        assert_eq!(files[0].scope, "ancestor:2");
        assert_eq!(files[1].scope, "workspace");
    }
}
