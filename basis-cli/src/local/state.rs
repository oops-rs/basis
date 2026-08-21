//! Durable per-agent state: metadata, terminal records, and their bounds.
//!
//! Three files carry an agent's task state, each atomic-replace JSON:
//! `meta.json` (bookkeeping and the recorded spawn request — never
//! conversation content, which is mentra's store's alone), `inbox.json`
//! (messages, see [`super::inbox`]), and `terminal.json` — written as the
//! executor's **last** act, whose existence is the completion signal. An agent
//! is resumable iff its terminal record does not exist; every crash before
//! that write resolves toward resumable.

use std::{
    io,
    time::{SystemTime, UNIX_EPOCH},
};

use basis::RunUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::data_dir::{AgentPaths, write_private_atomic};

pub(crate) const MAX_MESSAGES: usize = 16;
pub(crate) const MAX_MESSAGE: usize = 256 * 1024;
pub(crate) const MAX_PROMPT: usize = 256 * 1024;
pub(crate) const MAX_EVENT_BYTES: usize = 32 * 1024;
pub(crate) const MAX_RESULT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TASKS: usize = 1024;
/// Byte cap on one agent's `events.jsonl`, standing in for the daemon-era
/// journal cap: on overflow a final notice line is appended and recording
/// stops — a run is never failed for observability.
pub(crate) const MAX_EVENTS_BYTES: u64 = 32 * 1024 * 1024;

/// The recorded spawn request, so a later attach can execute what spawn
/// accepted. Credentials are intentionally absent: the attaching process owns
/// its own environment, exactly as any other host does.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct RunOptions {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub no_shell: bool,
    /// The host's say over the system prompt, as the two flags spell it:
    /// `system_prompt` replaces the workspace's context, `append_system_prompt`
    /// follows it. Flattened rather than held as a `basis::SystemPrompt`
    /// because that is what this record already does with an effort — and
    /// because clap has refused both at once by the time either is written, so
    /// the pair cannot disagree. Defaulted, so a `meta.json` written before
    /// these existed still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    pub effort: Option<String>,
    pub approve: String,
    pub deadline_ms: Option<u64>,
    pub tool_budget: Option<usize>,
    pub token_budget: Option<u64>,
}

