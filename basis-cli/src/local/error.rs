//! Lifecycle command errors and the structured timeout payloads.
//!
//! A wait timeout is a bounded observation, not a failed task, and therefore
//! exits with 3 while carrying the durable retry handle. The daemon used to
//! send timeout prose the client re-parsed; the client now mints the
//! structured payloads directly — same shapes, no parser.

use std::{process::ExitCode, time::Duration};

use basis_tasks::probe_state;
use serde_json::Value;

use crate::exit::{EXIT_BOUNDED, EXIT_FAILED, EXIT_USAGE};

/// An error returned by a local lifecycle command.
#[derive(Debug)]
pub(crate) struct ClientError {
    message: String,
    payload: Option<Value>,
    code: u8,
}

impl ClientError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            payload: None,
            code: EXIT_FAILED,
        }
    }

    /// A command line basis will not run whatever the world does.
    ///
    /// Exit 2 is clap's, and the distinction it draws is worth keeping by
    /// hand: a task that is *running* is a state, and the same command works
    /// once it settles; a handle from another workspace, or a template nobody
    /// wrote, is an argument, and no amount of waiting makes it right.
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            payload: None,
            code: EXIT_USAGE,
        }
    }

    /// The one follow-up this error admits, for the `next:` line and for the
    /// `--json` payload that carries the same fact.
    #[must_use]
    pub(crate) fn pointing_at(self, next: impl Into<String>) -> Self {
        let mut payload = self
            .payload
            .unwrap_or_else(|| serde_json::json!({"error": self.message}));
        payload["next"] = Value::String(next.into());
        Self {
            payload: Some(payload),
            ..self
        }
    }

    fn timeout(message: impl Into<String>, payload: Value) -> Self {
        Self {
            message: message.into(),
            payload: Some(payload),
            code: EXIT_BOUNDED,
        }
    }

    /// Render the error using the command's requested output mode.
    pub(crate) fn render(self, structured: bool, command: &str) -> ExitCode {
        if structured {
            println!("{}", self.json_payload(command));
        } else {
            eprintln!("basis: {}", self.message);
            if let Some(next) = self.next_action() {
                eprintln!("next: use `{next}`");
            } else {
                eprintln!("next: retry with `{command}` or inspect `basis --help`");
            }
        }
        ExitCode::from(self.code)
    }

    fn next_action(&self) -> Option<String> {
        self.payload
            .as_ref()
            .and_then(|payload| payload["next"].as_str())
            .map(str::to_string)
    }

    fn json_payload(&self, command: &str) -> Value {
        let mut payload = self
            .payload
            .clone()
            .unwrap_or_else(|| serde_json::json!({"error": self.message}));
        let object = payload
            .as_object_mut()
            .expect("client error payload must be a JSON object");
        object
            .entry("error".to_string())
            .or_insert_with(|| Value::String(self.message.clone()));
        object
            .entry("code".to_string())
            .or_insert_with(|| Value::String("failed".to_string()));
        object
            .entry("next".to_string())
            .or_insert_with(|| Value::String(command.to_string()));
        payload
    }
}

impl From<String> for ClientError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ClientError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// `basis-tasks`'s errors carry no exit code or hint of their own — that
/// mapping is ADR-0015's, and it lives here. Most become an ordinary
/// [`ClientError`] by their message alone; one fact does cross the boundary,
/// because it changes the code: an invalid reference (a handle from another
/// workspace, a malformed one) is an argument no amount of waiting fixes, the
/// same distinction [`ClientError::usage`] exists for. A call site that knows
/// more still (a timeout) builds its own instead of routing through this.
impl From<basis_tasks::Error> for ClientError {
    fn from(error: basis_tasks::Error) -> Self {
        if error.is_invalid_reference() {
            Self::usage(error.to_string())
        } else {
            Self::new(error.to_string())
        }
    }
}

pub(crate) fn wait_timeout(task: &str, timeout: Duration, attached: bool) -> ClientError {
    let state = probe_state(attached);
    let message = format!(
        "wait for {task} timed out after {}; the task is still {state}",
        human_duration(timeout)
    );
    let payload = serde_json::json!({
        "error": message,
        "code": "timeout",
        "timed_out": true,
        "task": task,
        "state": state,
        "next": format!("basis wait {task}"),
    });
    ClientError::timeout(message, payload)
}

pub(crate) fn message_timeout(task: &str, message_id: &str, timeout: Duration) -> ClientError {
    let message = format!(
        "message {message_id} on {task} timed out after {}; retry with `basis wait {task} --message {message_id}` or inspect `basis inbox {task}`",
        human_duration(timeout)
    );
    let payload = serde_json::json!({
        "error": message,
        "code": "timeout",
        "timed_out": true,
        "task": task,
        "message": message_id,
        "state": "waiting",
        "next": format!("basis wait {task} --message {message_id}"),
    });
    ClientError::timeout(message, payload)
}

pub(crate) fn watch_timeout(task: &str, attached: bool) -> ClientError {
    let state = probe_state(attached);
    let message = format!("watch for {task} timed out; the task is still {state}");
    let payload = serde_json::json!({
        "error": message,
        "code": "timeout",
        "timed_out": true,
        "task": task,
        "state": state,
        "next": format!("basis watch {task}"),
    });
    ClientError::timeout(message, payload)
}

pub(crate) fn human_duration(duration: Duration) -> String {
    if duration.as_secs().is_multiple_of(60) {
        format!("{}m", duration.as_secs() / 60)
    } else {
        format!("{}s", duration.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_timeout_keeps_the_durable_retry_handle() {
        let error = message_timeout("root/task", "msg-7", Duration::from_secs(1));

        assert_eq!(error.code, EXIT_BOUNDED);
        let payload = error.json_payload("basis ask <ID> <MESSAGE>");
        assert_eq!(payload["code"], "timeout");
        assert_eq!(payload["timed_out"], true);
        assert_eq!(payload["task"], "root/task");
        assert_eq!(payload["message"], "msg-7");
        assert_eq!(payload["state"], "waiting");
        assert_eq!(payload["next"], "basis wait root/task --message msg-7");
    }

    #[test]
    fn task_timeout_is_bounded_without_fabricating_a_message_id() {
        let error = wait_timeout("root/task", Duration::from_secs(30 * 60), true);

        assert_eq!(error.code, EXIT_BOUNDED);
        let payload = error.json_payload("basis wait <ID>");
        assert_eq!(payload["task"], "root/task");
        assert_eq!(payload["state"], "running");
        assert!(payload.get("message").is_none());
        assert_eq!(payload["next"], "basis wait root/task");
        assert!(
            payload["error"]
                .as_str()
                .unwrap()
                .contains("timed out after 30m"),
        );
    }

    #[test]
    fn an_unattached_task_times_out_as_resumable() {
        let error = wait_timeout("root/task", Duration::from_secs(45), false);
        let payload = error.json_payload("basis wait <ID>");
        assert_eq!(payload["state"], "resumable");
        assert!(
            payload["error"].as_str().unwrap().contains("resumable"),
            "{payload}"
        );
    }

    #[test]
    fn ordinary_errors_keep_failed_exit_and_structured_details() {
        let error = ClientError::new("task root/task does not exist");

        assert_eq!(error.code, EXIT_FAILED);
        let payload = error.json_payload("basis wait <ID>");
        assert_eq!(payload["error"], "task root/task does not exist");
        assert_eq!(payload["code"], "failed");
        assert_eq!(payload["next"], "basis wait <ID>");
    }
}
