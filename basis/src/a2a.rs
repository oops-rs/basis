//! Discovery of `a2a.json` — the remote agents a workspace can hand work to.
//!
//! The split is `crate::mcp`'s, and ADR-0028 draws it the same way:
//! mentra owns the client half — fetching an Agent Card, speaking JSON-RPC,
//! sending a task and waiting on it — and basis owns the convention half:
//! which file names an agent, where that file lives, and what its fields mean.
//! The model reaches a named agent through `spawn` (`@<agent> <task>`,
//! ADR-0029); `remote` holds the agents an open reaches, and `spawn` does the
//! talking.
//!
//! # A name is resolved
//!
//! ADR-0037: what this module reads from files is one way to answer
//! `@<name>`. A host may install an [`AgentResolver`](crate::agents::AgentResolver) of its own
//! ([`WorkspaceBuilder::with_agent_resolver`](crate::WorkspaceBuilder::with_agent_resolver)),
//! asked before the configured set; [`crate::agents`] has the contract. Everything
//! below describes the configured set.
//!
//! # The set is live
//!
//! ADR-0031: what an open connected is not fixed for its lifetime, because a
//! host that runs for weeks outlives the configuration it opened with.
//!
//! - **[`Workspace::reload_a2a`](crate::Workspace::reload_a2a)** reads the
//!   same three sources again, under every rule below, and applies the
//!   difference by name — an added agent connects, a removed one stops being
//!   routable, a changed one reconnects, an unchanged one has its card
//!   fetched again — and reports it as an [`A2aReload`]. A source that fails
//!   to read or validate changes nothing: a typo never drops an agent.
//! - **[`Workspace::watch_a2a`](crate::Workspace::watch_a2a)** polls the two
//!   files' paths and reloads on a change, until its [`A2aWatch`] is dropped.
//!   basis's long-running hosts turn it on at [`DEFAULT_A2A_WATCH_INTERVAL`];
//!   a one-shot run does not.
//! - **An unreachable agent is retried when it is asked for**, at most once
//!   per [`A2A_RETRY_BACKOFF`], so one that came up after the open is reached
//!   without a reload.
//!
//! Every agent a workspace mints or resumes holds the live set, so routing
//! follows a change at once and what `spawn` teaches follows it from the next
//! model request on — mentra captures tool descriptions at registration, and
//! basis asks it to capture them again after every change. A delegation
//! already in flight keeps the connection it started with.
//!
//! # The format is basis's
//!
//! Where `.mcp.json` is a format other agents already wrote, this one is
//! basis's own, so it lives under `.basis/` beside `config.json` and
//! `tools.json` and is read the way they are: an unknown key is refused
//! rather than tolerated, because nobody else writes this file and a key basis
//! does not know is a typo — a misspelled `headers` is an agent called without
//! the credential its operator wrote down.
//!
//! ```json
//! {"agents": {"nous": {"url": "https://nous.example.com", "headers": {"Authorization": "Bearer ${NOUS_TOKEN}"}}}}
//! ```
//!
//! `url` is a base URL (the card is fetched from
//! `/.well-known/agent-card.json` on that origin) or the card's own URL —
//! mentra's rule, stated on [`A2aAgentConfig`].
//!
//! # Every rule `.mcp.json` follows, unchanged
//!
//! - **Three places, and no fourth.** Supplied by the host, then the
//!   workspace file, then the global one, by name; the first source to name an
//!   agent wins it outright rather than merging with a weaker one.
//! - **No parent walk**, for `.mcp.json`'s reason and more urgently: this file
//!   names a host to send work to and the credential to send it with, and
//!   inheriting one from a directory the operator did not point basis at
//!   means handing that directory's agent this repository's work.
//! - **`${VAR}` expands** in `url` and in every header value, by
//!   the rules `.mcp.json` and `.basis/tools.json` expand by.
//! - **A name may not contain `__` or end in `_`** — mentra's
//!   [`validate_a2a_agent_name`](mentra::a2a::validate_a2a_agent_name), which
//!   is deliberately the MCP server-name rule.
//! - **A file that exists but names no `agents` is an error**, because the
//!   alternative is that a typo disables every agent and says nothing.
//! - **A committed file cannot grant its headers plaintext passage** to a
//!   non-loopback host. mentra's `allow_plaintext_credentials` override is not
//!   a key here; it stays with a host that builds an [`A2aAgentConfig`] in
//!   code.
//!
//! Nothing read here is ever repeated back: see [`A2aError`].

