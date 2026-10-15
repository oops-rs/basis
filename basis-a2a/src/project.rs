//! A task, seen as an A2A `Task`.
//!
//! Whoever runs a task, where it stands is a [`Standing`], and a settled one
//! an [`Outcome`]; everything a caller is shown is projected from those, so
//! both backends are reported alike. A completed task's answer is the parts
//! of its one `result` artifact.
//!
//! For a `basis-tasks` task the mapping is ADR-0028 §3's table and nothing
//! more. basis-tasks publishes
//! two facts about a task, and the A2A state is read straight off them:
//! whether `terminal.json` exists (and what it says), and whether anyone
//! holds the attach lock.
//!
//! | basis-tasks | A2A |
//! | --- | --- |
//! | `Succeeded { result }` | `COMPLETED`, the result as one text part |
//! | `Failed { error }` | `FAILED`, the error as the status message |
//! | `Cancelled` | `CANCELED` |
//! | a terminal state this build does not know | `FAILED`, saying so |
//! | no terminal, a question open to the caller | `INPUT_REQUIRED`, the question as the status message |
//! | no terminal, attached | `WORKING` |
//! | no terminal, not attached | `SUBMITTED` |
//!
//! A hosted task's [`HostedStanding`] maps one to one, its
//! `InputRequired { parts }` to `INPUT_REQUIRED` with those parts as the
//! agent's status message (ADR-0034).
//!
//! The question is the one fact here that is not on disk: it lives in the
//! driving process (see `service::ask`), which overlays it with
//! [`Standing::asking`].
//!
//! The final answer is an artifact, never a message: the spec reserves
//! messages for the exchange, and a caller reading only artifacts must still
//! get the whole result.

use a2a::{
    A2AError, Artifact, Message, Part, Role, Task, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use basis_tasks::{TaskHandle, Tasks, Terminal, TerminalRecord};
use chrono::{DateTime, Utc};

use crate::{
    host::HostedStanding,
    index::Record,
    service::{APPROVAL_EXTENSION, Asked, HOPS_EXTENSION},
};

/// The id of the one artifact a completed task carries.
pub(crate) const RESULT_ARTIFACT: &str = "result";

/// What a caller asked to see of a task.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View {
    /// `None`: all of it. `Some(0)`: none.
    pub(crate) history_length: Option<usize>,
    pub(crate) include_artifacts: bool,
}

impl View {
    /// `GetTask` and `SendMessage` show artifacts; only `ListTasks` hides
    /// them by default.
    pub(crate) fn full(history_length: Option<usize>) -> Self {
        Self {
            history_length,
            include_artifacts: true,
        }
    }
}

/// `historyLength` as the spec defines it: unset is no limit, zero omits the
/// history, a positive value keeps that many of the most recent messages.
pub(crate) fn history_length(requested: Option<i32>) -> Result<Option<usize>, A2AError> {
    match requested {
        None => Ok(None),
        Some(length) => usize::try_from(length)
            .map(Some)
            .map_err(|_| A2AError::invalid_params("historyLength must not be negative")),
    }
}

/// Where a task stands, whoever runs it.
#[derive(Debug, Clone)]
pub(crate) enum Standing {
    Submitted,
    Working,
    /// Parked on a question to the caller (ADR-0030 §7).
    Asking(Asked),
    /// A hosted task paused on a question of its host's to the caller
    /// (ADR-0034): these parts are the question.
    HostAsking(Vec<Part>),
    Settled(Outcome),
}

/// How a settled task ended, in the terms A2A reports it in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    /// The answer, as the parts of the one `result` artifact. `truncated`
    /// when the task layer cut it to its bound.
    Completed {
        parts: Vec<Part>,
        truncated: bool,
    },
    Failed {
        error: String,
    },
    Canceled,
    /// A terminal state this build does not know: never vouched for as
    /// success.
    Unknown,
}