/// A task whose own work has finished, but whose attached children may still
/// lack terminal records. Recorded in `meta.json` before the settle pass, so a
/// kill between a child's terminal and the parent's leaves the parent
/// resumable with the model work already done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum PendingTerminal {
    Succeeded { result: String },
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessageState {
    Pending,
    InFlight,
    Delivered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MessageReply {
    pub result: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub result_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MessageRecord {
    pub id: String,
    pub body: String,
    pub state: MessageState,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<MessageReply>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct EventRecord {
    pub seq: u64,
    pub event: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TaskMeta {
    pub id: String,
    pub parent: Option<String>,
    pub detached: bool,
    pub workspace: String,
    /// The mentra resume key; empty until the first attach prepares the run.
    pub agent_id: String,
    /// The conversation this task was told to pick up, when `spawn` was given
    /// `--continue` or `--session`. The first attach resumes it instead of
    /// minting one, which is what lets a new handle carry an old dialogue —
    /// and why continuing a settled task is a new task rather than a message
    /// to a closed inbox (ADR-0019).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<String>,
    /// How many assistant turns the conversation already held when this task
    /// first attached to it.
    ///
    /// Zero for a task that opened its own conversation. Nonzero only for a
    /// continued one, where the previous answers are on the transcript from
    /// the very first turn — and where the resume recovery in `attach` would
    /// otherwise read one of them as this task's own answer and settle
    /// `succeeded` without ever asking its prompt.
    #[serde(default)]
    pub answered_before: usize,
    pub prompt: String,
    pub options: RunOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_terminal: Option<PendingTerminal>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub result_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<String>,
    /// What every turn this task has driven reported spending, summed.
    ///
    /// Kept in `meta.json` rather than recomputed from the event journal
    /// because a task outlives the process that drove it: two attaches, each
    /// running turns, both add to one tally, and the journal may have been
    /// capped ([`MAX_EVENTS_BYTES`]) long before anyone asks.
    #[serde(default)]
    pub usage: RunUsage,
    pub deadline_at_ms: Option<u64>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

impl TaskMeta {
    pub(crate) fn new(
        id: String,
        parent: Option<String>,
        detached: bool,
        workspace: String,
        prompt: String,
        options: RunOptions,
        deadline_at_ms: Option<u64>,
    ) -> Self {
        let now = now_ms();
        Self {
            id,
            parent,
            detached,
            workspace,
            agent_id: String::new(),
            continues: None,
            answered_before: 0,
            prompt,
            options,
            pending_terminal: None,
            result_truncated: false,
            stopped_by: None,
            usage: RunUsage::default(),
            deadline_at_ms,
            created_ms: now,
            updated_ms: now,
        }
    }

    /// Records the conversation this task continues, when it continues one.
    ///
    /// A field rather than a pre-set `agent_id`, because the two mean
    /// different things to the executor: `agent_id` says *this task has
    /// attached before*, and taking a continued conversation for a resumed
    /// one is how a task settles on someone else's answer.
    #[must_use]
    pub(crate) fn continuing(self, agent_id: Option<String>) -> Self {
        Self {
            continues: agent_id,
            ..self
        }
    }

    pub(crate) fn deadline_passed(&self) -> bool {
        self.deadline_at_ms
            .is_some_and(|deadline| deadline <= now_ms())
    }

    /// The terminal payload the recorded completion earns: exactly the shape
    /// the daemon's journal produced, minted once into `terminal.json`.
    pub(crate) fn terminal_payload(&self) -> Option<Value> {
        let payload = match self.pending_terminal.as_ref()? {
            PendingTerminal::Succeeded { result } => {
                let mut terminal = serde_json::json!({"state": "succeeded", "result": result});
                if self.result_truncated {
                    terminal["result_truncated"] = Value::Bool(true);
                }
                with_stopped_by(terminal, self.stopped_by.as_deref())
            }
            PendingTerminal::Failed { error } => with_stopped_by(
                serde_json::json!({"state": "failed", "error": error}),
                self.stopped_by.as_deref(),
            ),
            PendingTerminal::Cancelled => serde_json::json!({"state": "cancelled"}),
        };
        Some(with_usage(payload, self.usage))
    }
}

fn with_stopped_by(mut payload: Value, stopped_by: Option<&str>) -> Value {
    if let Some(stopped_by) = stopped_by {
        payload["stopped_by"] = Value::String(stopped_by.to_string());
    }
    payload
}

/// Adds what the task spent, when it spent anything.
///
/// A record that names no usage is a task whose turns reported none — a
/// cancellation that never reached the model is the ordinary case — and a
/// `usage` object full of zeros would claim a measurement nobody made. The
/// same rule the finish line follows (`Event::RunFinished::usage`), so the
/// stream and the record cannot disagree.
fn with_usage(mut payload: Value, usage: RunUsage) -> Value {
    if usage != RunUsage::default() {
        payload["usage"] = serde_json::json!(usage);
    }
    payload
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn load_meta(paths: &AgentPaths) -> Result<TaskMeta, String> {
    let bytes = std::fs::read(paths.meta())
        .map_err(|error| format!("read task metadata for {}: {error}", paths.dir().display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "decode task metadata for {}: {error}",
            paths.dir().display()
        )
    })
}

pub(crate) fn save_meta(paths: &AgentPaths, meta: &TaskMeta) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(meta).map_err(|error| format!("encode task metadata: {error}"))?;
    write_private_atomic(&paths.meta(), &bytes)
        .map_err(|error| format!("persist task metadata: {error}"))
}

/// Reads the terminal record. `None` is the resumable state.
pub(crate) fn read_terminal(paths: &AgentPaths) -> Result<Option<Value>, String> {
    match std::fs::read(paths.terminal()) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("decode terminal record: {error}")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read terminal record: {error}")),
    }
}

pub(crate) fn write_terminal(paths: &AgentPaths, payload: &Value) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(payload).map_err(|error| format!("encode terminal record: {error}"))?;
    write_private_atomic(&paths.terminal(), &bytes)
        .map_err(|error| format!("persist terminal record: {error}"))
}

pub(crate) fn cancel_requested(paths: &AgentPaths) -> bool {
    paths.cancel_marker().exists()
}

/// Writes the cancel marker. Existence is the signal; the content is
/// diagnostic only.
pub(crate) fn request_cancel(paths: &AgentPaths, by: Option<&str>) -> Result<(), String> {
    let content = serde_json::json!({"requested_ms": now_ms(), "by": by});
    write_private_atomic(&paths.cancel_marker(), content.to_string().as_bytes())
        .map_err(|error| format!("record cancel request: {error}"))
}

pub(crate) fn bounded_text(mut value: String, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::data_dir::DataDir;

    fn agent(dir: &tempfile::TempDir) -> AgentPaths {
        let data = DataDir::from_path(dir.path()).unwrap();
        let paths = data
            .agent_dir("0123456789abcdef/0123456789abcdef0123456789abcdef")
            .unwrap();
        std::fs::create_dir_all(paths.dir()).unwrap();
        paths
    }

    fn meta(paths: &AgentPaths) -> TaskMeta {
        TaskMeta::new(
            "0123456789abcdef/0123456789abcdef0123456789abcdef".to_string(),
            None,
            true,
            "/repo".to_string(),
            "prompt".to_string(),
            RunOptions::default(),
            None,
        )
        .tap(paths)
    }

    trait Tap {
        fn tap(self, paths: &AgentPaths) -> Self;
    }
    impl Tap for TaskMeta {
        fn tap(self, paths: &AgentPaths) -> Self {
            save_meta(paths, &self).unwrap();
            self
        }
    }

    #[test]
    fn metadata_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        let saved = meta(&paths);
        assert_eq!(load_meta(&paths).unwrap(), saved);
    }

    #[test]
    fn terminal_records_are_absent_until_written_then_repeatably_observable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        assert!(read_terminal(&paths).unwrap().is_none());

        let mut record = meta(&paths);
        record.pending_terminal = Some(PendingTerminal::Succeeded {
            result: "done".to_string(),
        });
        let payload = record.terminal_payload().unwrap();
        write_terminal(&paths, &payload).unwrap();

        assert_eq!(read_terminal(&paths).unwrap().unwrap(), payload);
        assert_eq!(read_terminal(&paths).unwrap().unwrap(), payload);
        assert_eq!(
            payload,
            serde_json::json!({"state": "succeeded", "result": "done"})
        );
    }

    #[test]
    fn terminal_payloads_carry_truncation_and_bound_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        let mut record = meta(&paths);
        record.pending_terminal = Some(PendingTerminal::Failed {
            error: "took too long".to_string(),
        });
        record.stopped_by = Some("deadline".to_string());
        assert_eq!(
            record.terminal_payload().unwrap(),
            serde_json::json!({"state": "failed", "error": "took too long", "stopped_by": "deadline"})
        );

        record.pending_terminal = Some(PendingTerminal::Cancelled);
        assert_eq!(
            record.terminal_payload().unwrap(),
            serde_json::json!({"state": "cancelled"}),
            "a cancelled terminal never reports a bound"
        );
    }

    /// The terminal record is what `basis wait --json` and `basis list --json`
    /// read, and it is the only place a settled task's cost survives: the
    /// event journal can be capped, and the process that spent the tokens is
    /// gone.
    #[test]
    fn a_settled_task_records_what_it_spent_only_when_it_spent_something() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        let mut record = meta(&paths);
        record.pending_terminal = Some(PendingTerminal::Succeeded {
            result: "done".to_string(),
        });

        assert_eq!(
            record.terminal_payload().unwrap(),
            serde_json::json!({"state": "succeeded", "result": "done"}),
            "a task whose turns reported nothing claims no measurement"
        );

        record.usage = RunUsage {
            input_tokens: 900,
            output_tokens: 100,
            cache_read_tokens: 7,
            cache_creation_tokens: 3,
        };
        let payload = record.terminal_payload().unwrap();
        assert_eq!(payload["usage"]["input_tokens"], 900);
        assert_eq!(payload["usage"]["output_tokens"], 100);
        assert_eq!(payload["usage"]["cache_read_tokens"], 7);
        assert_eq!(payload["usage"]["cache_creation_tokens"], 3);
    }

    #[test]
    fn cancel_markers_signal_by_existence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        assert!(!cancel_requested(&paths));
        request_cancel(&paths, Some("caller/id")).unwrap();
        assert!(cancel_requested(&paths));
    }

    #[test]
    fn bounded_text_never_splits_utf8() {
        let (value, truncated) = bounded_text("a界b".to_string(), 2);
        assert_eq!(value, "a");
        assert!(truncated);
    }
}
