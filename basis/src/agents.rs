//! Who `@<name>` is, when it is a remote agent (ADR-0037) — the contract
//! between `spawn`'s grammar and whoever answers it.
//!
//! A workspace open answers `@<name>` through one or more resolvers, asked in
//! order: the one a host installs
//! ([`WorkspaceBuilder::with_agent_resolver`](crate::WorkspaceBuilder::with_agent_resolver)),
//! then whatever the build adds (the `a2a` feature's configured set, when it
//! is on). A resolver that does not know
//! a name says [`Unresolved::Unknown`] and the next one is asked; the first
//! other answer stands.
//!
//! What crosses the seam is small on purpose: a name in, a [`Peer`] out —
//! something a task can be handed to and that answers (ADR-0038). A2A is one
//! peer, behind its feature; naming needs none. Connecting, caching, credentials and retries are the resolver's.
//! basis keeps everything around it — the conversation with each agent, the
//! hop metadata, the exchange, and the one `spawn` door — so a host never
//! reimplements ADR-0029 §5 to answer a name its own way.
//!
//! `spawn` asks twice. Its authorization preview runs before the approver and
//! cannot wait on the network, so [`AgentResolver::route`] answers from what
//! the resolver already knows. Its execution can wait, so
//! [`AgentResolver::resolve`] does the work.

mod peer;
pub(crate) mod remote;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;

pub use peer::{
    Attachment, ExchangeBounds, Failure, Message, Outcome, Peer, PeerError, Request, Started,
    StillWorking,
};

/// What a resolver calls after its set changes — an agent added, removed or
/// reached — so the description `spawn` is registered with is captured again
/// (ADR-0031 §3). Shared, because every resolver of one open is handed the
/// same hook. Never called with a resolver's own lock held, because what it
/// does reads the resolver.
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// Who `@<name>` is, for one workspace open.
///
/// One required method. The rest default to the least a resolver can be: one
/// that cannot say anything without the network, teaches nothing, and never
/// changes. A resolver over a set it can enumerate — a file, a fixed list —
/// overrides all three.
#[async_trait]
pub trait AgentResolver: Send + Sync {
    /// The client `@name` is handed to, or why there is none.
    ///
    /// Connecting, caching, credentials and retries are the resolver's; basis
    /// holds the peer it is given for the whole exchange, so a resolver that
    /// hands out a new peer changes what the *next* `@name` reaches, never an
    /// exchange in flight (ADR-0031 §6).
    async fn resolve(&self, name: &str) -> Result<Resolved, Unresolved>;

    /// Where `@name` stands without waiting on anything — the authorization
    /// preview's question, asked before the approver.
    ///
    /// The default is [`Route::Maybe`]: the preview claims the delegation may
    /// leave the process, and [`resolve`](Self::resolve) decides.
    fn route(&self, _name: &str) -> Route {
        Route::Maybe
    }

    /// What `spawn` teaches the model about this resolver's names, read each
    /// time mentra captures the description. The default teaches nothing, so
    /// an agent is never taught a prefix that leads nowhere.
    fn taught(&self) -> Taught {
        Taught::default()
    }

    /// Given once, when the resolver is installed on an open: what to call
    /// after the set changes. The default keeps nothing, for a set that never
    /// changes.
    fn on_change(&self, _hook: OnChange) {}
}

/// Where `@<name>` stands, decided without waiting on anything.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Route {
    /// A remote agent this resolver knows, reachable or due a retry: the
    /// call may go ahead, and resolving it is the first thing it does.
    Known,
    /// Not this resolver's; the next one is asked.
    Unknown,
    /// Cannot say without the network.
    Maybe,
    /// Known, unreachable, and tried too recently to try again for this long
    /// (ADR-0031 §2).
    Backoff(Duration),
    /// Known, and refused for this reason — a complete sentence in the
    /// resolver's words, which `spawn` follows with its own guidance.
    Refused(String),
}

/// A remote agent, resolved: the peer to talk to, and who it is.
#[derive(Clone)]
#[non_exhaustive]
pub struct Resolved {
    /// What the task is handed to.
    pub peer: Arc<dyn Peer>,
    /// The permanent identity the conversation is kept under (ADR-0037 §6).
    /// The canonical name when the resolver has nothing more permanent.
    pub id: String,
    /// The canonical name: `ns/name` for a short handle the resolver
    /// expanded.
    pub name: String,
    /// Where the peer reaches the agent, as a string stable across
    /// processes, so a task outstanding elsewhere reads as moved
    /// (ADR-0031 §5).
    pub endpoint: String,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl Resolved {
    /// A resolved agent whose id is its name.
    pub fn new(peer: Arc<dyn Peer>, name: impl Into<String>, endpoint: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            peer,
            id: name.clone(),
            name,
            endpoint: endpoint.into(),
        }
    }

    /// Keeps the conversation under `id` rather than the name.
    #[must_use]
    pub fn with_id(self, id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..self
        }
    }

    /// Records `endpoint` with an outstanding task instead.
    #[must_use]
    pub fn with_endpoint(self, endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            ..self
        }
    }
}

/// Why a resolver has no client to hand back.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unresolved {
    /// Not this resolver's. The next one is asked; when none knows the name,
    /// `spawn` says so.
    Unknown,
    /// Known, unreachable, and tried too recently to try again for this long.
    Backoff(Duration),
    /// Known, tried just now, and not reached; tried again after `retry_in`
    /// when the resolver backs off.
    Unreachable {
        /// What went wrong, in words safe to show the model: no server text,
        /// no header value.
        error: String,
        retry_in: Option<Duration>,
    },
    /// Known, and refused for this reason — a complete sentence in the
    /// resolver's words, which `spawn` follows with its own guidance.
    Refused(String),
}

/// What a resolver teaches: the names it can list, and a sentence for the
/// ones it cannot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Taught {
    /// Agents to list, in the resolver's order.
    pub agents: Vec<TaughtAgent>,
    /// One sentence for names the list cannot enumerate, as "`@<team>/<agent>`
    /// reaches any agent in the company registry".
    pub note: Option<String>,
}

impl Taught {
    /// Teaches `agent`, after those already taught.
    #[must_use]
    pub fn with_agent(mut self, agent: TaughtAgent) -> Self {
        self.agents.push(agent);
        self
    }

    /// Teaches `note` about names the list does not hold.
    #[must_use]
    pub fn with_note(self, note: impl Into<String>) -> Self {
        Self {
            note: Some(note.into()),
            ..self
        }
    }
}

/// One remote agent as the model is taught it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TaughtAgent {
    /// The name the model writes after `@`.
    pub name: String,
    /// What its card says, in the words its own card chose — or `None` when
    /// it is configured but could not be reached, so the model is told it
    /// may still ask, and asking retries it.
    pub summary: Option<String>,
}

impl TaughtAgent {
    /// An agent taught by name alone, as one that could not be reached.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            summary: None,
        }
    }

    /// Teaches what the agent's card says.
    #[must_use]
    pub fn with_summary(self, summary: impl Into<String>) -> Self {
        Self {
            summary: Some(summary.into()),
            ..self
        }
    }
}