impl From<TerminalRecord> for Outcome {
    /// A `basis-tasks` terminal: its result is one text part.
    fn from(terminal: TerminalRecord) -> Self {
        match terminal.terminal {
            Some(Terminal::Succeeded { result }) => Self::Completed {
                parts: vec![Part::text(result)],
                truncated: terminal.result_truncated,
            },
            Some(Terminal::Failed { error }) => Self::Failed { error },
            Some(Terminal::Cancelled) => Self::Canceled,
            // `Terminal` is non-exhaustive; a state a newer basis adds is
            // not one this build can vouch for as success.
            _ => Self::Unknown,
        }
    }
}

impl From<HostedStanding> for Standing {
    /// What a host says of its task: a completion's parts are the answer,
    /// exactly as given.
    fn from(standing: HostedStanding) -> Self {
        match standing {
            HostedStanding::Submitted => Self::Submitted,
            HostedStanding::Working => Self::Working,
            HostedStanding::Completed { parts } => Self::Settled(Outcome::Completed {
                parts,
                truncated: false,
            }),
            HostedStanding::Failed { error } => Self::Settled(Outcome::Failed { error }),
            HostedStanding::Canceled => Self::Settled(Outcome::Canceled),
            HostedStanding::InputRequired { parts } => Self::HostAsking(parts),
        }
    }
}

impl Standing {
    /// Where a `basis-tasks` task stands, read off its directory without
    /// attaching.
    pub(crate) fn read(tasks: &Tasks, record: &Record) -> Result<Self, A2AError> {
        let handle = handle_of(record)?;
        if let Some(terminal) = tasks.terminal(handle).map_err(internal)? {
            let mut outcome = Outcome::from(terminal);
            // A completed answer carries the files its task attached, after
            // its text (ADR-0041 §7).
            if let Outcome::Completed { parts, .. } = &mut outcome {
                let files = tasks.answer_files(handle).map_err(internal)?;
                parts.extend(files.into_iter().map(|file| {
                    Part::raw(file.data)
                        .with_filename(file.name)
                        .with_media_type(file.media_type)
                }));
            }
            return Ok(Self::Settled(outcome));
        }
        Ok(if tasks.is_attached(handle).map_err(internal)? {
            Self::Working
        } else {
            Self::Submitted
        })
    }

    /// This standing, with `asked` open: a question a task that has not
    /// settled is parked on.
    pub(crate) fn asking(self, asked: Option<Asked>) -> Self {
        match (self, asked) {
            (Self::Settled(terminal), _) => Self::Settled(terminal),
            (_, Some(asked)) => Self::Asking(asked),
            (standing, None) => standing,
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Settled(_))
    }

    /// Terminal, or waiting on the caller: where a blocking send answers and
    /// a stream closes (spec §3.1.1, §11.7).
    pub(crate) fn is_interrupted_or_terminal(&self) -> bool {
        matches!(
            self,
            Self::Settled(_) | Self::Asking(_) | Self::HostAsking(_)
        )
    }

    pub(crate) fn state(&self) -> TaskState {
        match self {
            Self::Submitted => TaskState::Submitted,
            Self::Working => TaskState::Working,
            Self::Asking(_) | Self::HostAsking(_) => TaskState::InputRequired,
            Self::Settled(Outcome::Completed { .. }) => TaskState::Completed,
            Self::Settled(Outcome::Canceled) => TaskState::Canceled,
            Self::Settled(Outcome::Failed { .. } | Outcome::Unknown) => TaskState::Failed,
        }
    }
}

/// The task as A2A sees it.
pub(crate) fn task(record: &Record, standing: &Standing, view: View) -> Task {
    let status = status(record, standing);
    let artifacts = view
        .include_artifacts
        .then(|| result_artifact(standing))
        .flatten()
        .map(|artifact| vec![artifact]);
    Task {
        id: record.id.clone(),
        context_id: record.context_id.clone(),
        status,
        artifacts,
        history: history(record, view.history_length),
        metadata: hops_metadata(record),
    }
}

/// The hop the task was reached at and its effective deadline, under the
/// hops extension — what the server decided, for a caller to see.
fn hops_metadata(record: &Record) -> Option<std::collections::HashMap<String, serde_json::Value>> {
    let deadline = timestamp(record.deadline_ms?)?;
    Some(
        [(
            HOPS_EXTENSION.to_string(),
            serde_json::json!({
                "hop": record.hop,
                "deadline": deadline.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            }),
        )]
        .into_iter()
        .collect(),
    )
}

