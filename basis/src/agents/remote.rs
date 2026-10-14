//! The remote agents one workspace open reaches, as `spawn` targets: its
//! resolvers, asked in order, and the conversation basis keeps with each
//! agent they resolve.
//!
//! ADR-0029: a remote agent is a target of a delegation, not a tool. So
//! nothing here touches the runtime's tool registry — no name is claimed,
//! nothing is suffixed, nothing needs hiding from a sibling open. A workspace
//! records its [`RemoteAgents`] on every agent it mints or resumes
//! (`runtime::agents`); `spawn` reads the calling agent's row to route
//! `@<agent>`, so an open reaches exactly its own agents with its own
//! credentials by construction.
//!
//! # Who answers a name
//!
//! ADR-0037: `@<name>` is resolved. The resolvers are asked in order — the
//! host's, when it installed one, then the set the open's files configure
//! (the `a2a` feature's, when it is on) — and the first that does not answer
//! [`Unresolved::Unknown`] decides. What a resolver hands back is a connected
//! client and who the agent is; what this module keeps around it is
//! everything the model would otherwise carry.
//!
//! # The conversation is basis's
//!
//! ADR-0029 §5: within one session, successive delegations to one remote agent
//! continue one conversation, and a task the remote agent has not finished
//! is that agent's *outstanding* task. [`RemoteAgents::conversation`] is where
//! both live — keyed by the calling agent and the remote agent's id
//! (ADR-0037 §6) — so the model never carries an id.
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
//!
//! A host may instead bind a run's conversations to a key of its own
//! (ADR-0035), shared by every run bound to it; `conversation` has the
//! rules.

mod conversation;

use std::{path::PathBuf, sync::Arc};

use super::{AgentResolver, Resolved, Route, Taught, Unresolved};

pub(crate) use conversation::{Conversation, Outstanding, Remembered, Waiting, forget};

/// One workspace open's remote agents: who answers a name, and where each
/// conversation stands.
pub(crate) struct RemoteAgents {
    /// Asked in order; the first answer other than *unknown* stands.
    resolvers: Vec<Arc<dyn AgentResolver>>,
    conversations: conversation::Book,
    /// The hop the work behind these delegations arrived at in a chain of
    /// agents, from the runtime ([`crate::Runtime::hop`]); every message sent is the
    /// next one.
    hop: u32,
}

impl RemoteAgents {
    /// The agents `resolvers` answer for, strongest first, with conversations
    /// remembered under `remembered_in` when there is a directory to keep
    /// them in.
    pub(crate) fn over(
        resolvers: Vec<Arc<dyn AgentResolver>>,
        remembered_in: Option<PathBuf>,
    ) -> Self {
        Self {
            resolvers,
            conversations: conversation::Book::new(remembered_in),
            hop: 0,
        }
    }

    /// Says the work these agents are handed arrived at `hop`.
    pub(crate) fn with_hop(self, hop: u32) -> Self {
        Self { hop, ..self }
    }

    /// The hop the work behind these delegations arrived at.
    pub(crate) fn hop(&self) -> u32 {
        self.hop
    }

    /// Where `@<name>` stands now, decided without waiting on anything: the
    /// first resolver's answer that is not *unknown*.
    pub(crate) fn route(&self, name: &str) -> Route {
        self.resolvers
            .iter()
            .map(|resolver| resolver.route(name))
            .find(|route| *route != Route::Unknown)
            .unwrap_or(Route::Unknown)
    }

    /// The agent `@<name>` is handed to, from the first resolver that knows
    /// it. The caller holds the client for its whole exchange.
    pub(crate) async fn resolve(&self, name: &str) -> Result<Resolved, Unresolved> {
        for resolver in &self.resolvers {
            match resolver.resolve(name).await {
                Err(Unresolved::Unknown) => continue,
                answer => return answer,
            }
        }
        Err(Unresolved::Unknown)
    }

    /// What every resolver teaches, strongest first: the agents concatenated
    /// — one entry per name, the first resolver's, since that is the one a
    /// call reaches — and the notes joined.
    pub(crate) fn taught(&self) -> Taught {
        let mut taught = Taught::default();
        let mut notes = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for resolver in &self.resolvers {
            let each = resolver.taught();
            taught.agents.extend(
                each.agents
                    .into_iter()
                    .filter(|agent| seen.insert(agent.name.clone())),
            );
            notes.extend(each.note);
        }
        if !notes.is_empty() {
            taught.note = Some(notes.join(" "));
        }
        taught
    }

    /// Every taught name, strongest first — what
    /// [`Workspace::remote_agents`](crate::Workspace::remote_agents) reports.
    pub(crate) fn names(&self) -> Vec<String> {
        self.taught()
            .agents
            .into_iter()
            .map(|agent| agent.name)
            .collect()
    }

    /// Where `caller`'s conversation with the agent `id` stands, kept as
    /// `remembered` says.
    pub(crate) fn conversation(
        &self,
        caller: &str,
        id: &str,
        remembered: &Remembered,
    ) -> Conversation {
        self.conversations.get(caller, id, remembered)
    }

    /// Where the conversation with the agent `id` under a host's `key`
    /// stands, or `None` when there is none (ADR-0035).
    pub(crate) fn keyed_conversation(&self, key: &str, id: &str) -> Option<Conversation> {
        self.conversations.keyed(key, id)
    }

    /// Records where `caller`'s conversation with the agent `id` stands now,
    /// and — when it is remembered on disk — writes it through, so a restart
    /// finds it. A failed write is returned for the caller to report; a
    /// record kept by agent stands in memory either way, because the
    /// exchange it describes happened.
    pub(crate) fn record(
        &self,
        caller: &str,
        id: &str,
        conversation: Conversation,
        remembered: &Remembered,
    ) -> std::io::Result<()> {
        self.conversations
            .record(caller, id, conversation, remembered)
    }
}

/// Taught names only: nothing a host logs needs more than which agents there
/// are.
impl std::fmt::Debug for RemoteAgents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

#[cfg(test)]
impl RemoteAgents {
    /// Over no resolver at all, remembering under `dir` when given: enough
    /// to exercise the conversation record.
    pub(crate) fn alone(dir: Option<&std::path::Path>) -> Self {
        Self::over(Vec::new(), dir.map(std::path::Path::to_path_buf))
    }
}

#[cfg(all(test, feature = "a2a"))]
impl RemoteAgents {
    /// Agents that were configured but never connected — enough to exercise
    /// routing and refusals without a server.
    pub(crate) fn unreachable(names: &[&str]) -> Self {
        Self::over(
            vec![Arc::new(
                crate::a2a::configured::ConfiguredAgents::unreachable(names),
            )],
            None,
        )
    }

    /// Over one configured set, which the test keeps a handle on to reload.
    pub(crate) fn over_configured(
        configured: &Arc<crate::a2a::configured::ConfiguredAgents>,
    ) -> Self {
        Self::over(vec![Arc::clone(configured) as Arc<dyn AgentResolver>], None)
    }
}

#[cfg(test)]
mod tests;