pub(crate) mod configured;
mod file;
pub(crate) mod live;
mod reload;
pub(crate) mod remote;
mod watch;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::context::ContextScope;

pub use configured::A2A_RETRY_BACKOFF;
pub use mentra::a2a::{A2aAgentConfig, A2aLimits};
pub use mentra::mcp::SecretString;
pub use reload::A2aReload;
pub use watch::{A2aWatch, A2aWatchError, DEFAULT_A2A_WATCH_INTERVAL};

/// The URI of basis's hops extension (`docs/a2a/hops-v1.md`, ADR-0030 §9):
/// a message carries `{"hop", "deadline"}` under it in its metadata, so a
/// loop between agents ends at a hop cap or the first deadline.
///
/// Both halves are basis's: `spawn` sends it on every `@<agent>` message, and
/// `basis-a2a` reads it from every message it serves.
pub const HOPS_EXTENSION: &str = "https://github.com/oops-rs/basis/blob/main/docs/a2a/hops-v1.md";

/// Where basis looks inside a workspace, relative to its root.
pub const DEFAULT_WORKSPACE_A2A_FILE: &str = ".basis/a2a.json";

/// Where basis looks inside the global config directory. Undotted, for the
/// reason every file there is: a hidden file inside a directory that exists
/// to hold configuration would be hiding it from the person who put it there.
pub const DEFAULT_GLOBAL_A2A_FILE: &str = "a2a.json";

/// One remote agent, as basis hands it to mentra.
///
/// A newtype over mentra's [`A2aAgentConfig`] rather than a type of basis's
/// own, because mentra owns what an agent configuration *is*. It exists for
/// one property: a `Debug` that is safe wherever it is nested. mentra's own
/// derives print `url` whole, and by the time a configuration reaches this
/// type basis has already expanded `${NOUS_TOKEN}` into it — a query string
/// is where such a credential sits — while [`A2aConfig`] and [`A2aSource`],
/// which hold these, derive `Debug` and are exactly what a host logs.
#[derive(Clone)]
pub struct A2aAgent(A2aAgentConfig);

impl A2aAgent {
    pub fn new(config: A2aAgentConfig) -> Self {
        Self(config)
    }

    /// The agent's name, which is also how `spawn` addresses it:
    /// `@{name} <task>`.
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// mentra's configuration, for a caller that needs the concrete type.
    pub fn config(&self) -> &A2aAgentConfig {
        &self.0
    }

    pub fn into_config(self) -> A2aAgentConfig {
        self.0
    }

    /// Which endpoint this configuration reaches, as a short fingerprint — what
    /// an outstanding task records, so a later call can tell that the agent
    /// moved since the task was handed over (ADR-0031 §5).
    ///
    /// The name, the URL and the header *names*, FNV-1a hashed: stable across
    /// processes and Rust releases, because the record it goes into is read
    /// back after a restart. Header values are left out on purpose — a
    /// credential rotated at the same endpoint does not move a task, and a
    /// value, even hashed, has no business in a file beside the transcript.
    pub(crate) fn endpoint(&self) -> String {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let parts = [self.0.name.as_str(), self.0.url.as_str()]
            .into_iter()
            .chain(self.0.headers.keys().map(String::as_str));
        let mut hash = OFFSET;
        for part in parts {
            // A separator no part can contain, so `a`+`bc` and `ab`+`c` differ.
            for byte in part.bytes().chain([0xff]) {
                hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
            }
        }
        format!("{hash:016x}")
    }

