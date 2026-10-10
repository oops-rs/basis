//! The remote agents one workspace open reaches, as `spawn` targets — a live
//! set.
//!
//! ADR-0029: a remote agent is a target of a delegation, not a tool. So
//! nothing here touches the runtime's tool registry — no name is claimed,
//! nothing is suffixed, nothing needs hiding from a sibling open. A workspace
//! records its [`RemoteAgents`] on every agent it mints or resumes
//! (`runtime::agents`); `spawn` reads the calling agent's row to route
//! `@<agent>`, so an open reaches exactly its own agents with its own
//! credentials by construction.
//!
//! # The set is live
//!
//! ADR-0031: what an open connected is not fixed for its lifetime. The agents
//! sit behind one lock as an immutable [`Roster`], swapped whole when the set
//! changes, so a reader — routing a call, describing `spawn` — takes one
//! consistent snapshot and never sees half a reload. Every holder of the
//! `Arc<RemoteAgents>` sees the swap at once, which is what makes a change
//! reach every agent of the open without touching a row.
//!
//! Each agent keeps the configuration it was connected with, so a reload can
//! tell a changed agent from an unchanged one, and when a connect was last
//! attempted, so an unreachable one is retried at most once per backoff
//! interval. A delegation clones the `Arc<A2aClient>` it starts with and holds
//! it to the end, so a swap never interrupts an exchange already in flight
//! (ADR-0031 §6) — it changes what the *next* `@<agent>` reaches.
//!
//! A name a reload took away is remembered as *removed*, so `@<agent>` is
//! refused as no longer configured rather than as a name nobody knows. Its
//! conversations are not touched (ADR-0031 §5): they are keyed by name, not by
//! connection, so re-adding the name continues them, and an outstanding task
//! whose agent moved to a new URL is waited on at the new one.
//!
//! After every change — a reload, or an unreachable agent reached on a retry
//! ([`retry`]) — the set calls the hook it was built with, which asks mentra to
//! capture every registered tool's description again; the `spawn` copies that
//! hold this set compute theirs from it, so the next model request is told the
//! set as it is.
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

mod conversation;
mod reload;
mod retry;

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use mentra::a2a::{A2aClient, A2aError as ClientError};

use super::A2aAgent;

pub(crate) use conversation::{Conversation, Outstanding, Waiting, forget};
pub(crate) use retry::{Route, Unreached};

/// How long an unreachable agent is left alone after a connect attempt
/// before `@<agent>` tries it again (ADR-0031 §2).
///
/// Long enough that a model asking for a down agent in a loop costs one
/// connect attempt rather than one per call; short enough that an agent
/// which came up is reached within a minute without anybody reloading.
pub const A2A_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// What the set calls after it changes — a reload, or an agent reached on a
/// retry — so the description `spawn` is registered with is captured again
/// (ADR-0031 §3). Called outside every lock here, because what it does reads
/// the set.
pub(crate) type OnChange = Box<dyn Fn() + Send + Sync>;

/// One configured remote agent, connected or not.
pub(crate) struct RemoteAgent {
    /// The configuration it was connected with — what a reload diffs against.
    agent: A2aAgent,
    /// `None` when the card could not be fetched: the name is still
    /// configured — and reported, and taught as unreachable — but there is
    /// nothing to send to until a later attempt succeeds.
    client: Option<Arc<A2aClient>>,
    /// When a connect was last attempted, successful or not: what the retry
    /// backoff is measured from.
    attempted: Mutex<Instant>,
}

impl RemoteAgent {
    /// An agent whose connect was attempted just now.
    fn new(agent: A2aAgent, client: Option<Arc<A2aClient>>) -> Self {
        Self {
            agent,
            client,
            attempted: Mutex::new(Instant::now()),
        }
    }

    pub(crate) fn name(&self) -> &str {
        self.agent.name()
    }

    pub(crate) fn client(&self) -> Option<&Arc<A2aClient>> {
        self.client.as_ref()
    }
}

/// One consistent view of the set: who is configured, in configuration order,
/// and who was configured once and is not any more.
#[derive(Default)]
pub(crate) struct Roster {
    agents: Vec<Arc<RemoteAgent>>,
    /// Names a reload took away, kept so `@<name>` is refused as *no longer
    /// configured* rather than as a name nobody ever heard of (ADR-0031 §5).
    /// A name configured again leaves this set; its conversations were never
    /// touched, so they continue.
    removed: BTreeSet<String>,
}

impl Roster {
    pub(crate) fn agents(&self) -> &[Arc<RemoteAgent>] {
        &self.agents
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<RemoteAgent>> {
        self.agents.iter().find(|agent| agent.name() == name)
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.agents
            .iter()
            .map(|agent| agent.name().to_string())
            .collect()
    }
}

/// One workspace open's remote agents, live.
pub(crate) struct RemoteAgents {
    roster: RwLock<Arc<Roster>>,
    conversations: conversation::Book,
    /// [`A2A_RETRY_BACKOFF`], but for tests.
    backoff: Duration,
    on_change: Option<OnChange>,
}

impl RemoteAgents {
    /// Connects `agents`, fetching each one's card.
    ///
    /// The cards are fetched concurrently — an open waits for the slowest, not
    /// the sum — but warnings are said in configuration order, which is what a
    /// host reading them expects.
    pub(crate) async fn connect(agents: Vec<A2aAgent>, remembered_in: Option<PathBuf>) -> Self {
        let connected = connect_each(agents)
            .await
            .into_iter()
            .map(|(agent, client)| {
                let client = warned(agent.name(), client);
                Arc::new(RemoteAgent::new(agent, client))
            })
            .collect();

        Self::with_roster(
            Roster {
                agents: connected,
                removed: BTreeSet::new(),
            },
            remembered_in,
        )
    }

