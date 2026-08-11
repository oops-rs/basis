//! Where a connection's sessions come from, and what its client cannot say.
//!
//! One module for both halves because they are one decision: a [`ServeConfig`]
//! is a [`SessionSource`] plus the mode its sessions open in, and
//! [`ConfiguredSource`] — the source lan builds when the caller supplies a
//! [`RunConfig`] rather than a runtime of its own — is what
//! [`ServeConfig::new`] reaches for. A constructor and the only thing it
//! constructs do not belong on opposite sides of a file boundary.
//!
//! Nothing here answers a request. The handlers read this and never write it,
//! which is why it is `Clone` and holds no lock: every closure in
//! [`serve`](super::serve) gets its own copy.

use std::{path::PathBuf, sync::Arc};

use crate::mode::ApprovalMode;
use lan_core::{McpServer, PersistedSession, PreparedRun, RunConfig, RunError};

/// Where an ACP session's [`PreparedRun`] comes from.
///
/// The same seam as [`prepare_with_session`](lan_core::run::prepare_with_session),
/// at the protocol layer: a Rust host that already owns a mentra runtime —
/// custom tools, its own store, a provider lan does not know — can serve ACP
/// over it instead of letting lan build one. lan's own tests are the other
/// consumer, driving the whole server against a scripted runtime with no
/// network.
///
/// A source that builds its own runtime owns its tool authorizer too, and a
/// session mode only reaches calls that authorizer surfaces: install
/// [`ApprovalGate`](lan_core::approval::ApprovalGate) — which is what lan's own
/// source gets from [`prepare_without_prompt`](lan_core::run::prepare_without_prompt)
/// — or the client's mode picker will have nothing to decide.
#[async_trait::async_trait]
pub trait SessionSource: Send + Sync + 'static {
    /// Opens a conversation in `cwd`, for `session/new`, with the MCP servers
    /// the client configured for this session.
    async fn create(&self, cwd: PathBuf, mcp: Vec<McpServer>) -> Result<PreparedRun, RunError>;

    /// Picks up the conversation persisted under `agent_id`, for
    /// `session/load`. The default refuses, which is the honest answer for a
    /// source whose sessions do not outlive the process.
    async fn resume(
        &self,
        agent_id: &str,
        cwd: PathBuf,
        mcp: Vec<McpServer>,
    ) -> Result<PreparedRun, RunError> {
        let _ = (agent_id, cwd, mcp);
        Err(RunError::NoSuchSession)
    }

    /// Whether this source can enumerate the conversations it has persisted.
    ///
    /// `session/list` is advertised and answered only when this is true. A
    /// source that keeps no registry would otherwise report "no sessions" for
    /// a workspace that has some, and a capability that answers wrongly is
    /// worse than one that was never claimed — an unregistered method at least
    /// says so, with `-32601`.
    fn lists_sessions(&self) -> bool {
        false
    }

    /// Every conversation persisted for `cwd`, oldest first.
    ///
    /// Only called when [`lists_sessions`](Self::lists_sessions) is true, so
    /// the default is unreachable rather than a claim about anything.
    async fn list_sessions(&self, cwd: PathBuf) -> Result<Vec<PersistedSession>, RunError> {
        let _ = cwd;
        Ok(Vec::new())
    }
}

/// The default source: build a runtime per session from a [`RunConfig`].
pub(super) struct ConfiguredSource {
    pub(super) template: Option<RunConfig>,
}

impl ConfiguredSource {
    /// Builds the config for one session, in the client's working directory.
    ///
    /// Nothing here says anything about approval. A runtime's authorizer is
    /// fixed for its life, so lan-core installs one that surfaces every
    /// consequential call and answers none of them; which of those the client
    /// actually sees is the session's mode, which can still change (see
    /// [`mode`](crate::mode)).
    pub(super) fn config_for(&self, cwd: PathBuf, mcp: Vec<McpServer>) -> RunConfig {
        let config = match &self.template {
            Some(template) => {
                let mut config = template.clone();
                config.workspace = cwd;
                config
            }
            None => RunConfig::new(cwd, ""),
        };

        // The client's servers outrank the workspace's own: it is answering
        // for this session in particular. Discovery still runs, so a
        // `.mcp.json` the client said nothing about is still honored.
        let mcp = config.mcp.clone().with_supplied(mcp);
        config.with_mcp(mcp)
    }
}

#[async_trait::async_trait]
impl SessionSource for ConfiguredSource {
    async fn create(&self, cwd: PathBuf, mcp: Vec<McpServer>) -> Result<PreparedRun, RunError> {
        lan_core::run::prepare_without_prompt(self.config_for(cwd, mcp)).await
    }

    async fn resume(
        &self,
        agent_id: &str,
        cwd: PathBuf,
        mcp: Vec<McpServer>,
    ) -> Result<PreparedRun, RunError> {
        lan_core::run::resume(agent_id, self.config_for(cwd, mcp)).await
    }

    fn lists_sessions(&self) -> bool {
        true
    }

    /// Reads mentra's store directly. Building a session to enumerate sessions
    /// would resolve a model over the network to answer a question about a
    /// SQLite table.
    ///
    /// This depends on
    /// [`WorkspaceBuilder::open`](lan_core::WorkspaceBuilder::open) tagging each
    /// conversation with
    /// [`store::runtime_identifier`](lan_core::store::runtime_identifier) for
    /// its workspace. Until it did, conversations were written under mentra's
    /// `"default"` tag and no workspace's list found any of them.
    async fn list_sessions(&self, cwd: PathBuf) -> Result<Vec<PersistedSession>, RunError> {
        lan_core::store::list(&cwd)
    }
}

/// How a served connection is configured.
///
/// The client supplies the workspace per session (`cwd` on `session/new`), so
/// what belongs here is only what the client cannot say: which model and
/// endpoint to use, whether commands are granted, and which permission mode
/// each session opens in.
#[derive(Clone)]
pub struct ServeConfig {
    pub(super) source: Arc<dyn SessionSource>,
    /// Where a new session's mode picker starts.
    pub(super) initial_mode: ApprovalMode,
}

impl std::fmt::Debug for ServeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeConfig")
            .field("initial_mode", &self.initial_mode)
            .finish_non_exhaustive()
    }
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ServeConfig {
    /// Serves sessions built from `template`, whose workspace each session
    /// replaces with the `cwd` its client sent.
    ///
    /// Sessions open at [`ApprovalMode::Prompt`] rather than at lan's library
    /// default of allowing everything: over ACP there is a client to ask, which
    /// is the whole reason the protocol carries a permission request. An
    /// operator who wants otherwise says so with
    /// [`with_initial_mode`](Self::with_initial_mode) — the template cannot
    /// carry it, because a [`RunConfig`] no longer has an opinion about
    /// approval to carry (ADR-0010).
    pub fn new(template: impl Into<Option<RunConfig>>) -> Self {
        Self {
            source: Arc::new(ConfiguredSource {
                template: template.into(),
            }),
            initial_mode: ApprovalMode::default(),
        }
    }

    /// Serves sessions the caller supplies.
    pub fn with_source(source: impl SessionSource) -> Self {
        Self {
            source: Arc::new(source),
            initial_mode: ApprovalMode::default(),
        }
    }

    /// Opens each session in `mode` instead of asking every time.
    pub fn with_initial_mode(self, mode: ApprovalMode) -> Self {
        Self {
            initial_mode: mode,
            ..self
        }
    }
}
