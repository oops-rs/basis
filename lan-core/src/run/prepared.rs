//! Driving a session that is already built.
//!
//! Splitting this from [`run`](super::run) gives two things. A host that
//! already owns a mentra [`Runtime`](mentra::Runtime) — with its own provider,
//! store, or custom tools — can still use lan's context discovery and event
//! stream instead of reimplementing them. And lan's own tests can drive the
//! whole pipeline against a scripted runtime, so the event contract is checked
//! without a network call.

use std::path::PathBuf;

use mentra::{ContentBlock, Session};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::oneshot;

use self::forward::forward_events;
use super::{
    Bound, EventSink, OutputReport, OutputSpec, RunError, RunReport, RunUsage, TurnOptions,
    turn::bounded,
};
use crate::{
    approval::{AllowAll, Approver},
    context::WorkspaceContext,
    event::{ContextFile, EVENT_SCHEMA_VERSION, Event, RunOutcome, SkillSummary, TemplateSummary},
    templates::Template,
};

mod forward;

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
    /// Template directories that exist, most specific first.
    pub templates_dirs: Vec<PathBuf>,
    /// The templates those directories produced, after layering, name-ordered.
    /// Over ACP these become the client's commands, mapped by `lan-acp`.
    pub templates: Vec<Template>,
    /// MCP configuration files in effect, weakest precedence first.
    pub mcp_files: Vec<ContextFile>,
    /// The servers those files produced, after layering. Names only: the
    /// header must not echo a command or a credential.
    pub mcp_servers: Vec<String>,
}

/// A skill available to the run, without its body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// A session and the prompt to send it. Nothing has been sent yet.
pub struct PreparedRun {
    session: Session,
    run: RunContext,
    bounds: TurnOptions,
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
            .field("templates", &self.run.templates.len())
            .field("mcp_servers", &self.run.mcp_servers.len())
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

impl PreparedRun {
    pub fn new(session: Session, run: RunContext) -> Self {
        Self {
            session,
            run,
            bounds: TurnOptions::default(),
        }
    }

    /// Sets what every turn on this run may spend.
    ///
    /// [`prepare`](super::prepare) installs [`RunConfig`](super::RunConfig)'s
    /// bounds here; a host that built its own session says so itself. Only the
    /// limits are read — a cancellation token belongs to one call, not to the
    /// run, and arrives through [`send_with_options`](Self::send_with_options).
    pub fn with_bounds(self, bounds: TurnOptions) -> Self {
        Self { bounds, ..self }
    }

