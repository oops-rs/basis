//! Which A2A ids this server handed out, for which tasks.
//!
//! A basis task directory knows nothing about A2A, and should not: it is
//! exactly what `basis-tasks` writes, readable by `basis list` and `basis
//! watch` with no adapter in sight. What A2A adds — a `contextId` grouping
//! follow-ups, the caller's own `messageId`, the full text it sent — lives
//! here, one small file per task under the host directory
//! `basis_tasks::Tasks::host_dir` gives this crate, or under the state
//! directory of a server whose tasks a host runs (ADR-0033 §2).
//!
//! For a `basis-tasks` task the A2A task id is the handle's own task part
//! (32 hex digits); the workspace key in front of it is the same for every
//! task one server serves, so it adds nothing a caller needs. For a hosted
//! task the server mints an id of the same shape and keeps the host's own
//! beside it, so a caller never sees a host's id and a host never parses an
//! A2A one. A caller's id is only ever looked up in memory, never joined
//! onto a path, so nothing a caller sends can name a file.
//!
//! Losing this directory loses the A2A view of past tasks, never a task:
//! every `basis-tasks` one is still a basis task, still listed and still
//! resumable by the CLI, and a hosted one is still its host's.

use std::{
    collections::HashMap,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use a2a::Part;
use basis_tasks::TaskHandle;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::config::{DEFAULT_PRINCIPAL, Principal};

/// Who runs a task, and the id it knows the task by.
///
/// One server serves one backend, so every record it writes carries the
/// same kind of key; a record of the other kind is one this server never
/// wrote, and is refused where it is used rather than guessed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Key {
    /// A `basis-tasks` task, by its full handle, `<workspace key>/<id>`.
    Tasks(TaskHandle),
    /// A task a host runs, by the host's own id for it (ADR-0033 §4). A
    /// caller never sees it.
    Hosted(String),
}

/// One task this server accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Stored", into = "Stored")]
pub(crate) struct Record {
    /// The A2A task id: the handle's task part, or one this server minted
    /// for a hosted task.
    pub(crate) id: String,
    pub(crate) key: Key,
    /// The name of the principal whose message created the task, recorded
    /// here rather than inferred later (proposal 0005 §4). A record written
    /// before principals existed belongs to the one caller there was.
    pub(crate) principal: String,
    pub(crate) context_id: String,
    /// The caller's `messageId` for the message that started the task.
    pub(crate) message_id: String,
    /// The prompt the task was started with: the caller's text exactly as
    /// sent, or its parts rendered into one (see `service::parts`).
    pub(crate) text: String,
    /// The caller's parts, kept only when some were more than text — the
    /// history then shows what was sent, not what the model was handed.
    pub(crate) parts: Option<Vec<Part>>,
    pub(crate) created_ms: u64,
    /// The hop the message that started the task arrived at (ADR-0030 §9).
    pub(crate) hop: u32,
    /// When the task gives up: its own deadline, clamped to its caller's.
    /// Absent on records written before hops existed.
    pub(crate) deadline_ms: Option<u64>,
    /// When this server saw the task settle. Absent while it runs.
    pub(crate) finished_ms: Option<u64>,
}

/// A record as it is written: [`Key`] spread over two optional fields, so a
/// file written before hosted tasks existed — `handle` and no `host_id` —
/// still loads as the `basis-tasks` record it is.
#[derive(Serialize, Deserialize)]
struct Stored {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handle: Option<TaskHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_id: Option<String>,
    #[serde(default = "default_principal")]
    principal: String,
    context_id: String,
    message_id: String,
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parts: Option<Vec<Part>>,
    created_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    hop: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finished_ms: Option<u64>,
}

impl TryFrom<Stored> for Record {
    type Error = String;