    fn with_roster(roster: Roster, remembered_in: Option<PathBuf>) -> Self {
        Self {
            roster: RwLock::new(Arc::new(roster)),
            conversations: conversation::Book::new(remembered_in),
            backoff: A2A_RETRY_BACKOFF,
            on_change: None,
        }
    }

    /// Calls `on_change` after every change to the set from now on.
    pub(crate) fn with_on_change(self, on_change: OnChange) -> Self {
        Self {
            on_change: Some(on_change),
            ..self
        }
    }

    /// Says the set changed. Never called with a lock of this type held.
    fn changed(&self) {
        if let Some(on_change) = &self.on_change {
            on_change();
        }
    }

    /// Makes `roster` the set every reader sees from now on.
    fn swap(&self, roster: Roster) {
        *self
            .roster
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(roster);
    }

    /// The set as it stands now. Cheap — one `Arc` clone — and consistent: a
    /// reload swaps the whole roster, so a snapshot is never half of one.
    pub(crate) fn snapshot(&self) -> Arc<Roster> {
        Arc::clone(
            &self
                .roster
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Every configured name, connected or not — what
    /// [`Workspace::a2a_agents`](crate::Workspace::a2a_agents) reports.
    pub(crate) fn names(&self) -> Vec<String> {
        self.snapshot().names()
    }

    /// Where `caller`'s conversation with `remote` stands — read from disk
    /// the first time, when `caller` is `remembered`.
    pub(crate) fn conversation(
        &self,
        caller: &str,
        remote: &str,
        remembered: bool,
    ) -> Conversation {
        self.conversations.get(caller, remote, remembered)
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
        self.conversations
            .record(caller, remote, conversation, remembered)
    }
}

/// Fetches every agent's card at once and hands the results back in the order
/// the agents came in.
///
/// One task per agent rather than a joined future, so this needs no
/// dependency beyond tokio; awaiting the handles in order is what keeps the
/// order. A task that died is reported as a failed connect, which is what it
/// was.
async fn connect_each(
    agents: Vec<A2aAgent>,
) -> Vec<(A2aAgent, Result<Arc<A2aClient>, ClientError>)> {
    let pending: Vec<_> = agents
        .into_iter()
        .map(|agent| {
            let config = agent.config().clone();
            let task = tokio::spawn(async move { A2aClient::connect(&config).await });
            (agent, task)
        })
        .collect();

    let mut connected = Vec::with_capacity(pending.len());
    for (agent, task) in pending {
        let client = match task.await {
            Ok(result) => result.map(Arc::new),
            Err(_) => Err(ClientError::Connect),
        };
        connected.push((agent, client));
    }
    connected
}

/// The client, or — when the connect failed — `None`, after the one warning
/// a failed connect produces.
fn warned(name: &str, client: Result<Arc<A2aClient>, ClientError>) -> Option<Arc<A2aClient>> {
    // mentra's error carries no server text and no header value, and a
    // configuration error that would have quoted the URL was refused at load
    // (`a2a::file::refused`).
    client
        .inspect_err(|error| eprintln!("Warning: A2A agent '{name}' failed to connect: {error}"))
        .ok()
}

/// Names and reachability only: a client's `Debug` is safe, but nothing a host
/// logs needs more than which agents there are and which came up.
impl std::fmt::Debug for RemoteAgents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let roster = self.snapshot();
        f.debug_list()
            .entries(
                roster
                    .agents
                    .iter()
                    .map(|agent| (agent.name().to_string(), agent.client.is_some())),
            )
            .finish()
    }
}

#[cfg(test)]
impl RemoteAgents {
    /// Agents that were configured but never connected — enough to exercise
    /// routing and refusals without a server.
    pub(crate) fn unreachable(names: &[&str]) -> Self {
        Self::with_roster(
            Roster {
                agents: names
                    .iter()
                    .map(|name| {
                        Arc::new(RemoteAgent::new(
                            A2aAgent::new(mentra::a2a::A2aAgentConfig::new(
                                *name,
                                "http://127.0.0.1:1",
                            )),
                            None,
                        ))
                    })
                    .collect(),
                removed: BTreeSet::new(),
            },
            None,
        )
    }

    /// The same, remembering under `dir`.
    pub(crate) fn unreachable_in(names: &[&str], dir: &std::path::Path) -> Self {
        let mut agents = Self::unreachable(names);
        agents.conversations = conversation::Book::new(Some(dir.to_path_buf()));
        agents
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
