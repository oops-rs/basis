//! Where each session's conversation with each remote agent stands
//! (ADR-0029 §5), and the files that carry it across a restart.
//!
//! Keyed by the calling agent and the remote agent's *name*, never by the
//! connection: a record outlives a reload that reconnects the agent, and one
//! that removes it (ADR-0031 §5), so re-adding the same name continues the
//! conversation it had.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

use crate::a2a::A2aError;

/// Where one session's conversation with one remote agent stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Conversation {
    /// The A2A `contextId` the remote agent minted on the first task, sent
    /// with every later one so they read as one conversation.
    pub(crate) context_id: Option<String>,
    /// The task the remote agent has not finished, when there is one.
    pub(crate) outstanding: Option<Outstanding>,
}

/// A task handed back before it settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Outstanding {
    pub(crate) task_id: String,
    pub(crate) waiting_for: Waiting,
    /// The endpoint the task was handed to ([`A2aAgent::endpoint`]), so a
    /// call after the agent moved can tell that the task belongs to the old
    /// one. `None` on a record written before basis kept it.
    ///
    /// [`A2aAgent::endpoint`]: crate::a2a::A2aAgent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) endpoint: Option<String>,
}

/// What an outstanding task is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Waiting {
    /// The remote agent itself: it is still working, or the last check on it
    /// failed and it may be.
    Agent,
    /// This side: it asked a question, and `@<agent> <answer>` answers it.
    Input,
}

/// Every calling agent's conversations, loaded from disk the first time a
/// remembered agent is asked about.
pub(super) struct Book {
    by_caller: Mutex<HashMap<String, Conversations>>,
    /// Where remembered agents' records live, or `None` when history is kept
    /// nowhere.
    remembered_in: Option<PathBuf>,
}

/// One calling agent's conversations, by remote agent name — the file's shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Conversations {
    agents: BTreeMap<String, Conversation>,
}

impl Book {
    pub(super) fn new(remembered_in: Option<PathBuf>) -> Self {
        Self {
            by_caller: Mutex::new(HashMap::new()),
            remembered_in,
        }
    }

    /// Where `caller`'s conversation with `remote` stands — read from disk
    /// the first time, when `caller` is `remembered`.
    pub(super) fn get(&self, caller: &str, remote: &str, remembered: bool) -> Conversation {
        let mut by_caller = self
            .by_caller
            .lock()
            .expect("A2A conversation map poisoned");
        if !by_caller.contains_key(caller) {
            let loaded = self
                .file(caller, remembered)
                .map(|file| load(&file))
                .unwrap_or_default();
            by_caller.insert(caller.to_string(), loaded);
        }
        by_caller
            .get(caller)
            .and_then(|own| own.agents.get(remote))
            .cloned()
            .unwrap_or_default()
    }

    /// Records where `caller`'s conversation with `remote` stands now, writing
    /// it through when `caller` is `remembered`.
    pub(super) fn record(
        &self,
        caller: &str,
        remote: &str,
        conversation: Conversation,
        remembered: bool,
    ) -> std::io::Result<()> {
        let mut by_caller = self
            .by_caller
            .lock()
            .expect("A2A conversation map poisoned");
        let own = by_caller.entry(caller.to_string()).or_default();
        own.agents.insert(remote.to_string(), conversation);
        match self.file(caller, remembered) {
            Some(file) => store(&file, own),
            None => Ok(()),
        }
    }

    /// The file `caller`'s record lives in, when it is kept on disk at all.
    fn file(&self, caller: &str, remembered: bool) -> Option<PathBuf> {
        if !remembered || !is_file_safe(caller) {
            return None;
        }
        Some(self.remembered_in.as_ref()?.join(format!("{caller}.json")))
    }
}

/// An agent id basis will turn into a file name: mentra's ids are this shape,
/// and anything else stays in memory rather than become a path.
fn is_file_safe(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A record on disk, or nothing. A file that cannot be read is said once and
/// treated as no record: the worst that follows is a new conversation, which
/// is what having no record already means.
fn load(file: &Path) -> Conversations {
    match std::fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            eprintln!(
                "Warning: ignoring unreadable A2A conversation record {}: {error}",
                file.display()
            );
            Conversations::default()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Conversations::default(),
        Err(error) => {
            eprintln!(
                "Warning: ignoring unreadable A2A conversation record {}: {error}",
                file.display()
            );
            Conversations::default()
        }
    }
}

/// Replaces a record whole: written beside it, then renamed over it, so a
/// crash leaves the old record or the new one and never half of either.
fn store(file: &Path, conversations: &Conversations) -> std::io::Result<()> {
    let dir = file.parent().expect("a record file has a directory");
    std::fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec_pretty(conversations).map_err(std::io::Error::other)?;
    let staged = file.with_extension("json.tmp");
    std::fs::write(&staged, bytes)?;
    std::fs::rename(&staged, file)
}

/// Removes `agent_id`'s record from `dir`, when it has one — what
/// [`crate::store::forget`] does for the conversation's remote work.
pub(crate) fn forget(dir: &Path, agent_id: &str) -> Result<(), A2aError> {
    if !is_file_safe(agent_id) {
        return Ok(());
    }
    let path = dir.join(format!("{agent_id}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(A2aError::Forget { path, source }),
    }
}
