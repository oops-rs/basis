//! Where each session's conversation with each remote agent stands
//! (ADR-0029 §5), and the files that carry it across a restart.
//!
//! Keyed by the calling agent and the remote agent's *name*, never by the
//! connection: a record outlives a reload that reconnects the agent, and one
//! that removes it (ADR-0031 §5), so re-adding the same name continues the
//! conversation it had.
//!
//! # Or by a host's key
//!
//! ADR-0035: a host that mints a fresh agent per message of one chat binds
//! its runs to a key of its own
//! ([`PreparedRun::with_remote_conversations`](crate::PreparedRun::with_remote_conversations)),
//! and every run bound to one key shares one record, whoever its calling
//! agent is. A keyed record is read from its file on every call rather than
//! cached, because the runs sharing it are not one session and need not be
//! one process; it is written as every record is — whole, beside the file,
//! then renamed over it — so a reader sees the old record or the new one,
//! never half. Two runs on one key writing at once are last writer wins: a
//! write reads the file, changes the one remote agent's entry it is about,
//! and replaces the file, so a concurrent change to *another* agent's entry
//! made between that read and that rename is lost. Within one workspace open
//! the read-change-write is serialized, so only writers in different opens
//! or processes can race. Runs on one key are a host's one conversation, one
//! message at a time, so the host decides whether they overlap.
//!
//! A runtime whose history is kept nowhere keeps keyed records in memory, per
//! workspace open, as it does every other.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};

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

/// Where a calling agent's conversations are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Remembered {
    /// In memory, under the calling agent's id: a delegated child, which is
    /// never resumed.
    #[default]
    Not,
    /// On disk, under the calling agent's id: an agent a workspace minted or
    /// resumed.
    ByAgent,
    /// Under a host's key, whoever the calling agent is (ADR-0035).
    ByKey(String),
}

/// The subdirectory keyed records live in, apart from agent ids' so a key
/// and an id that happen to be one string never share a file.
const KEYS_DIR: &str = "keys";

/// Every calling agent's conversations, loaded from disk the first time a
/// remembered agent is asked about — and every keyed one's, read from disk
/// every time.
pub(super) struct Book {
    by_caller: Mutex<HashMap<String, Conversations>>,
    /// Keyed records, when there is no directory to keep them in.
    by_key: Mutex<HashMap<String, Conversations>>,
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
            by_key: Mutex::new(HashMap::new()),
            remembered_in,
        }
    }

    /// Where `caller`'s conversation with `remote` stands — read from disk
    /// the first time, when `caller` is remembered by agent, and every time
    /// when it is remembered by key.
    pub(super) fn get(&self, caller: &str, remote: &str, remembered: &Remembered) -> Conversation {
        let remembered = match remembered {
            Remembered::ByKey(key) => return self.keyed(key, remote).unwrap_or_default(),
            Remembered::ByAgent => true,
            Remembered::Not => false,
        };
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
        remembered: &Remembered,
    ) -> std::io::Result<()> {
        let remembered = match remembered {
            Remembered::ByKey(key) => return self.record_keyed(key, remote, conversation),
            Remembered::ByAgent => true,
            Remembered::Not => false,
        };
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

    /// Where the conversation with `remote` under `key` stands, or `None`
    /// when there is none.
    pub(super) fn keyed(&self, key: &str, remote: &str) -> Option<Conversation> {
        match &self.remembered_in {
            Some(dir) => load(&keyed_file(dir, key)).agents.remove(remote),
            None => self
                .by_key
                .lock()
                .expect("A2A conversation map poisoned")
                .get(key)?
                .agents
                .get(remote)
                .cloned(),
        }
    }

    /// Records the conversation with `remote` under `key`: read, changed and
    /// replaced under this book's lock, so two runs of one open cannot lose
    /// each other's change. See the module docs for two opens.
    fn record_keyed(
        &self,
        key: &str,
        remote: &str,
        conversation: Conversation,
    ) -> std::io::Result<()> {
        let mut by_key = self.by_key.lock().expect("A2A conversation map poisoned");
        match &self.remembered_in {
            Some(dir) => {
                let file = keyed_file(dir, key);
                let mut own = load(&file);
                own.agents.insert(remote.to_string(), conversation);
                store(&file, &own)
            }
            None => {
                by_key
                    .entry(key.to_string())
                    .or_default()
                    .agents
                    .insert(remote.to_string(), conversation);
                Ok(())
            }
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

/// The file a keyed record lives in under `dir`.
fn keyed_file(dir: &Path, key: &str) -> PathBuf {
    dir.join(KEYS_DIR)
        .join(format!("{}.json", file_name_of(key)))
}

/// A host's key as a file name: the key itself when it is already a plain
/// one — lowercase letters, digits, `-` and `_`, at most 64 of them — and
/// otherwise its plain characters (lowercased, everything else `_`, at most
/// 64) followed by a digest of the whole key, so no key names a path outside
/// the directory, two keys a case-insensitive file system would confuse stay
/// apart, and distinct keys keep distinct files.
pub(crate) fn file_name_of(key: &str) -> String {
    let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
    if !key.is_empty() && key.len() <= 64 && key.chars().all(plain) {
        return key.to_string();
    }
    let readable: String = key
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if plain(c) { c } else { '_' })
        .take(64)
        .collect();
    format!("{readable}~{:016x}", fnv1a(key.as_bytes()))
}

/// FNV-1a, 64-bit: stable across builds and platforms, which a file name a
/// later process must find again needs, and std's hasher does not promise.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
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
    // Named for this write alone, so two writers of one record — two runs
    // on one key — never interleave into one staged file.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let staged = file.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&staged, bytes)?;
    std::fs::rename(&staged, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })
}

/// Removes `agent_id`'s record from `dir`, when it has one — what
/// [`crate::store::forget`] does for the conversation's remote work. A
/// failure names the file it could not remove.
pub(crate) fn forget(dir: &Path, agent_id: &str) -> Result<(), (PathBuf, std::io::Error)> {
    if !is_file_safe(agent_id) {
        return Ok(());
    }
    let path = dir.join(format!("{agent_id}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err((path, source)),
    }
}
