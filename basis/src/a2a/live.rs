//! One workspace open's A2A configuration, kept so it can be read again.
//!
//! The open reads the three sources once and connects what they name; a
//! reload (ADR-0031 §1) reads the same three sources the same way — the same
//! [`discovered`] the open calls, so the rules cannot drift apart — and hands
//! the result to the live set. What the run header reports about A2A, the
//! files in effect and the names configured, is read from here at the moment
//! it is reported, so it follows the set rather than the open.
//!
//! Every read is stamped first — each source path's existence, length and
//! modification time ([`super::watch`]) — so a watch can tell whether
//! anything changed since the last read without reading anything itself.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use super::{
    A2aAgent, A2aConfig, A2aError, A2aReload, DEFAULT_GLOBAL_A2A_FILE,
    configured::ConfiguredAgents,
    watch::{self, A2aWatch, A2aWatchError, Stamp},
};
use crate::{agents::AgentResolver, event::Binding};
use crate::{event::ContextFile, runtime::Runtime};

/// What a workspace open knows about its remote agents: where it looked, what
/// it found there, and the live set it connected.
pub(crate) struct LiveA2a {
    root: PathBuf,
    config: A2aConfig,
    /// The files that can configure an agent, whether or not they exist —
    /// what a watch polls. Host-supplied agents have no file.
    paths: Vec<PathBuf>,
    /// What [`paths`](Self::paths) looked like just before the last read,
    /// successful or not.
    seen: Mutex<Vec<Stamp>>,
    /// The files that took effect on the last successful read.
    files: RwLock<Vec<ContextFile>>,
    /// The resolver over what the files configure — what a reload applies
    /// to, and what the open chains after the host's own (ADR-0037 §1).
    configured: Arc<ConfiguredAgents>,
    /// Held across a whole reload — reading, connecting, applying — so two
    /// reloads, a host's and the watch's say, apply one after the other
    /// rather than each diffing against a set the other is about to replace.
    reloading: tokio::sync::Mutex<()>,
}

impl LiveA2a {
    /// Connects what [`discovered`] found on `runtime`, and keeps what it
    /// took to find it. The configured set is told to refresh descriptions
    /// when it changes (ADR-0031 §3).
    pub(crate) async fn connect(
        root: PathBuf,
        config: A2aConfig,
        discovery: Discovery,
        runtime: &Arc<Runtime>,
    ) -> Self {
        let configured = Arc::new(ConfiguredAgents::connect(discovery.agents).await);
        configured.on_change(runtime.refresher());
        Self {
            paths: sources(&root, &config),
            seen: Mutex::new(discovery.seen),
            root,
            config,
            files: RwLock::new(discovery.files),
            configured,
            reloading: tokio::sync::Mutex::new(()),
        }
    }

    /// The resolver over the configured set, for the open to chain after the
    /// host's own.
    pub(crate) fn configured(&self) -> Arc<dyn AgentResolver> {
        Arc::clone(&self.configured) as Arc<dyn AgentResolver>
    }

    /// This binding's entry in a run header (ADR-0039), as it stands now:
    /// the files in effect and the configured names.
    pub(crate) fn binding(&self) -> Binding {
        Binding::new("a2a").with_files(self.files()).with_names(
            self.configured
                .taught()
                .agents
                .into_iter()
                .map(|a| a.name)
                .collect(),
        )
    }

    /// The same, read again each time a header is written. Weak, because
    /// the run outlives the workspace and must not keep its open alive; a
    /// dropped open reports what it last reported.
    pub(crate) fn binding_source(self: &Arc<Self>) -> crate::run::BindingSource {
        let live = Arc::downgrade(self);
        let last = std::sync::Mutex::new(self.binding());
        Arc::new(move || {
            let mut last = last.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(live) = live.upgrade() {
                *last = live.binding();
            }
            last.clone()
        })
    }

    /// Every configured name, in configuration order.
    #[cfg(test)]
    pub(crate) fn names(&self) -> Vec<String> {
        self.configured.names()
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
        let discovery = match discovered(&self.root, &self.config) {
            Ok(discovery) => discovery,
            // Recorded at once for a read that failed: there is nothing to
            // apply, and a watch should try a broken file again when it
            // changes, not on every poll.
            Err((seen, error)) => {
                self.saw(seen);
                return Err(error);
            }
        };

        let report = self.configured.reload(discovery.agents).await;
        *self
            .files
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = discovery.files;
        // Recorded only once the set is swapped: a reload cancelled while it
        // connected — a host's timeout, a watch dropped mid-reload — applied
        // nothing, so the edit it read must still look unseen to the next
        // poll rather than be skipped for good.
        self.saw(discovery.seen);
        Ok(report)
    }

    fn saw(&self, seen: Vec<Stamp>) {
        *self
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = seen;
    }

    /// What the sources looked like just before the last read.
    pub(crate) fn seen(&self) -> Vec<Stamp> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Starts polling this open's A2A files every `interval`, reloading on a
    /// change. See [`Workspace::watch_a2a`](crate::Workspace::watch_a2a).
    pub(crate) fn watch(self: &Arc<Self>, interval: Duration) -> Result<A2aWatch, A2aWatchError> {
        watch::start(Arc::downgrade(self), self.paths.clone(), interval)
    }
}

/// What one read of the sources found, and what they looked like just before.
pub(crate) struct Discovery {
    files: Vec<ContextFile>,
    agents: Vec<A2aAgent>,
    seen: Vec<Stamp>,
}

/// The files that can configure an agent for `workspace`, existing or not, in
/// [`super::discover`]'s order.
fn sources(workspace: &Path, config: &A2aConfig) -> Vec<PathBuf> {
    crate::paths::candidate(workspace, &config.workspace_file)
        .into_iter()
        .chain(
            config
                .global_dir
                .as_ref()
                .map(|global| global.join(DEFAULT_GLOBAL_A2A_FILE)),
        )
        .collect()
}

/// Discovers the remote agents a workspace can reach, and which files said
/// so — what both the open and every reload read. Stamps the sources first,
/// and hands the stamps back with a failure too.
///
/// Each file is read once: the sources are what the header names, and the
/// same sources are layered under the host's own list into what connects.
pub(crate) fn discovered(
    workspace: &Path,
    config: &A2aConfig,
) -> Result<Discovery, (Vec<Stamp>, A2aError)> {
    let seen = watch::stamps(&sources(workspace, config));
    let read = || -> Result<_, A2aError> {
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
    };
    match read() {
        Ok((files, agents)) => Ok(Discovery {
            files,
            agents,
            seen,
        }),
        Err(error) => Err((seen, error)),
    }
}

/// Names and files only, for [`super::A2aAgent`]'s reason: the config holds
/// expanded credentials.
impl std::fmt::Debug for LiveA2a {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveA2a")
            .field("files", &self.files())
            .field("agents", &self.configured)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