/// The status update that says where `standing` leaves the task.
pub(crate) fn status_update(record: &Record, standing: &Standing) -> TaskStatusUpdateEvent {
    TaskStatusUpdateEvent {
        task_id: record.id.clone(),
        context_id: record.context_id.clone(),
        status: status(record, standing),
        metadata: None,
    }
}

fn status(record: &Record, standing: &Standing) -> TaskStatus {
    let message = match standing {
        Standing::Settled(Outcome::Failed { error }) => Some(agent_message(record, error.clone())),
        Standing::Settled(Outcome::Unknown) => Some(agent_message(
            record,
            "the task ended in a state this server does not recognize".to_string(),
        )),
        Standing::Asking(asked) => Some(question(record, asked)),
        Standing::HostAsking(parts) => {
            (!parts.is_empty()).then(|| agent_message_parts(record, parts.clone()))
        }
        _ => None,
    };
    TaskStatus {
        state: standing.state(),
        message,
        timestamp: timestamp(record.status_ms()),
    }
}

/// A question as its status message: words a model can act on, and the same
/// facts as a data part under the approval extension (`docs/a2a/approval-v1.md`).
fn question(record: &Record, asked: &Asked) -> Message {
    let text = format!(
        "Approval needed: the agent wants to call `{tool}`.\n{preview}\nReply `approve {id}` or \
         `deny {id}`.",
        tool = asked.tool,
        preview = asked.preview,
        id = asked.call_id,
    );
    let data = Part::data(serde_json::json!({
        APPROVAL_EXTENSION: {
            "callId": asked.call_id,
            "tool": asked.tool,
            "preview": asked.preview,
        }
    }));
    Message {
        extensions: Some(vec![APPROVAL_EXTENSION.to_string()]),
        ..agent_message_parts(record, vec![Part::text(text), data])
    }
}

/// The completed task's answer, as the artifact a caller reads it from. An
/// answer of no parts has no artifact: the wire wants at least one.
pub(crate) fn result_artifact(standing: &Standing) -> Option<Artifact> {
    let Standing::Settled(Outcome::Completed { parts, truncated }) = standing else {
        return None;
    };
    if parts.is_empty() {
        return None;
    }
    Some(Artifact {
        artifact_id: RESULT_ARTIFACT.to_string(),
        name: Some(RESULT_ARTIFACT.to_string()),
        description: truncated.then(|| "truncated by the task layer's result bound".to_string()),
        parts: parts.clone(),
        metadata: None,
        extensions: None,
    })
}

/// The exchange so far: the caller's message, as sent. What the agent
/// answered is the artifact, not history.
fn history(record: &Record, history_length: Option<usize>) -> Option<Vec<Message>> {
    if history_length == Some(0) {
        return None;
    }
    Some(vec![Message {
        message_id: record.message_id.clone(),
        context_id: Some(record.context_id.clone()),
        task_id: Some(record.id.clone()),
        role: Role::User,
        parts: record
            .parts
            .clone()
            .unwrap_or_else(|| vec![Part::text(record.text.clone())]),
        metadata: None,
        extensions: None,
        reference_task_ids: None,
    }])
}

/// A message from the agent about this task — a progress note, or why it
/// failed.
pub(crate) fn agent_message(record: &Record, text: String) -> Message {
    agent_message_parts(record, vec![Part::text(text)])
}

fn agent_message_parts(record: &Record, parts: Vec<Part>) -> Message {
    Message {
        context_id: Some(record.context_id.clone()),
        task_id: Some(record.id.clone()),
        ..Message::new(Role::Agent, parts)
    }
}

pub(crate) fn timestamp(ms: u64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(i64::try_from(ms).ok()?)
}

/// The `basis-tasks` handle `record` names. A record of a hosted task is
/// one a `basis-tasks` server never wrote — its state directory is some
/// host's — and is this server's fault to report, not the caller's.
pub(crate) fn handle_of(record: &Record) -> Result<&TaskHandle, A2AError> {
    record
        .handle()
        .ok_or_else(|| internal(format!("task {} is not a basis-tasks task", record.id)))
}

