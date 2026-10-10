//! Which A2A ids this server handed out, for which basis tasks.
//!
//! A basis task directory knows nothing about A2A, and should not: it is
//! exactly what `basis-tasks` writes, readable by `basis list` and `basis
//! watch` with no adapter in sight. What A2A adds — a `contextId` grouping
//! follow-ups, the caller's own `messageId`, the full text it sent — lives
//! here, one small file per task under the host directory
//! `basis_tasks::Tasks::host_dir` gives this crate.
//!
//! The A2A task id is the handle's own task part (32 hex digits); the
//! workspace key in front of it is the same for every task one server
//! serves, so it adds nothing a caller needs. A caller's id is only ever
//! looked up in memory, never joined onto a path, so nothing a caller sends
//! can name a file.
//!
//! Losing this directory loses the A2A view of past tasks, never a task:
//! every one of them is still a basis task, still listed and still
//! resumable by the CLI.

use std::{
    collections::HashMap,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use basis_tasks::TaskHandle;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::config::{DEFAULT_PRINCIPAL, Principal};

/// One task this server accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The A2A task id: the handle's task part.
    pub(crate) id: String,
    /// The full basis handle, `<workspace key>/<id>`.
    pub(crate) handle: String,
    /// The name of the principal whose message created the task, recorded
    /// here rather than inferred later (proposal 0005 §4). A record written
    /// before principals existed belongs to the one caller there was.
    #[serde(default = "default_principal")]
    pub(crate) principal: String,
    pub(crate) context_id: String,
    /// The caller's `messageId` for the message that started the task.
    pub(crate) message_id: String,
    /// The text the task was started with, exactly as the caller sent it.
    pub(crate) text: String,
    pub(crate) created_ms: u64,
    /// When this server saw the task settle. Absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) finished_ms: Option<u64>,
}

impl Record {
    pub(crate) fn new(
        handle: &TaskHandle,
        principal: &Principal,
        context_id: String,
        message_id: String,
        text: String,
        created_ms: u64,
    ) -> Self {
        Self {
            id: task_id_of(handle).to_string(),
            handle: handle.as_str().to_string(),
            principal: principal.name().to_string(),
            context_id,
            message_id,
            text,
            created_ms,
            finished_ms: None,
        }
    }

    pub(crate) fn handle(&self) -> TaskHandle {
        TaskHandle::parse(self.handle.clone()).expect("a record is only built from a parsed handle")
    }

    /// The moment the task's status last changed, as far as this server
    /// knows: when it settled, else when it was accepted.
    pub(crate) fn status_ms(&self) -> u64 {
        self.finished_ms.unwrap_or(self.created_ms)
    }
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
        assert_eq!(loaded.handle(), handle(&id));
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
    fn an_unreadable_record_fails_the_open_by_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tasks")).unwrap();
        fs::write(dir.path().join("tasks/broken.json"), b"{").unwrap();
        let error = Index::open(dir.path()).err().expect("refused");
        assert!(error.to_string().contains("broken.json"), "{error}");
    }
}
