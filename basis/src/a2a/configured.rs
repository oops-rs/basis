//! The resolver over what a workspace's files configure — the
//! [`AgentResolver`] every open gets when its host installs none
//! (ADR-0037 §9).
//!
//! What it resolves is the set ADR-0028 §2 reads: the host-supplied list,
//! `.basis/a2a.json`, the global `a2a.json`, layered by name. Nothing here
//! reads a file — [`super::live`] does, and hands the agents over — and
//! nothing here touches the runtime's tool registry: a remote agent is a
//! target of a delegation, not a tool (ADR-0029).
//!
//! # The set is live
//!
//! ADR-0031: what an open connected is not fixed for its lifetime. The agents
//! sit behind one lock as an immutable [`Roster`], swapped whole when the set
//! changes, so a reader — routing a call, describing `spawn` — takes one
//! consistent snapshot and never sees half a reload. Every holder of the
//! `Arc<ConfiguredAgents>` sees the swap at once, which is what makes a change
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
//! ([`retry`]) — the set calls the hook it was given, which asks mentra to
//! capture every registered tool's description again.

mod reload;
mod retry;

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use mentra::a2a::{A2aClient, A2aError as ClientError};

use super::A2aAgent;
use crate::agents::{AgentResolver, OnChange, Resolved, Route, Taught, TaughtAgent, Unresolved};

/// How long an unreachable agent is left alone after a connect attempt
/// before `@<agent>` tries it again (ADR-0031 §2).
///
/// Long enough that a model asking for a down agent in a loop costs one
/// connect attempt rather than one per call; short enough that an agent
/// which came up is reached within a minute without anybody reloading.
pub const A2A_RETRY_BACKOFF: Duration = Duration::from_secs(30);

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

    #[cfg(test)]
    pub(crate) fn names(&self) -> Vec<String> {
        self.agents
            .iter()
            .map(|agent| agent.name().to_string())
            .collect()
    }
}

/// The agents one workspace open's files configure, live.
pub(crate) struct ConfiguredAgents {
    roster: RwLock<Arc<Roster>>,
    /// [`A2A_RETRY_BACKOFF`], but for tests.
    backoff: Duration,
    /// Set once, when the open installs this resolver.
    on_change: OnceLock<OnChange>,
}

impl ConfiguredAgents {
    /// Connects `agents`, fetching each one's card.
    ///
    /// The cards are fetched concurrently — an open waits for the slowest, not
    /// the sum — but warnings are said in configuration order, which is what a
    /// host reading them expects.
    pub(crate) async fn connect(agents: Vec<A2aAgent>) -> Self {
        let connected = connect_each(agents)
            .await
            .into_iter()
            .map(|(agent, client)| {
                let client = warned(agent.name(), client);
                Arc::new(RemoteAgent::new(agent, client))
            })
            .collect();

        Self::with_roster(Roster {
            agents: connected,
            removed: BTreeSet::new(),
        })
    }

    fn with_roster(roster: Roster) -> Self {
        Self {
            roster: RwLock::new(Arc::new(roster)),
            backoff: A2A_RETRY_BACKOFF,
            on_change: OnceLock::new(),
        }
    }

    /// Says the set changed. Never called with a lock of this type held.
    fn changed(&self) {
        if let Some(on_change) = self.on_change.get() {
            on_change();
        }
    }

    /// Makes what `next` builds from the set as it stands *now* the set every
    /// reader sees from now on — decided under the write lock, so nothing
    /// installed meanwhile (a retry's client, [`retry`]) is overwritten by a
    /// view taken before it.
    fn swap_with(&self, next: impl FnOnce(&Roster) -> Roster) {
        let mut roster = self
            .roster
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *roster = Arc::new(next(&roster));
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

    /// Every configured name, connected or not.
    #[cfg(test)]
    pub(crate) fn names(&self) -> Vec<String> {
        self.snapshot().names()
    }
}

#[async_trait]
impl AgentResolver for ConfiguredAgents {
    async fn resolve(&self, name: &str) -> Result<Resolved, Unresolved> {
        self.reach(name).await
    }

    fn route(&self, name: &str) -> Route {
        self.standing(name)
    }

    /// Every configured agent, in configuration order, with what its card
    /// says — or as unreachable when its card could not be fetched, since
    /// asking is what retries it (ADR-0031 §2).
    fn taught(&self) -> Taught {
        self.snapshot()
            .agents()
            .iter()
            .fold(Taught::default(), |taught, agent| {
                let mut entry = TaughtAgent::new(agent.name());
                if let Some(client) = agent.client() {
                    entry = entry.with_summary(mentra::a2a::card_summary(client.card()));
                }
                taught.with_agent(entry)
            })
    }

    fn on_change(&self, hook: OnChange) {
        // Given once, at install; a second installation keeps the first.
        let _ = self.on_change.set(hook);
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
impl std::fmt::Debug for ConfiguredAgents {
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
impl ConfiguredAgents {
    /// Agents that were configured but never connected — enough to exercise
    /// routing and refusals without a server.
    pub(crate) fn unreachable(names: &[&str]) -> Self {
        Self::with_roster(Roster {
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
        })
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