    fn try_from(stored: Stored) -> Result<Self, Self::Error> {
        let key = match (stored.handle, stored.host_id) {
            (Some(handle), None) => Key::Tasks(handle),
            (None, Some(host_id)) => Key::Hosted(host_id),
            _ => return Err("a task record names exactly one of `handle` and `host_id`".into()),
        };
        Ok(Self {
            id: stored.id,
            key,
            principal: stored.principal,
            context_id: stored.context_id,
            message_id: stored.message_id,
            text: stored.text,
            parts: stored.parts,
            created_ms: stored.created_ms,
            hop: stored.hop,
            deadline_ms: stored.deadline_ms,
            finished_ms: stored.finished_ms,
        })
    }
}

impl From<Record> for Stored {
    fn from(record: Record) -> Self {
        let (handle, host_id) = match record.key {
            Key::Tasks(handle) => (Some(handle), None),
            Key::Hosted(host_id) => (None, Some(host_id)),
        };
        Self {
            id: record.id,
            handle,
            host_id,
            principal: record.principal,
            context_id: record.context_id,
            message_id: record.message_id,
            text: record.text,
            parts: record.parts,
            created_ms: record.created_ms,
            hop: record.hop,
            deadline_ms: record.deadline_ms,
            finished_ms: record.finished_ms,
        }
    }
}

impl Record {
    /// A `basis-tasks` task's record: its A2A id is the handle's task part.
    #[cfg(test)]
    pub(crate) fn new(
        handle: &TaskHandle,
        principal: &Principal,
        context_id: String,
        message_id: String,
        text: String,
        created_ms: u64,
    ) -> Self {
        Self::keyed(
            task_id_of(handle).to_string(),
            Key::Tasks(handle.clone()),
            principal,
            context_id,
            message_id,
            text,
            created_ms,
        )
    }

    /// A record under the A2A id `id`, for the task `key` names.
    pub(crate) fn keyed(
        id: String,
        key: Key,
        principal: &Principal,
        context_id: String,
        message_id: String,
        text: String,
        created_ms: u64,
    ) -> Self {
        Self {
            id,
            key,
            principal: principal.name().to_string(),
            context_id,
            message_id,
            text,
            parts: None,
            hop: 0,
            deadline_ms: None,
            created_ms,
            finished_ms: None,
        }
    }

    pub(crate) fn with_parts(self, parts: Option<Vec<Part>>) -> Self {
        Self { parts, ..self }
    }

    pub(crate) fn with_hops(self, hop: u32, deadline_ms: u64) -> Self {
        Self {
            hop,
            deadline_ms: Some(deadline_ms),
            ..self
        }
    }

    /// The `basis-tasks` handle, when a `basis-tasks` task is what this
    /// record names.
    pub(crate) fn handle(&self) -> Option<&TaskHandle> {
        match &self.key {
            Key::Tasks(handle) => Some(handle),
            Key::Hosted(_) => None,
        }
    }

    /// The host's id for the task, when a host runs it.
    pub(crate) fn host_id(&self) -> Option<&str> {
        match &self.key {
            Key::Hosted(host_id) => Some(host_id),
            Key::Tasks(_) => None,
        }
    }

