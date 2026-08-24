//! Where a connection's sessions come from, and what its client cannot say.
//!
//! A [`ServeConfig`] is a [`SessionSource`] plus the mode its sessions open
//! in, and the source [`ServeConfig::new`] reaches for is
//! [`ConfiguredSource`](super::workspaces::ConfiguredSource) — which is next
//! door rather than here, because since ADR-0018 it is no longer a mapping
//! from a template to a config. It holds the process's runtime and a workspace
//! per directory, which is a lifetime rather than a configuration, and
//! configuration is all this file is.
//!
//! Nothing here answers a request. The handlers read this and never write it,
//! which is why it is `Clone` and holds no lock: every closure in
//! [`serve`](super::serve) gets its own copy.

use std::{path::PathBuf, sync::Arc};

use super::workspaces::ConfiguredSource;
use crate::mode::ApprovalMode;
use basis::{
    BuiltinProvider, Effort, McpServer, ModelSelector, PersistedSession, PreparedRun, RunError,
    ShellAccess, SystemPrompt,
};

/// Where an ACP session's [`PreparedRun`] comes from.
///
/// The same seam as [`prepare_with_session`](basis::run::prepare_with_session),
/// at the protocol layer: a Rust host that already owns a mentra runtime —
/// custom tools, its own store, a provider basis does not know — can serve ACP
/// over it instead of letting basis build one. basis's own tests are the other
/// consumer, driving the whole server against a scripted runtime with no
/// network.
///
/// A source that builds its own runtime owns its tool authorizer too, and a
/// session mode only reaches calls that authorizer surfaces: install
/// [`ApprovalGate`](basis::approval::ApprovalGate) — which is what basis's own
/// source gets from the [`Runtime`](basis::Runtime) it builds — or the
/// client's mode picker will have nothing to decide.
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

    /// Every conversation persisted for `cwd`, most recently used first.
    ///
    /// The order is the source's to decide and is sent on unchanged: a list is
    /// read to find the conversation you were just in, and a source knows what
    /// "just" means for the store it keeps. basis's own answers from
    /// [`store::list`](basis::store::list), which sorts by when a row was last
    /// written.
    ///
    /// Only called when [`lists_sessions`](Self::lists_sessions) is true, so
    /// the default is unreachable rather than a claim about anything.
    async fn list_sessions(&self, cwd: PathBuf) -> Result<Vec<PersistedSession>, RunError> {
        let _ = cwd;
        Ok(Vec::new())
    }

    /// Whether this source can remove a conversation for good.
    ///
    /// Separate from [`lists_sessions`](Self::lists_sessions) because the two
    /// are separate promises: a source can perfectly well enumerate a store it
    /// has no authority to write to, and `session/delete` is advertised only
    /// on the strength of this one. The rule is the same either way — a
    /// capability that answers wrongly is worse than one never claimed.
    fn deletes_sessions(&self) -> bool {
        false
    }

    /// Removes the conversation persisted under `agent_id`, for
    /// `session/delete`.
    ///
    /// No `cwd`, unlike [`create`](Self::create) and
    /// [`list_sessions`](Self::list_sessions), because `session/delete` sends
    /// none — and needs to send none: a store is indexed by conversation, so
    /// the id is the whole of what identifies one.
    ///
    /// Only called when [`deletes_sessions`](Self::deletes_sessions) is true.
    /// Deleting one that is not there must succeed: a client deletes by an id
    /// it read from a list, and "it is gone" is the outcome either way.
    async fn delete(&self, agent_id: &str) -> Result<(), RunError> {
        let _ = agent_id;
        Err(RunError::NoSuchSession)
    }
}

/// What the operator said and a client cannot say for itself: which provider
/// and endpoint, which model, whether commands are granted, the product's own
/// voice, how hard to think, and what to call the sessions.
///
/// The workspace is deliberately absent. Every session brings its own `cwd`
/// on `session/new`, so there is nothing here for it to replace — where the
/// old one-shot config had to carry a placeholder path because its type
/// demanded one.
#[derive(Debug, Clone, Default)]
pub struct SessionTemplate {
    pub(super) provider: Option<BuiltinProvider>,
    pub(super) base_url: Option<String>,
    pub(super) model: Option<ModelSelector>,
    pub(super) shell: ShellAccess,
    pub(super) system_prompt: Option<SystemPrompt>,
    pub(super) effort: Option<Effort>,
    pub(super) session_name: Option<String>,
}

impl SessionTemplate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prefers `provider` over whichever API key the environment offers first.
    pub fn with_provider(self, provider: BuiltinProvider) -> Self {
        Self {
            provider: Some(provider),
            ..self
        }
    }

    /// Points every session at an OpenAI-compatible endpoint.
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            ..self
        }
    }

    pub fn with_model(self, model: ModelSelector) -> Self {
        Self {
            model: Some(model),
            ..self
        }
    }

    /// Grants or denies command execution for every session (ADR-0013).
    pub fn with_shell(self, shell: ShellAccess) -> Self {
        Self { shell, ..self }
    }

    /// The host's say on top of each workspace's own context files.
    pub fn with_system_prompt(self, system_prompt: SystemPrompt) -> Self {
        Self {
            system_prompt: Some(system_prompt),
            ..self
        }
    }

    /// How hard the model should think, where the provider supports it.
    pub fn with_effort(self, effort: Effort) -> Self {
        Self {
            effort: Some(effort),
            ..self
        }
    }

    pub fn with_session_name(self, session_name: impl Into<String>) -> Self {
        Self {
            session_name: Some(session_name.into()),
            ..self
        }
    }
}

/// How a served connection is configured.
///
/// The client supplies the workspace per session (`cwd` on `session/new`), so
/// what belongs here is only what the client cannot say: which model and
/// endpoint to use, whether commands are granted, and which permission mode
/// each session opens in.
///
/// One of these describes a *server*, not a connection: it is cloned into every
/// handler and, on the bridge, into every connection served, so the runtime and
/// workspaces its source holds are the process's (ADR-0018). Building a second
/// one builds a second runtime.
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
    /// Serves sessions built from `template`, each in the `cwd` its client
    /// sent.
    ///
    /// The template's process half — provider, endpoint, model — becomes the
    /// recipe for the one [`Runtime`](basis::Runtime) every session runs on
    /// (ADR-0018), built on the first `session/new` rather than here, so that a
    /// missing credential still reaches the client as `auth_required` rather
    /// than stopping the server from starting.
    ///
    /// Sessions open at [`ApprovalMode::Prompt`] rather than at basis's library
    /// default of allowing everything: over ACP there is a client to ask, which
    /// is the whole reason the protocol carries a permission request. An
    /// operator who wants otherwise says so with
    /// [`with_initial_mode`](Self::with_initial_mode) — the template carries
    /// no opinion about approval at all (ADR-0010).
    pub fn new(template: impl Into<Option<SessionTemplate>>) -> Self {
        Self {
            source: Arc::new(ConfiguredSource::new(template.into())),
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