    /// Whether `other` reaches the same agent the same way: the same name,
    /// URL, headers, plaintext override and limits. What a reload asks to
    /// decide between fetching a card again and reconnecting (ADR-0031 §1).
    ///
    /// Compared field by field rather than through a derived `PartialEq`,
    /// because mentra's configuration derives none — and so that a field
    /// mentra adds later is a compile error here rather than a difference
    /// quietly ignored.
    pub(crate) fn connects_like(&self, other: &Self) -> bool {
        let A2aAgentConfig {
            name,
            url,
            headers,
            allow_plaintext_credentials,
            limits,
        } = &self.0;
        *name == other.0.name
            && *url == other.0.url
            && *headers == other.0.headers
            && *allow_plaintext_credentials == other.0.allow_plaintext_credentials
            && *limits == other.0.limits
    }
}

impl From<A2aAgentConfig> for A2aAgent {
    fn from(config: A2aAgentConfig) -> Self {
        Self(config)
    }
}

/// Hand-written for the reason the type exists. Header values need no help —
/// mentra types them as [`SecretString`], which redacts itself — but `url`
/// is a plain `String` and goes through `redaction::redacted_url`. Header
/// *names* survive, because naming `Authorization` is what makes a
/// misconfiguration fixable.
impl std::fmt::Debug for A2aAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAgent")
            .field("name", &self.0.name)
            .field("url", &crate::redaction::redacted_url(&self.0.url))
            .field("headers", &self.0.headers)
            .field(
                "allow_plaintext_credentials",
                &self.0.allow_plaintext_credentials,
            )
            .field("limits", &self.0.limits)
            .finish()
    }
}

/// Which remote agents a workspace gets: where to look for configured ones,
/// and any the host already has in hand.
#[derive(Debug, Clone)]
pub struct A2aConfig {
    /// Path relative to the workspace root. Empty names no file to look for;
    /// [`A2aConfig::supplied_only`] writes it.
    pub workspace_file: PathBuf,
    /// The global config directory, if any. `a2a.json` inside it is used.
    pub global_dir: Option<PathBuf>,
    /// Agents the host supplies directly, outranking anything on disk of the
    /// same name.
    pub supplied: Vec<A2aAgent>,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            workspace_file: PathBuf::from(DEFAULT_WORKSPACE_A2A_FILE),
            global_dir: crate::context::default_global_dir(),
            supplied: Vec::new(),
        }
    }
}

impl A2aConfig {
    /// Adds agents the host already holds, replacing any set before.
    pub fn with_supplied(self, supplied: Vec<A2aAgent>) -> Self {
        Self { supplied, ..self }
    }

    /// The host's own agents, and no file discovery at all: neither
    /// `.basis/a2a.json` nor the global one is read.
    ///
    /// What `WorkspaceBuilder::without_discovery` leaves of this config, for
    /// `McpConfig::supplied_only`'s reason: being handed an agent is not
    /// finding one.
    pub fn supplied_only(self) -> Self {
        Self {
            workspace_file: PathBuf::new(),
            global_dir: None,
            ..self
        }
    }
}

/// One `a2a.json` that exists on disk, and what it configured.
#[derive(Debug, Clone)]
pub struct A2aSource {
    pub path: PathBuf,
    pub scope: ContextScope,
    pub agents: Vec<A2aAgent>,
}

