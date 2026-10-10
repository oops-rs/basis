//! One workspace open's A2A configuration, kept so it can be read again.
//!
//! The open reads the three sources once and connects what they name; a
//! reload (ADR-0031 §1) reads the same three sources the same way — the same
//! [`discovered`] the open calls, so the rules cannot drift apart — and hands
//! the result to the live set. What the run header reports about A2A, the
//! files in effect and the names configured, is read from here at the moment
//! it is reported, so it follows the set rather than the open.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use super::{
    A2aAgent, A2aConfig, A2aError, A2aReload,
    remote::{OnChange, RemoteAgents},
};
use crate::{event::ContextFile, runtime::Runtime};

/// What a workspace open knows about its remote agents: where it looked, what
/// it found there, and the live set it connected.
pub(crate) struct LiveA2a {
    root: PathBuf,
    config: A2aConfig,
    /// The files that took effect on the last successful read.
    files: RwLock<Vec<ContextFile>>,
    remote: Arc<RemoteAgents>,
    /// Held across a whole reload — reading, connecting, applying — so two
    /// reloads, a host's and the watch's say, apply one after the other
    /// rather than each diffing against a set the other is about to replace.
    reloading: tokio::sync::Mutex<()>,
}

impl LiveA2a {
    /// Connects what [`discovered`] found on `runtime`, and keeps what it
    /// took to find it.
    pub(crate) async fn connect(
        root: PathBuf,
        config: A2aConfig,
        (files, agents): (Vec<ContextFile>, Vec<A2aAgent>),
        runtime: &Arc<Runtime>,
    ) -> Self {
        let remembered_in = runtime.a2a_conversations_dir().map(Path::to_path_buf);
        let remote = RemoteAgents::connect(agents, remembered_in)
            .await
            .with_on_change(refresher(runtime));
        Self {
            root,
            config,
            files: RwLock::new(files),
            remote: Arc::new(remote),
            reloading: tokio::sync::Mutex::new(()),
        }
    }

    /// The live set, as every agent minted or resumed from this open holds it.
    pub(crate) fn remote(&self) -> &Arc<RemoteAgents> {
        &self.remote
    }

    /// Every configured name, in configuration order.
    pub(crate) fn names(&self) -> Vec<String> {
        self.remote.names()
    }

    /// The files in effect, most specific first.
    pub(crate) fn files(&self) -> Vec<ContextFile> {
        self.files
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Reads the sources again and applies the difference.
    ///
    /// A source that fails to read or validate is returned as the error before
    /// anything is applied, so the set — and the files reported — stay exactly
    /// as they were: a half-typed file never drops an agent (ADR-0031 §1).
    pub(crate) async fn reload(&self) -> Result<A2aReload, A2aError> {
        let _serial = self.reloading.lock().await;
        let (files, agents) = discovered(&self.root, &self.config)?;
        let report = self.remote.reload(agents).await;
        *self
            .files
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = files;
        Ok(report)
    }
}

/// Asks mentra to capture every registered tool's description again, so the
/// next model request of every agent holding the set is told the set as it
/// is (ADR-0031 §3).
///
/// Weak, because the set is held by the `spawn` copies mentra's registry owns:
/// a strong runtime here would be a cycle through that registry, and the
/// runtime would never drop — the reason `spawn` reads the agent ledger
/// through its own `Arc` rather than the runtime's.
fn refresher(runtime: &Arc<Runtime>) -> OnChange {
    let runtime = Arc::downgrade(runtime);
    Box::new(move || {
        if let Some(runtime) = runtime.upgrade() {
            runtime.mentra_runtime().refresh_tool_descriptors();
        }
    })
}

/// Discovers the remote agents a workspace can reach, and which files said
/// so — what both the open and every reload read.
///
/// Each file is read once: the sources are what the header names, and the
/// same sources are layered under the host's own list into what connects.
pub(crate) fn discovered(
    workspace: &Path,
    config: &A2aConfig,
) -> Result<(Vec<ContextFile>, Vec<A2aAgent>), A2aError> {
    let supplied = super::supplied(config)?;
    let sources = super::discover(workspace, config)?;
    let files = sources
        .iter()
        .map(|source| ContextFile {
            path: source.path.clone(),
            scope: source.scope.label(),
        })
        .collect();

    Ok((files, super::layered(supplied, sources)))
}

/// Names and files only, for [`super::A2aAgent`]'s reason: the config holds
/// expanded credentials.
impl std::fmt::Debug for LiveA2a {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveA2a")
            .field("files", &self.files())
            .field("agents", &self.remote)
            .finish_non_exhaustive()
    }
}
