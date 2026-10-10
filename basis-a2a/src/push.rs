//! Push notifications, durably (ADR-0030 §4; proposal 0005 §3).
//!
//! A task's push configs live beside its A2A index record, one file per task
//! under `push/`, holding three things:
//!
//! - the configs, each with the sequence number of the last status change
//!   delivered to it;
//! - the status changes not yet delivered to every config, in order;
//! - the last status this server recorded for the task, so a change that
//!   happened while it was down — a task that settled between two lives —
//!   is still noticed as a change when it comes back.
//!
//! So delivery is at least once and survives a restart: whatever a config
//! had not acknowledged when the process ended is sent again by the next one.
//! The file is written before a change is acted on, and replaced atomically,
//! with the owner-only permissions `tempfile` creates it with — a config's
//! credentials are the caller's secret, held for it.
//!
//! The URL guard is in [`guard`], one POST in [`deliver`]; when to send is
//! the service's (`service::push`).

pub(crate) mod deliver;
pub(crate) mod guard;

use std::{
    collections::HashMap,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use a2a::{StreamResponse, TaskPushNotificationConfig};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

/// The most configs one task may carry. A webhook per interested party is
/// a handful; more is a caller fanning this server's requests out for it.
pub(crate) const MAX_CONFIGS: usize = 8;

/// The most undelivered changes kept per task. A task changes status a few
/// times in its life; a config that has fallen this far behind is skipped
/// forward rather than allowed to grow the file without bound.
const MAX_EVENTS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredConfig {
    config: TaskPushNotificationConfig,
    /// The sequence number of the last change delivered (or given up on).
    delivered: u64,
}

/// One status change, numbered in the order it was seen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Change {
    pub(crate) seq: u64,
    pub(crate) update: StreamResponse,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TaskPush {
    configs: Vec<StoredConfig>,
    #[serde(default)]
    changes: Vec<Change>,
    /// The number the last change was given.
    #[serde(default)]
    seq: u64,
    /// What the last recorded status was, as `service::push` keys it.
    #[serde(default)]
    last_status: Option<String>,
}

impl TaskPush {
    /// Drops every change all configs have had.
    fn prune(&mut self) {
        let floor = self
            .configs
            .iter()
            .map(|stored| stored.delivered)
            .min()
            .unwrap_or(self.seq);
        self.changes.retain(|change| change.seq > floor);
        if self.changes.len() > MAX_EVENTS {
            let excess = self.changes.len() - MAX_EVENTS;
            self.changes.drain(..excess);
        }
    }
}

/// Every task's push state, in memory, over one directory.
pub(crate) struct PushStore {
    dir: PathBuf,
    tasks: Mutex<HashMap<String, TaskPush>>,
}

impl PushStore {
    /// Loads every task's push file under `dir/push`. One that cannot be
    /// read fails the open by name, as the index does: a server that dropped
    /// it would silently stop calling a caller back.
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        let dir = dir.join("push");
        fs::create_dir_all(&dir)?;
        let mut tasks = HashMap::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()).filter(|_| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            }) else {
                continue;
            };
            let push: TaskPush = serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", path.display()),
                )
            })?;
            tasks.insert(id.to_string(), push);
        }
        Ok(Self {
            dir,
            tasks: Mutex::new(tasks),
        })
    }

    fn tasks(&self) -> MutexGuard<'_, HashMap<String, TaskPush>> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Adds `config` to `task`, or replaces the one with its id. A config is
    /// told about changes from now on, not about the ones before it.
    /// `None` when the task already has [`MAX_CONFIGS`] others.
    pub(crate) fn upsert(
        &self,
        task: &str,
        config: TaskPushNotificationConfig,
        status: Option<String>,
    ) -> io::Result<Option<TaskPushNotificationConfig>> {
        let mut tasks = self.tasks();
        let mut push = tasks.get(task).cloned().unwrap_or_default();
        if push.last_status.is_none() {
            push.last_status = status;
        }
        let stored = StoredConfig {
            config: config.clone(),
            delivered: push.seq,
        };
        let existing = push
            .configs
            .iter()
            .position(|existing| existing.config.id == config.id);
        match existing {
            Some(index) => push.configs[index] = stored,
            None if push.configs.len() >= MAX_CONFIGS => return Ok(None),
            None => push.configs.push(stored),
        }
        push.prune();
        self.persist(task, &push)?;
        tasks.insert(task.to_string(), push);
        Ok(Some(config))
    }

    pub(crate) fn get(&self, task: &str, id: &str) -> Option<TaskPushNotificationConfig> {
        self.list(task)
            .into_iter()
            .find(|config| config.id.as_deref() == Some(id))
    }

    pub(crate) fn list(&self, task: &str) -> Vec<TaskPushNotificationConfig> {
        self.tasks()
            .get(task)
            .map(|push| {
                push.configs
                    .iter()
                    .map(|stored| stored.config.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Removes `id` from `task`; `false` when there was no such config.
    pub(crate) fn delete(&self, task: &str, id: &str) -> io::Result<bool> {
        let mut tasks = self.tasks();
        let Some(mut push) = tasks.get(task).cloned() else {
            return Ok(false);
        };
        let before = push.configs.len();
        push.configs
            .retain(|stored| stored.config.id.as_deref() != Some(id));
        if push.configs.len() == before {
            return Ok(false);
        }
        push.prune();
        self.persist(task, &push)?;
        tasks.insert(task.to_string(), push);
        Ok(true)
    }

    /// Every task that has a config.
    pub(crate) fn tasks_with_configs(&self) -> Vec<String> {
        self.tasks()
            .iter()
            .filter(|(_, push)| !push.configs.is_empty())
            .map(|(task, _)| task.clone())
            .collect()
    }

    /// The status last recorded for `task`.
    #[cfg(test)]
    pub(crate) fn last_status(&self, task: &str) -> Option<String> {
        self.tasks()
            .get(task)
            .and_then(|push| push.last_status.clone())
    }

    /// Records that `task`'s status is now `status`, as `update` says. A
    /// status the same as the last is no change, and a task with no configs
    /// has nobody to tell; both answer `false`.
    pub(crate) fn record(
        &self,
        task: &str,
        status: String,
        update: StreamResponse,
    ) -> io::Result<bool> {
        let mut tasks = self.tasks();
        let Some(mut push) = tasks.get(task).cloned() else {
            return Ok(false);
        };
        if push.configs.is_empty() || push.last_status.as_deref() == Some(status.as_str()) {
            return Ok(false);
        }
        push.seq += 1;
        push.changes.push(Change {
            seq: push.seq,
            update,
        });
        push.last_status = Some(status);
        push.prune();
        self.persist(task, &push)?;
        tasks.insert(task.to_string(), push);
        Ok(true)
    }

    /// For each of `task`'s configs that is behind, the next change it has
    /// not had.
    pub(crate) fn pending(&self, task: &str) -> Vec<(TaskPushNotificationConfig, Change)> {
        let tasks = self.tasks();
        let Some(push) = tasks.get(task) else {
            return Vec::new();
        };
        push.configs
            .iter()
            .filter_map(|stored| {
                push.changes
                    .iter()
                    .find(|change| change.seq > stored.delivered)
                    .map(|change| (stored.config.clone(), change.clone()))
            })
            .collect()
    }

    /// Records that config `id` of `task` has had change `seq` — delivered,
    /// or given up on.
    pub(crate) fn delivered(&self, task: &str, id: &str, seq: u64) -> io::Result<()> {
        let mut tasks = self.tasks();
        let Some(mut push) = tasks.get(task).cloned() else {
            return Ok(());
        };
        let Some(stored) = push
            .configs
            .iter_mut()
            .find(|stored| stored.config.id.as_deref() == Some(id))
        else {
            return Ok(());
        };
        stored.delivered = stored.delivered.max(seq);
        push.prune();
        self.persist(task, &push)?;
        tasks.insert(task.to_string(), push);
        Ok(())
    }

    fn persist(&self, task: &str, push: &TaskPush) -> io::Result<()> {
        let bytes = serde_json::to_vec(push).map_err(io::Error::other)?;
        let mut temporary = NamedTempFile::new_in(&self.dir)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.dir.join(format!("{task}.json")))
            .map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