/// Anything that can go wrong turning configuration into agents.
///
/// `McpError`'s rule, for its reason: these messages
/// travel into `basis spawn --json`, an ACP client's error pane and whatever a
/// host logs, and a `headers` value is a credential — already expanded by the
/// time it could be repeated. A message may name the file, the agent, the
/// field, a header's name and an environment variable, and nothing else it
/// read. [`Parse`](Self::Parse) keeps a location and a category instead of
/// serde's own message, which quotes the value it choked on; an unknown key is
/// therefore reported as a place in the file rather than as the word that was
/// wrong.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum A2aError {
    #[error("failed to read A2A configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not a valid A2A configuration: {problem} at line {line}, column {column}")]
    Parse {
        path: PathBuf,
        problem: &'static str,
        line: usize,
        column: usize,
    },

    #[error("{path} has no `agents` object")]
    NoAgents { path: PathBuf },

    #[error("{origin}: A2A agent `{name}` {reason}")]
    Invalid {
        origin: String,
        name: String,
        reason: String,
    },

    /// A forgotten conversation's record of the remote work it handed off
    /// could not be removed ([`crate::store::forget`]).
    #[error("failed to remove remembered A2A conversations {path}: {source}")]
    Forget {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Every `a2a.json` that exists, most specific first.
///
/// A missing file is not an error — most workspaces name no agents. A file
/// that exists and cannot be read or understood is, because the operator
/// wrote it meaning something.
pub fn discover(workspace: &Path, config: &A2aConfig) -> Result<Vec<A2aSource>, A2aError> {
    let mut sources = Vec::new();

    if let Some(workspace_file) = crate::paths::candidate(workspace, &config.workspace_file)
        && workspace_file.is_file()
    {
        sources.push(read(workspace_file, ContextScope::Workspace)?);
    }

    if let Some(global) = &config.global_dir {
        let global_file = global.join(DEFAULT_GLOBAL_A2A_FILE);
        // The same file reached twice is one source, not two: bridging its
        // agents again would collide with itself in the tool roster.
        if global_file.is_file()
            && !sources
                .iter()
                .any(|source| crate::paths::same_dir(&source.path, &global_file))
        {
            sources.push(read(global_file, ContextScope::Global)?);
        }
    }

    Ok(sources)
}

/// Every agent a workspace should connect, strongest source first, one per
/// name.
pub fn agents(workspace: &Path, config: &A2aConfig) -> Result<Vec<A2aAgent>, A2aError> {
    Ok(layered(supplied(config)?, discover(workspace, config)?))
}

/// [`agents`] over sources a caller already discovered, so the workspace open
/// reads each file once while still reporting which ones took effect.
pub(crate) fn layered(supplied: Vec<A2aAgent>, discovered: Vec<A2aSource>) -> Vec<A2aAgent> {
    layer(
        supplied
            .into_iter()
            .chain(discovered.into_iter().flat_map(|source| source.agents)),
    )
}

/// The host-supplied agents, checked as a file's entries are.
///
/// A file's entries are validated as they are parsed; a host's own list has no
/// door of its own, and nothing stops it building an [`A2aAgentConfig`] that
/// mentra would refuse. Checking it here — the whole of mentra's
/// [`validate`](A2aAgentConfig::validate), not just the name — makes a bad
/// supplied agent refuse the open loudly, as a bad file entry does, rather
/// than degrade into a connect-time warning an ACP client never sees.
pub(crate) fn supplied(config: &A2aConfig) -> Result<Vec<A2aAgent>, A2aError> {
    for agent in &config.supplied {
        agent
            .config()
            .validate()
            .map_err(|error| A2aError::Invalid {
                origin: "the supplied A2A agent list".to_string(),
                name: agent.name().to_string(),
                reason: file::refused(&error),
            })?;
    }

    Ok(layer(config.supplied.iter().cloned()))
}

/// Keeps the first agent seen under each name.
///
/// Callers pass strongest-first, so "first wins" is "most specific wins".
/// Configuration order is otherwise kept, so the connect order does not
/// depend on hashing.
fn layer(agents: impl IntoIterator<Item = A2aAgent>) -> Vec<A2aAgent> {
    let mut seen = std::collections::BTreeSet::new();
    agents
        .into_iter()
        .filter(|agent| seen.insert(agent.name().to_string()))
        .collect()
}

fn read(path: PathBuf, scope: ContextScope) -> Result<A2aSource, A2aError> {
    let text = std::fs::read_to_string(&path).map_err(|source| A2aError::Read {
        path: path.clone(),
        source,
    })?;

    let agents = file::parse(&path, &text)?;

    Ok(A2aSource {
        path,
        scope,
        agents,
    })
}

#[cfg(test)]
mod tests;