    /// What every turn on this run may spend.
    pub const fn bounds(&self) -> &TurnOptions {
        &self.bounds
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
    /// Consequential calls are approved by [`AllowAll`], the default for a run
    /// that was given no approver of its own;
    /// [`execute_with_approver`](Self::execute_with_approver) is where anything
    /// stricter goes.
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

    /// Sends the configured prompt, streaming into `sink` and putting every
    /// consequential call to `approver`.
    ///
    /// The approver runs on the forwarding task while the turn is blocked
    /// waiting on it, which is what makes an interactive answer possible at
    /// all — and what means an approver must answer rather than defer. One that
    /// cannot answer denies; see [`Approver`].
    pub async fn execute_with_approver<S: EventSink, A: Approver>(
        &mut self,
        sink: S,
        approver: A,
    ) -> Result<RunReport<S>, RunError> {
        self.execute_with_approver_and_options(sink, approver, TurnOptions::default())
            .await
    }

    /// Sends the configured prompt with explicit run options — a cancellation
    /// token, a deadline, a tool budget.
    ///
    /// The one-shot path is bounded by its config but had no way to be
    /// *stopped*: a token belongs to one call, so it cannot travel in a config
    /// that mints many. This is where it arrives, and it is what a host driving
    /// a one-prompt run behind a UI needs, exactly as
    /// [`send_with_options`](Self::send_with_options) serves a conversation.
    pub async fn execute_with_options<S: EventSink>(
        &mut self,
        sink: S,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        self.execute_with_approver_and_options(sink, AllowAll, options)
            .await
    }

    /// Sends the configured prompt with both an approver and explicit options.
    pub async fn execute_with_approver_and_options<S: EventSink, A: Approver>(
        &mut self,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        let prompt = self.run.prompt.clone();
        self.turn(prompt, sink, approver, options).await
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
    ///
    /// A bound left unset here falls back to the run's own
    /// ([`bounds`](Self::bounds)). Attaching a token is a statement about
    /// stopping, not about limits, and reading it as "no deadline after all"
    /// would quietly unbound a run its caller had configured.
    pub async fn send_with_options<S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        self.turn(prompt.into(), sink, approver, options).await
    }

    /// Sends a prompt whose answer must be a value of type `T` rather than
    /// prose.
    ///
    /// ADR-0010's structured output, and the primitive a workflow is built on:
    /// the model is handed one terminal tool whose input *is* the answer, is
    /// required to call it, and `T` is deserialized from what it sent. The
    /// caller writes the schema — see [`OutputSpec`] for why lan derives
    /// nothing.
    ///
    /// The stream is unchanged. Header, forwarded events, permissions put to
    /// the approver, `RunFinished`: a client reading events cannot tell a typed
    /// turn from any other, which is the point — only the return value differs.
    /// The answer travels as the terminal tool's
    /// [`ToolQueued`](Event::ToolQueued) input and
    /// [`ToolCompleted`](Event::ToolCompleted) summary, and
    /// [`RunReport::final_message`] stays `None`, because a typed turn's
    /// committed final message is that tool result — putting a JSON payload in
    /// a field named for the assistant's prose would have every client render
    /// it as speech. Prose the model wrote alongside the call, usually none,
    /// arrives as [`Event::AssistantMessage`].
    ///
    /// Where a plain turn reports its failure on the stream and still returns
    /// `Ok`, this returns `Err`: a typed turn without a value has nothing to
    /// hand back.
    ///
    /// - [`RunError::OutputMismatch`] — an answer arrived that `T` did not
    ///   accept. mentra commits the exchange before lan reads it, so the
    ///   transcript keeps the attempt and a follow-up turn can say what was
    ///   wrong with it.
    /// - [`RunError::Runtime`] — the turn failed, *or* it finished without ever
    ///   calling the terminal tool. mentra reports both as
    ///   `MalformedProviderEvent` and lan will not read error prose to tell
    ///   them apart.
    ///
    /// The stream is complete and closed in every one of those cases, so a sink
    /// with somewhere to put events — a file, a channel — has the whole run.
    /// Only the sink *value* is lost, because it comes back inside the report.
    ///
    /// ```no_run
    /// use serde::Deserialize;
    /// use serde_json::json;
    ///
    /// #[derive(Deserialize)]
    /// struct Review {
    ///     verdict: String,
    /// }
    ///
    /// # async fn example(run: &mut lan_core::PreparedRun) -> Result<(), lan_core::RunError> {
    /// let spec = lan_core::OutputSpec::new(
    ///     "submit_review",
    ///     "call this once you have read every changed file",
    ///     json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "verdict": { "type": "string", "description": "ship or hold" }
    ///         },
    ///         "required": ["verdict"]
    ///     }),
    /// );
    ///
    /// let output = run
    ///     .output::<Review, _, _>(
    ///         "review the diff on this branch",
    ///         spec,
    ///         lan_core::NullSink,
    ///         lan_core::AllowAll,
    ///     )
    ///     .await?;
    ///
    /// // A value, not a paragraph to parse — and what it cost, for a caller
    /// // adding runs up against a budget.
    /// println!("{} ({} tokens)", output.value.verdict, output.report.usage.total_tokens());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn output<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        spec: OutputSpec,
        sink: S,
        approver: A,
    ) -> Result<OutputReport<T, S>, RunError> {
        self.typed_turn(prompt.into(), spec, sink, approver, TurnOptions::default())
            .await
    }

