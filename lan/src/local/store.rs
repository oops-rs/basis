//! Durable task snapshots for the local service.
//!
//! The daemon is the sole writer, so a small JSON journal is enough for the
//! first service slice. Each update is written to a sibling temporary file and
//! atomically renamed; a client therefore observes either the old complete
//! snapshot or the new complete snapshot, never a partially serialized state.

use std::{
    collections::BTreeMap,
    fs, io,
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    protocol::MAX_MESSAGE,
    registry::{Registry, write_private_atomic},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub(crate) const MAX_EVENTS: usize = 128;
pub(crate) const MAX_MESSAGES: usize = 16;
pub(crate) const MAX_EVENT_BYTES: usize = 32 * 1024;
pub(crate) const MAX_RESULT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TASKS: usize = 1024;
pub(crate) const MAX_JOURNAL_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum DurableState {
    Running,
    Succeeded { result: String },
    Failed { error: String },
    Cancelled,
    Orphaned,
}

impl DurableState {
    pub(crate) const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// A task's own work has finished, but its attached children may still be
/// running. This deliberately excludes `Running` and `Orphaned`: only a
/// completed worker can be pending, while orphaning settles the whole daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum PendingTerminal {
    Succeeded { result: String },
    Failed { error: String },
    Cancelled,
}

impl From<PendingTerminal> for DurableState {
    fn from(value: PendingTerminal) -> Self {
        match value {
            PendingTerminal::Succeeded { result } => Self::Succeeded { result },
            PendingTerminal::Failed { error } => Self::Failed { error },
            PendingTerminal::Cancelled => Self::Cancelled,
        }
    }
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
pub(crate) struct TaskRecord {
    pub id: String,
    pub parent: Option<String>,
    pub detached: bool,
    pub workspace: String,
    pub agent_id: String,
    pub state: DurableState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_terminal: Option<PendingTerminal>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub result_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<String>,
    pub cancel_requested: bool,
    pub deadline_at_ms: Option<u64>,
    pub messages: Vec<MessageRecord>,
    pub events: Vec<EventRecord>,
    pub next_event: u64,
    pub created_ms: u64,
    pub updated_ms: u64,
}

impl TaskRecord {
    pub(crate) fn new(
        id: String,
        parent: Option<String>,
        detached: bool,
        workspace: String,
        agent_id: String,
        deadline_at_ms: Option<u64>,
    ) -> Self {
        let now = now_ms();
        Self {
            id,
            parent,
            detached,
            workspace,
            agent_id,
            state: DurableState::Running,
            pending_terminal: None,
            result_truncated: false,
            stopped_by: None,
            cancel_requested: false,
            deadline_at_ms,
            messages: Vec::new(),
            events: Vec::new(),
            next_event: 1,
            created_ms: now,
            updated_ms: now,
        }
    }

    /// Whether this task can still accept work into its local agent session.
    /// A task whose worker finished remains externally `Running` while its
    /// children settle, but no worker remains to consume new children or
    /// messages.
    pub(crate) fn accepts_work(&self) -> bool {
        matches!(self.state, DurableState::Running)
            && self.pending_terminal.is_none()
            && !self.cancel_requested
    }

    pub(crate) fn add_message(&mut self, body: String) -> Result<String, String> {
        if body.len() > MAX_MESSAGE {
            return Err(format!(
                "message is {} bytes; the limit is {MAX_MESSAGE}",
                body.len()
            ));
        }
        if self.messages.len() >= MAX_MESSAGES {
            return Err(format!("task inbox is full (limit {MAX_MESSAGES})"));
        }
        let id = Uuid::new_v4().simple().to_string();
        self.messages.push(MessageRecord {
            id: id.clone(),
            body,
            state: MessageState::Pending,
            created_ms: now_ms(),
            reply: None,
        });
        self.updated_ms = now_ms();
        Ok(id)
    }

    pub(crate) fn start_next_message(&mut self) -> Option<(String, String)> {
        let message = self
            .messages
            .iter_mut()
            .find(|message| message.state == MessageState::Pending)?;
        message.state = MessageState::InFlight;
        self.updated_ms = now_ms();
        Some((message.id.clone(), message.body.clone()))
    }

    pub(crate) fn finish_message(&mut self, id: &str, reply: Option<MessageReply>) {
        if let Some(message) = self.messages.iter_mut().find(|message| message.id == id) {
            message.state = MessageState::Delivered;
            message.reply = reply;
            self.updated_ms = now_ms();
        }
    }

    pub(crate) fn finish_unanswered_messages(&mut self) {
        let mut changed = false;
        for message in &mut self.messages {
            if message.state != MessageState::Delivered {
                message.state = MessageState::Delivered;
                changed = true;
            }
        }
        if changed {
            self.updated_ms = now_ms();
        }
    }

    pub(crate) fn record_event(&mut self, event: Value) {
        let event = if serde_json::to_vec(&event).is_ok_and(|bytes| bytes.len() <= MAX_EVENT_BYTES)
        {
            event
        } else {
            serde_json::json!({
                "type": "notice",
                "message": format!("event omitted because it exceeded {MAX_EVENT_BYTES} bytes"),
            })
        };
        let seq = self.next_event;
        self.next_event = self.next_event.saturating_add(1);
        self.events.push(EventRecord { seq, event });
        if self.events.len() > MAX_EVENTS {
            let remove = self.events.len() - MAX_EVENTS;
            self.events.drain(0..remove);
        }
        self.updated_ms = now_ms();
    }

    pub(crate) fn terminal_result(&self) -> Option<Value> {
        match &self.state {
            DurableState::Succeeded { result } => {
                let mut terminal =
                    self.terminal_object("succeeded", serde_json::json!({"result": result}));
                if self.result_truncated {
                    terminal["result_truncated"] = Value::Bool(true);
                }
                Some(terminal)
            }
            DurableState::Failed { error } => {
                Some(self.terminal_object("failed", serde_json::json!({"error": error})))
            }
            DurableState::Cancelled => Some(serde_json::json!({"state": "cancelled"})),
            DurableState::Orphaned => Some(serde_json::json!({"state": "orphaned"})),
            DurableState::Running => None,
        }
    }

    fn terminal_object(&self, state: &str, fields: Value) -> Value {
        let mut object =
            serde_json::Map::from_iter([("state".to_string(), Value::String(state.to_string()))]);
        if let Some(fields) = fields.as_object() {
            object.extend(fields.clone());
        }
        if let Some(stopped_by) = &self.stopped_by {
            object.insert("stopped_by".to_string(), Value::String(stopped_by.clone()));
        }
        Value::Object(object)
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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

pub(crate) type Journal = BTreeMap<String, TaskRecord>;

pub(crate) fn load(registry: &Registry, instance: &str) -> io::Result<Journal> {
    let path = registry.task_journal(instance);
    let mut journal = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Journal>(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => return Err(error),
    };

    // No in-memory worker can be reconstructed safely after a crash: replaying
    // a side-effectful prompt would duplicate work. Every persisted Running
    // task therefore settles as Orphaned before the daemon accepts clients.
    let mut changed = false;
    for task in journal.values_mut() {
        if matches!(task.state, DurableState::Running) {
            task.finish_unanswered_messages();
            task.state = DurableState::Orphaned;
            task.pending_terminal = None;
            task.updated_ms = now_ms();
            changed = true;
        }
    }
    if changed {
        save(registry, instance, &journal)?;
    }
    Ok(journal)
}

pub(crate) fn save(registry: &Registry, instance: &str, journal: &Journal) -> io::Result<()> {
    let path = registry.task_journal(instance);
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            format!("task journal exceeds {MAX_JOURNAL_BYTES} bytes"),
        ));
    }
    write_private_atomic(&path, &bytes)
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

    #[test]
    fn terminal_state_is_immutable_by_policy_and_reopenable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::from_path(dir.path()).expect("registry");
        let mut journal = Journal::new();
        let task = TaskRecord::new(
            "instance/task".to_string(),
            None,
            true,
            "/repo".to_string(),
            "agent".to_string(),
            None,
        );
        journal.insert(task.id.clone(), task);
        save(&registry, "instance", &journal).expect("save");
        let loaded = load(&registry, "instance").expect("load");
        assert_eq!(loaded.len(), 1);
        assert!(matches!(
            loaded["instance/task"].state,
            DurableState::Orphaned
        ));

        journal.get_mut("instance/task").unwrap().state = DurableState::Succeeded {
            result: "done".to_string(),
        };
        save(&registry, "instance", &journal).expect("save terminal");
        let reopened = load(&registry, "instance").expect("reopen");
        assert_eq!(
            reopened["instance/task"].terminal_result(),
            Some(serde_json::json!({"state": "succeeded", "result": "done"}))
        );
    }

    #[test]
    fn restart_settles_running_work_as_orphaned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::from_path(dir.path()).expect("registry");
        let mut journal = Journal::new();
        journal.insert(
            "i/t".to_string(),
            TaskRecord::new(
                "i/t".to_string(),
                None,
                true,
                "/repo".to_string(),
                "agent".to_string(),
                None,
            ),
        );
        save(&registry, "i", &journal).expect("save");
        let reopened = load(&registry, "i").expect("load");
        assert!(matches!(reopened["i/t"].state, DurableState::Orphaned));
    }

    #[test]
    fn restart_resolves_unanswered_messages_with_orphan_terminal_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::from_path(dir.path()).expect("registry");
        let mut journal = Journal::new();
        let mut task = TaskRecord::new(
            "i/t".to_string(),
            None,
            true,
            "/repo".to_string(),
            "agent".to_string(),
            None,
        );
        let in_flight = task
            .add_message("in flight".to_string())
            .expect("in-flight message");
        let pending = task
            .add_message("pending".to_string())
            .expect("pending message");
        task.start_next_message().expect("start first message");
        journal.insert(task.id.clone(), task);
        save(&registry, "i", &journal).expect("save");

        let reopened = load(&registry, "i").expect("load");
        let task = &reopened["i/t"];
        assert!(matches!(task.state, DurableState::Orphaned));
        for id in [in_flight, pending] {
            let message = task
                .messages
                .iter()
                .find(|message| message.id == id)
                .expect("message survives restart");
            assert_eq!(message.state, MessageState::Delivered);
            assert!(message.reply.is_none());
        }
    }

    #[test]
    fn bounded_text_never_splits_utf8() {
        let (value, truncated) = bounded_text("a界b".to_string(), 2);
        assert_eq!(value, "a");
        assert!(truncated);
    }

    #[test]
    fn oversized_events_become_small_explicit_notices() {
        let mut task = TaskRecord::new(
            "i/t".to_string(),
            None,
            true,
            "/repo".to_string(),
            "agent".to_string(),
            None,
        );
        task.record_event(serde_json::json!({"text": "x".repeat(MAX_EVENT_BYTES)}));

        assert_eq!(task.events.len(), 1);
        assert_eq!(task.events[0].event["type"], "notice");
    }

    #[test]
    fn journals_written_before_correlated_replies_still_deserialize() {
        // This is the minimum shape emitted before MessageRecord.reply and
        // TaskRecord.pending_terminal were introduced.  Compatibility is a
        // persistence invariant: adding fields must not make existing task
        // history unreadable.
        let old_journal = serde_json::json!({
            "i/t": {
                "id": "i/t",
                "parent": null,
                "detached": true,
                "workspace": "/repo",
                "agent_id": "agent",
                "state": {"state": "succeeded", "result": "done"},
                "cancel_requested": false,
                "deadline_at_ms": null,
                "messages": [{
                    "id": "message-1",
                    "body": "hello",
                    "state": "delivered",
                    "created_ms": 1
                }],
                "events": [],
                "next_event": 1,
                "created_ms": 1,
                "updated_ms": 2
            }
        });

        let journal: Journal = serde_json::from_value(old_journal).expect("old journal loads");
        let task = &journal["i/t"];
        assert!(task.pending_terminal.is_none());
        assert_eq!(task.messages.len(), 1);
        assert!(task.messages[0].reply.is_none());
        assert_eq!(task.messages[0].state, MessageState::Delivered);
    }

    #[test]
    fn terminal_cleanup_delivers_pending_and_inflight_messages() {
        let mut task = TaskRecord::new(
            "i/t".to_string(),
            None,
            true,
            "/repo".to_string(),
            "agent".to_string(),
            None,
        );
        let pending = task
            .add_message("pending".to_string())
            .expect("pending message");
        let in_flight = task
            .add_message("in flight".to_string())
            .expect("in-flight message");
        let started = task.start_next_message().expect("start message");
        assert_eq!(started.0, pending);
        assert!(matches!(
            task.messages
                .iter()
                .find(|message| message.id == pending)
                .unwrap()
                .state,
            MessageState::InFlight
        ));
        assert!(matches!(
            task.messages
                .iter()
                .find(|message| message.id == in_flight)
                .unwrap()
                .state,
            MessageState::Pending
        ));

        task.finish_unanswered_messages();

        assert!(
            task.messages
                .iter()
                .all(|message| message.state == MessageState::Delivered)
        );
    }
}