/// A failure of this server's own, not the caller's: logged in full where
/// the operator can read it, reported to the caller without paths or
/// anything else of the host's.
pub(crate) fn internal(error: impl std::fmt::Display) -> A2AError {
    eprintln!("basis-a2a: {error}");
    A2AError::internal("the server could not complete the request")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> Record {
        let handle = TaskHandle::parse(format!("{}/{}", "0".repeat(16), "a".repeat(32))).unwrap();
        Record::new(
            &handle,
            &crate::config::Principal::anonymous(),
            "ctx".into(),
            "m1".into(),
            "hi".into(),
            1_000,
        )
    }

    fn settled(raw: serde_json::Value) -> Standing {
        Standing::Settled(
            TerminalRecord {
                terminal: serde_json::from_value(raw.clone()).ok(),
                raw,
                stopped_by: None,
                usage: None,
                result_truncated: false,
            }
            .into(),
        )
    }

    #[test]
    fn a_succeeded_task_is_completed_with_its_result_as_the_artifact() {
        let standing = settled(json!({"state": "succeeded", "result": "42"}));
        let task = task(&record(), &standing, View::full(None));
        assert_eq!(task.status.state, TaskState::Completed);
        let artifacts = task.artifacts.expect("a completed task carries its result");
        assert_eq!(artifacts[0].parts[0].as_text(), Some("42"));
        assert!(task.status.message.is_none());
    }

    #[test]
    fn a_failed_task_says_why_in_its_status() {
        let standing = settled(json!({"state": "failed", "error": "boom"}));
        let task = task(&record(), &standing, View::full(None));
        assert_eq!(task.status.state, TaskState::Failed);
        assert_eq!(task.status.message.unwrap().text(), Some("boom"));
        assert!(task.artifacts.is_none());
    }

    #[test]
    fn an_unknown_terminal_state_is_a_failure_not_a_success() {
        let standing = settled(json!({"state": "from_the_future"}));
        assert_eq!(standing.state(), TaskState::Failed);
    }

    #[test]
    fn a_completion_of_several_parts_is_one_artifact_of_them_all() {
        let parts = vec![Part::text("see data"), Part::data(json!({"answer": 42}))];
        let standing = Standing::Settled(Outcome::Completed {
            parts: parts.clone(),
            truncated: false,
        });
        let task = task(&record(), &standing, View::full(None));
        assert_eq!(task.status.state, TaskState::Completed);
        let artifacts = task.artifacts.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].parts, parts);
    }

    #[test]
    fn a_completion_of_no_parts_has_no_artifact() {
        let standing = Standing::Settled(Outcome::Completed {
            parts: Vec::new(),
            truncated: false,
        });
        assert!(result_artifact(&standing).is_none());
        assert_eq!(standing.state(), TaskState::Completed);
    }

    #[test]
    fn cancelled_is_canceled() {
        assert_eq!(
            settled(json!({"state": "cancelled"})).state(),
            TaskState::Canceled
        );
    }

    #[test]
    fn history_length_zero_omits_the_history() {
        let task = task(&record(), &Standing::Working, View::full(Some(0)));
        assert!(task.history.is_none());
        let task = super::task(&record(), &Standing::Working, View::full(None));
        assert_eq!(task.history.unwrap()[0].message_id, "m1");
    }

    #[test]
    fn a_negative_history_length_is_refused() {
        assert!(history_length(Some(-1)).is_err());
        assert_eq!(history_length(Some(3)).unwrap(), Some(3));
    }

    #[test]
    fn listing_without_artifacts_omits_the_field_entirely() {
        let standing = settled(json!({"state": "succeeded", "result": "42"}));
        let view = View {
            history_length: None,
            include_artifacts: false,
        };
        let task = task(&record(), &standing, view);
        assert!(task.artifacts.is_none());
        assert!(
            serde_json::to_value(&task)
                .unwrap()
                .get("artifacts")
                .is_none()
        );
    }
}