    /// A typed turn with explicit run options.
    ///
    /// Same relationship to [`output`](Self::output) as
    /// [`send_with_options`](Self::send_with_options) has to
    /// [`send`](Self::send): a typed turn is cancellable and boundable like any
    /// other, and a fan-out that gives each of its runs a deadline should not
    /// have to give up types to get one.
    pub async fn output_with_options<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        spec: OutputSpec,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<OutputReport<T, S>, RunError> {
        self.typed_turn(prompt.into(), spec, sink, approver, options)
            .await
    }

    /// One turn, start to finish: header, forwarded events, outcome.
    async fn turn<S: EventSink, A: Approver>(
        &mut self,
        prompt: String,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        let turn = self.begin(&prompt, sink, approver)?;

        let result = self
            .session
            .append_turn_with_options(
                vec![ContentBlock::text(prompt)],
                bounded(options, &self.bounds).into_run_options(),
            )
            .await;

        let ended = match &result {
            Ok(message) => Ended::Answered(Some(message.text())),
            Err(error) => Ended::Failed(error),
        };
        self.finish(turn, ended).await
    }

    /// One typed turn. Identical to [`turn`](Self::turn) but for the one call
    /// in the middle — which is the whole reason both are written this way,
    /// since a second copy of the header-and-forwarding dance is a second thing
    /// to keep in step with the stream contract.
    ///
    /// mentra is asked for a [`Value`] rather than for `T` directly, and lan
    /// deserializes. That costs nothing (the payload is already JSON) and buys
    /// the error distinction: a value that does not fit `T` is lan's own
    /// finding, reported as [`RunError::OutputMismatch`], instead of arriving
    /// as one more `MalformedProviderEvent` indistinguishable from a provider
    /// that misbehaved.
    async fn typed_turn<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: String,
        spec: OutputSpec,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<OutputReport<T, S>, RunError> {
        let turn = self.begin(&prompt, sink, approver)?;

        let result = self
            .session
            .append_turn_to_output::<Value>(
                vec![ContentBlock::text(prompt)],
                bounded(options, &self.bounds).into_run_options(),
                spec.into_terminal_spec(),
            )
            .await;

        let typed = match result {
            Ok(output) => Ok(serde_json::from_value::<T>(output.value)),
            Err(error) => Err(error),
        };
        let ended = match &typed {
            Ok(Ok(_)) => Ended::Answered(None),
            Ok(Err(mismatch)) => Ended::Mismatched(mismatch),
            Err(error) => Ended::Failed(error),
        };

        let report = self.finish(turn, ended).await?;

        match typed {
            Ok(Ok(value)) => Ok(OutputReport { value, report }),
            Ok(Err(mismatch)) => Err(RunError::OutputMismatch(mismatch)),
            Err(error) => Err(RunError::Runtime(error)),
        }
    }

    /// Opens a turn: checks the prompt, emits the header, starts forwarding.
    ///
    /// Split from [`finish`](Self::finish) so that everything between them is
    /// exactly one call to mentra. Every entry point above shares this pair, so
    /// no turn can announce itself differently from the others — the same split
    /// mentra makes internally, and for the same reason.
    fn begin<S: EventSink, A: Approver>(
        &self,
        prompt: &str,
        sink: S,
        approver: A,
    ) -> Result<Turn<S>, RunError> {
        if prompt.trim().is_empty() {
            return Err(RunError::EmptyPrompt);
        }

        let permissions = self.session.permission_handle();
        let session_id = self.session.id().to_string();
        let receiver = self.session.subscribe();

        let mut sink = sink;
        sink.emit(header_for(&session_id, &self.run))?;

        let (done, done_rx) = oneshot::channel();
        let forwarder = tokio::spawn(forward_events(
            receiver,
            sink,
            done_rx,
            approver,
            permissions,
        ));

        Ok(Turn {
            session_id,
            done,
            forwarder,
        })
    }

