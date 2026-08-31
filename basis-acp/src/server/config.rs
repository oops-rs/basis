//! Where a connection's sessions come from, and what its client cannot say.
//!
//! A [`ServeConfig`] is a [`SessionSource`] plus the mode its sessions open
//! in, and the source [`ServeConfig::new`] reaches for is
//! [`ConfiguredSource`](basis_host::ConfiguredSource), which holds the
//! process's runtime and a workspace per directory.
//!
//! Nothing here answers a request. The handlers read this and never write it,
//! which is why it is `Clone` and holds no lock: every closure in
//! [`serve`](super::serve) gets its own copy.

use std::sync::Arc;

use crate::mode::ApprovalMode;
use basis_host::ConfiguredSource;

pub use basis_host::{Discovery, SessionSource, SessionTemplate};

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
