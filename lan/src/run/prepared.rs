//! Driving a session that is already built.
//!
//! Splitting this from [`run`](super::run) gives two things. A host that
//! already owns a mentra [`Runtime`](mentra::Runtime) — with its own provider,
//! store, or custom tools — can still use lan's context discovery and event
//! stream instead of reimplementing them. And lan's own tests can drive the
//! whole pipeline against a scripted runtime, so the event contract is checked
//! without a network call.

use std::path::PathBuf;

use mentra::{ContentBlock, Session, SessionEvent, SessionEventReceiver};
use tokio::sync::{
    broadcast::error::{RecvError, TryRecvError},
    oneshot,
};

use super::{EventSink, RunError, RunReport};
use crate::{
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

    /// Sends the prompt and streams the run into `sink`.
    ///
    /// The stream always opens with [`Event::RunStarted`] and always closes
    /// with [`Event::RunFinished`], including when the turn fails: by then the
    /// stream has content a client needs to be able to finish reading.
    pub async fn execute<S: EventSink>(self, sink: S) -> Result<RunReport<S>, RunError> {
        let Self { mut session, run } = self;

        let session_id = session.id().to_string();
        let receiver = session.subscribe();

        let mut sink = sink;
        sink.emit(header_for(&session_id, &run))?;

        let (done_tx, done_rx) = oneshot::channel();
        let forwarder = tokio::spawn(forward_events(receiver, sink, done_rx));

        let turn = session
            .append_turn(vec![ContentBlock::text(run.prompt.clone())])
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
            model: run.model,
            provider: run.provider,
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
async fn forward_events<S: EventSink>(
    mut receiver: SessionEventReceiver,
    mut sink: S,
    done: oneshot::Receiver<()>,
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
                drain(&mut receiver, &mut sink);
                return sink;
            }
        }
    }
}

/// Empties whatever the broadcast channel still holds.
fn drain<S: EventSink>(receiver: &mut SessionEventReceiver, sink: &mut S) {
    loop {
        match receiver.try_recv() {
            Ok(event) => {
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