    /// Closes a turn opened by [`begin`](Self::begin): stops forwarding, states
    /// the outcome on the stream, and reports.
    ///
    /// Classifying `ended` here rather than at each call site is what keeps one
    /// answer to "did this run succeed" — a second site would be a second
    /// opinion.
    async fn finish<S: EventSink>(
        &self,
        turn: Turn<S>,
        ended: Ended<'_>,
    ) -> Result<RunReport<S>, RunError> {
        let Turn {
            session_id,
            done,
            forwarder,
        } = turn;

        // The forwarder stops on this signal rather than on the channel
        // closing, so a sender clone held elsewhere in the runtime cannot
        // strand the task.
        let _ = done.send(());
        let (mut sink, usage) = forwarder.await?;

        let (final_message, outcome, stopped_by) = match ended {
            Ended::Answered(final_message) => (final_message, RunOutcome::Ok, None),
            Ended::Failed(error) => (
                None,
                RunOutcome::Error {
                    message: error.to_string(),
                },
                tripped_bound(error),
            ),
            // The turn itself completed, so mentra kept the exchange — but the
            // caller asked for a shape and did not get one, and a stream that
            // said "ok" while its caller received an error would be describing
            // a different run from the one that happened.
            Ended::Mismatched(mismatch) => (
                None,
                RunOutcome::Error {
                    message: format!("output did not match the requested type: {mismatch}"),
                },
                None,
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
            stopped_by,
            usage,
            sink,
        })
    }
}

/// A turn in flight: the forwarding task, and the signal that ends it.
struct Turn<S> {
    session_id: String,
    done: oneshot::Sender<()>,
    forwarder: tokio::task::JoinHandle<(S, RunUsage)>,
}

/// How a turn ended, in the terms the stream reports.
///
/// Borrowed rather than owned so a caller can hand the failure over for
/// classification and still return it: the error a typed turn reports to its
/// caller and the message the stream carries have to be the same error.
enum Ended<'a> {
    /// mentra completed the turn. Carries the assistant's final prose, when the
    /// turn had any — a typed turn's answer is not prose and is not put here.
    Answered(Option<String>),
    /// mentra failed the turn.
    Failed(&'a mentra::error::RuntimeError),
    /// The turn completed, but its answer did not fit the requested type.
    Mismatched(&'a serde_json::Error),
}

/// Which of the run's own bounds ended the turn, if one did.
///
/// Classified here, from the typed error, rather than left for someone to
/// recognize in a message later — a caller matching on prose would break the
/// first time mentra reworded one.
fn tripped_bound(error: &mentra::error::RuntimeError) -> Option<Bound> {
    match error {
        mentra::error::RuntimeError::DeadlineExceeded => Some(Bound::Deadline),
        mentra::error::RuntimeError::ToolBudgetExceeded(_) => Some(Bound::ToolBudget),
        // Everything else is a failure of the work, not of the allowance: a
        // provider error, a cancelled turn, an unreadable transcript.
        _ => None,
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
        templates_dirs: run.templates_dirs.clone(),
        templates: run
            .templates
            .iter()
            .map(|template| TemplateSummary {
                name: template.name.clone(),
                description: template.description.clone(),
                argument_hint: template.argument_hint.clone(),
            })
            .collect(),
        mcp_files: run.mcp_files.clone(),
        mcp_servers: run.mcp_servers.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextDocument, ContextScope};

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

    #[test]
    fn a_tripped_bound_is_told_apart_from_a_failed_run() {
        use mentra::error::RuntimeError;

        assert_eq!(
            tripped_bound(&RuntimeError::DeadlineExceeded),
            Some(Bound::Deadline)
        );
        assert_eq!(
            tripped_bound(&RuntimeError::ToolBudgetExceeded(40)),
            Some(Bound::ToolBudget)
        );

        // A run the provider refused is a failure, and a shell script that
        // retried it as if it had merely run out of time would retry forever.
        assert_eq!(tripped_bound(&RuntimeError::EmptyAssistantResponse), None);
        assert_eq!(tripped_bound(&RuntimeError::Cancelled), None);
    }
}