    /// The moment the task's status last changed, as far as this server
    /// knows: when it settled, else when it was accepted.
    pub(crate) fn status_ms(&self) -> u64 {
        self.finished_ms.unwrap_or(self.created_ms)
    }
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn default_principal() -> String {
    DEFAULT_PRINCIPAL.to_string()
}

/// The handle's task part — what A2A calls the task id.
pub(crate) fn task_id_of(handle: &TaskHandle) -> &str {
    handle
        .as_str()
        .rsplit_once('/')
        .map_or(handle.as_str(), |(_, id)| id)
}

/// Every record this server has written, in memory, over one directory.
pub(crate) struct Index {
    dir: PathBuf,
    records: Mutex<HashMap<String, Record>>,
}

impl Index {
    /// Loads every record under `dir/tasks`. A file that cannot be read or
    /// parsed fails the load, naming it: a server that silently dropped one
    /// would answer `TaskNotFound` for a task a caller was handed.
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        let dir = dir.join("tasks");
        fs::create_dir_all(&dir)?;
        let mut records = HashMap::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let bytes = fs::read(&path)?;
            let record: Record = serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", path.display()),
                )
            })?;
            records.insert(record.id.clone(), record);
        }
        Ok(Self {
            dir,
            records: Mutex::new(records),
        })
    }

    fn records(&self) -> MutexGuard<'_, HashMap<String, Record>> {
        // A panic while holding this lock leaves a map that was either
        // written or not; there is no half-state to protect.
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The record `id` names, whoever owns it — for this server's own
    /// bookkeeping. A caller's lookup goes through [`owned`](Self::owned).
    pub(crate) fn get(&self, id: &str) -> Option<Record> {
        self.records().get(id).cloned()
    }

    /// The record `id` names, when `caller` owns it. Another principal's task
    /// is `None` exactly as a missing one is, so "not yours" and "not found"
    /// cannot be told apart (ADR-0030 §8).
    pub(crate) fn owned(&self, caller: &Principal, id: &str) -> Option<Record> {
        self.get(id).filter(|record| caller.owns(&record.principal))
    }

    pub(crate) fn all(&self) -> Vec<Record> {
        self.records().values().cloned().collect()
    }

    /// `caller`'s tasks this server has not yet seen settle.
    pub(crate) fn unsettled_owned(&self, caller: &Principal) -> Vec<Record> {
        self.records()
            .values()
            .filter(|record| record.finished_ms.is_none() && caller.owns(&record.principal))
            .cloned()
            .collect()
    }

    /// Every record `caller` owns.
    pub(crate) fn all_owned(&self, caller: &Principal) -> Vec<Record> {
        self.records()
            .values()
            .filter(|record| caller.owns(&record.principal))
            .cloned()
            .collect()
    }

    /// The most recently accepted task `caller` owns in `context_id` — the
    /// one a follow-up continues from.
    pub(crate) fn latest_in(&self, caller: &Principal, context_id: &str) -> Option<Record> {
        self.records()
            .values()
            .filter(|record| record.context_id == context_id && caller.owns(&record.principal))
            .max_by_key(|record| record.created_ms)
            .cloned()
    }

    /// Writes `record` durably, then makes it visible. The file is written
    /// first so that a record a caller has been told about is never one a
    /// restart forgets.
    pub(crate) fn insert(&self, record: Record) -> io::Result<()> {
        self.persist(&record)?;
        self.records().insert(record.id.clone(), record);
        Ok(())
    }

    /// Records that `id` settled at `finished_ms`. A record already marked
    /// keeps its first time.
    pub(crate) fn finish(&self, id: &str, finished_ms: u64) -> io::Result<()> {
        let Some(mut record) = self.get(id) else {
            return Ok(());
        };
        if record.finished_ms.is_some() {
            return Ok(());
        }
        record.finished_ms = Some(finished_ms);
        self.insert(record)
    }

    fn persist(&self, record: &Record) -> io::Result<()> {
        let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
        let mut temporary = NamedTempFile::new_in(&self.dir)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.dir.join(format!("{}.json", record.id)))
            .map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef";

    fn handle(id: &str) -> TaskHandle {
        TaskHandle::parse(format!("{KEY}/{id}")).unwrap()
    }

    fn record(id: &str, context: &str, created_ms: u64) -> Record {
        Record::new(
            &handle(id),
            &Principal::anonymous(),
            context.into(),
            format!("m-{id}"),
            "hello".into(),
            created_ms,
        )
    }

    #[test]
    fn the_a2a_id_is_the_handles_task_part() {
        let id = "a".repeat(32);
        assert_eq!(task_id_of(&handle(&id)), id);
    }

    #[test]
    fn a_record_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        let id = "b".repeat(32);
        index.insert(record(&id, "ctx", 1)).unwrap();
        index.finish(&id, 5).unwrap();

        let reopened = Index::open(dir.path()).unwrap();
        let loaded = reopened.get(&id).expect("persisted");
        assert_eq!(loaded.finished_ms, Some(5));
        assert_eq!(loaded.handle(), Some(&handle(&id)));
    }

    #[test]
    fn finishing_twice_keeps_the_first_time() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        let id = "c".repeat(32);
        index.insert(record(&id, "ctx", 1)).unwrap();
        index.finish(&id, 5).unwrap();
        index.finish(&id, 9).unwrap();
        assert_eq!(index.get(&id).unwrap().status_ms(), 5);
    }

    #[test]
    fn the_latest_task_in_a_context_is_the_last_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        index.insert(record(&"1".repeat(32), "ctx", 1)).unwrap();
        index.insert(record(&"2".repeat(32), "ctx", 3)).unwrap();
        index.insert(record(&"3".repeat(32), "other", 9)).unwrap();
        let caller = Principal::anonymous();
        assert_eq!(index.latest_in(&caller, "ctx").unwrap().id, "2".repeat(32));
        assert!(index.latest_in(&caller, "missing").is_none());
    }

    #[test]
    fn another_principals_record_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        let id = "d".repeat(32);
        let alice = Principal::new("alice").unwrap();
        let bob = Principal::new("bob").unwrap();
        let mut owned = record(&id, "ctx", 1);
        owned.principal = alice.name().to_string();
        index.insert(owned).unwrap();
        assert!(index.owned(&alice, &id).is_some());
        assert!(index.owned(&bob, &id).is_none());
        assert!(index.all_owned(&bob).is_empty());
        assert!(index.latest_in(&bob, "ctx").is_none());
    }

    #[test]
    fn a_record_written_before_principals_belongs_to_the_default_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tasks")).unwrap();
        let id = "e".repeat(32);
        let legacy = serde_json::json!({
            "id": id, "handle": format!("{KEY}/{id}"), "context_id": "ctx",
            "message_id": "m", "text": "hi", "created_ms": 1,
        });
        fs::write(
            dir.path().join(format!("tasks/{id}.json")),
            legacy.to_string(),
        )
        .unwrap();
        let index = Index::open(dir.path()).unwrap();
        assert!(index.owned(&Principal::anonymous(), &id).is_some());
    }

    #[test]
    fn a_hosted_record_survives_reopening_under_its_hosts_id() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        let id = "f".repeat(32);
        let hosted = Record::keyed(
            id.clone(),
            Key::Hosted("run-7".into()),
            &Principal::anonymous(),
            "ctx".into(),
            "m".into(),
            "hi".into(),
            1,
        );
        index.insert(hosted).unwrap();
        let loaded = Index::open(dir.path()).unwrap().get(&id).unwrap();
        assert_eq!(loaded.host_id(), Some("run-7"));
        assert!(loaded.handle().is_none());
    }

    #[test]
    fn a_record_naming_both_keys_or_neither_is_refused() {
        let id = "0".repeat(32);
        for key in [
            serde_json::json!({"handle": format!("{KEY}/{id}"), "host_id": "x"}),
            serde_json::json!({}),
        ] {
            let mut raw = serde_json::json!({
                "id": id, "context_id": "ctx", "message_id": "m", "text": "hi",
                "created_ms": 1,
            });
            raw.as_object_mut()
                .unwrap()
                .extend(key.as_object().unwrap().clone());
            assert!(serde_json::from_value::<Record>(raw).is_err());
        }
    }

    #[test]
    fn an_unreadable_record_fails_the_open_by_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tasks")).unwrap();
        fs::write(dir.path().join("tasks/broken.json"), b"{").unwrap();
        let error = Index::open(dir.path()).err().expect("refused");
        assert!(error.to_string().contains("broken.json"), "{error}");
    }
}
