//! The remote agents one workspace open connected, as `spawn` targets.
//!
//! ADR-0029: a remote agent is a target of a delegation, not a tool. So
//! nothing here touches the runtime's tool registry — no name is claimed,
//! nothing is suffixed, nothing needs hiding from a sibling open. A workspace
//! connects its agents once, at open, and records the result on every agent it
//! mints or resumes (`runtime::agents`); `spawn` reads the calling agent's row
//! to route `@<agent>`, so an open reaches exactly its own agents with its own
//! credentials by construction.
//!
//! An agent that cannot be reached degrades as it did when it was a tool: a
//! warning names it, the open continues, and the name is still reported, so a
//! host can see what was configured. Its card is fetched once, here; nothing
//! retries it later.
//!
//! # The conversation is basis's
//!
//! ADR-0029 §5: within one session, successive delegations to one remote agent
//! continue one A2A conversation, and a task the remote agent has not finished
//! is that agent's *outstanding* task. [`RemoteAgents::conversation`] is where
//! both live — keyed by the calling agent and the remote one — so the model
//! never carries an id.
//!
//! **And it survives the session.** A basis task outlives its process, so the
//! remote work it handed off has to as well: a session resumed after a
//! restart that had forgotten its outstanding task would hand the same work
//! off a second time. So the record of an agent basis minted or resumed is
//! written beside the store that holds the session — one small file per agent
//! id, replaced whole on every change — and read back the first time a
//! resumed session asks. A delegated child is never resumed, so its record
//! stays in memory; so does every record on a runtime whose history is kept
//! nowhere. One writer per file is the attach lock's guarantee (one executor
//! per conversation), not this module's.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mentra::a2a::A2aClient;
use serde::{Deserialize, Serialize};

use super::{A2aAgent, A2aError};

/// One configured remote agent, connected or not.
pub(crate) struct RemoteAgent {
    name: String,
    /// `None` when the card could not be fetched at open: the name is still
    /// configured — and reported — but there is nothing to send to.
    client: Option<Arc<A2aClient>>,
}

impl RemoteAgent {
    pub(crate) fn client(&self) -> Option<&Arc<A2aClient>> {
        self.client.as_ref()
    }
}

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

/// One workspace open's remote agents, in configuration order.
pub(crate) struct RemoteAgents {
    agents: Vec<RemoteAgent>,
    /// Every calling agent's conversations, by its id; loaded from disk the
    /// first time a remembered agent is asked about.
    conversations: Mutex<HashMap<String, Conversations>>,
    /// Where remembered agents' records live, or `None` when history is kept
    /// nowhere.
    remembered_in: Option<PathBuf>,
}

/// One calling agent's conversations, by remote agent name — the file's shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Conversations {
    agents: BTreeMap<String, Conversation>,
}

impl RemoteAgents {
    /// Connects `agents`, fetching each one's card.
    ///
    /// One at a time, as the MCP servers are: an open is not latency-critical
    /// and a host reading warnings wants them in configuration order.
    pub(crate) async fn connect(agents: Vec<A2aAgent>, remembered_in: Option<PathBuf>) -> Self {
        let mut connected = Vec::with_capacity(agents.len());
        for agent in agents {
            let config = agent.into_config();
            let client = match A2aClient::connect(&config).await {
                Ok(client) => Some(Arc::new(client)),
                // mentra's error carries no server text and no header value,
                // and a configuration error that would have quoted the URL was
                // refused at load (`a2a::file::refused`).
                Err(error) => {
                    eprintln!(
                        "Warning: A2A agent '{}' failed to connect: {error}",
                        config.name
                    );
                    None
                }
            };
            connected.push(RemoteAgent {
                name: config.name,
                client,
            });
        }

        Self {
            agents: connected,
            conversations: Mutex::new(HashMap::new()),
            remembered_in,
        }
    }

    /// Every configured name, connected or not — what
    /// [`Workspace::a2a_agents`](crate::Workspace::a2a_agents) reports.
    pub(crate) fn names(&self) -> Vec<String> {
        self.agents.iter().map(|agent| agent.name.clone()).collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&RemoteAgent> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    /// The agents a model can actually be offered: the ones whose card came
    /// back.
    pub(crate) fn reachable(&self) -> impl Iterator<Item = (&str, &Arc<A2aClient>)> {
        self.agents
            .iter()
            .filter_map(|agent| Some((agent.name.as_str(), agent.client.as_ref()?)))
    }

    /// Where `caller`'s conversation with `remote` stands — read from disk
    /// the first time, when `caller` is `remembered`.
    pub(crate) fn conversation(
        &self,
        caller: &str,
        remote: &str,
        remembered: bool,
    ) -> Conversation {
        let mut conversations = self
            .conversations
            .lock()
            .expect("A2A conversation map poisoned");
        if !conversations.contains_key(caller) {
            let loaded = self
                .file(caller, remembered)
                .map(|file| load(&file))
                .unwrap_or_default();
            conversations.insert(caller.to_string(), loaded);
        }
        conversations
            .get(caller)
            .and_then(|own| own.agents.get(remote))
            .cloned()
            .unwrap_or_default()
    }

    /// Records where `caller`'s conversation with `remote` stands now, and —
    /// when `caller` is `remembered` — writes it through, so a restart finds
    /// it. A failed write is returned for the caller to report; the record in
    /// memory stands either way, because the exchange it describes happened.
    pub(crate) fn record(
        &self,
        caller: &str,
        remote: &str,
        conversation: Conversation,
        remembered: bool,
    ) -> std::io::Result<()> {
        let mut conversations = self
            .conversations
            .lock()
            .expect("A2A conversation map poisoned");
        let own = conversations.entry(caller.to_string()).or_default();
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

/// Names only: a client's `Debug` is safe, but nothing a host logs needs more
/// than which agents there are and which came up.
impl std::fmt::Debug for RemoteAgents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.agents
                    .iter()
                    .map(|agent| (&agent.name, agent.client.is_some())),
            )
            .finish()
    }
}

#[cfg(test)]
impl RemoteAgents {
    /// Agents that were configured but never connected — enough to exercise
    /// routing and refusals without a server.
    pub(crate) fn unreachable(names: &[&str]) -> Self {
        Self {
            agents: names
                .iter()
                .map(|name| RemoteAgent {
                    name: (*name).to_string(),
                    client: None,
                })
                .collect(),
            conversations: Mutex::new(HashMap::new()),
            remembered_in: None,
        }
    }

    /// The same, remembering under `dir`.
    pub(crate) fn unreachable_in(names: &[&str], dir: &Path) -> Self {
        Self {
            remembered_in: Some(dir.to_path_buf()),
            ..Self::unreachable(names)
        }
    }
}

#[cfg(test)]
mod tests;
