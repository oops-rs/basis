//! A workspace's A2A files, from the outside: what a host reads and what it
//! can ask for (ADR-0031).
//!
//! Split from [`workspace`](super) along the seam the feature draws:
//! everything here exists only with `a2a` on, and the configured set itself
//! is [`crate::a2a::live`]'s. The names an open can reach, and where each
//! conversation stands, are [`super::remote`]'s, feature or no feature.

use std::time::Duration;

use super::Workspace;
use crate::{
    a2a::{A2aError, A2aReload, A2aWatch, A2aWatchError},
    event::ContextFile,
};

#[cfg(doc)]
use crate::PreparedRun;

impl Workspace {
    /// The A2A files in effect now, most specific first — what the run header
    /// reports beside [`remote_agents`](Self::remote_agents).
    pub(super) fn a2a_files(&self) -> Vec<ContextFile> {
        self.a2a.files()
    }

    /// Reads this workspace's A2A configuration again — the host-supplied
    /// agents, the workspace file, the global file, under every rule the open
    /// read them by — and applies the difference to the live set (ADR-0031
    /// §1):
    ///
    /// - an **added** agent is connected;
    /// - a **removed** one is no longer routable, though the conversation a
    ///   session had with it is kept and continues if the name comes back;
    /// - a **changed** one — URL, headers or limits — is reconnected;
    /// - an **unchanged** one has its card fetched again, since a card is the
    ///   remote agent's to change;
    /// - an **unreachable** one is tried again.
    ///
    /// Every agent minted or resumed from this workspace routes — and is
    /// taught — the new set from its next model request on. A delegation
    /// already in flight keeps the connection it started with.
    ///
    /// **A source that fails to read or validate changes nothing.** The error
    /// is returned and the set stays exactly as it was, so an operator saving
    /// a half-typed file never drops an agent. Like the open's, the error
    /// names the file, the agent and the field, never a value read.
    pub async fn reload_a2a(&self) -> Result<A2aReload, A2aError> {
        self.a2a.reload().await
    }

    /// Watches this workspace's A2A files — the workspace's `.basis/a2a.json`
    /// and the global `a2a.json` — and reloads whenever one changes
    /// (ADR-0031 §4), until the returned handle is dropped.
    ///
    /// Polls every `interval` ([`DEFAULT_A2A_WATCH_INTERVAL`] is what basis's
    /// own long-running hosts use) for each file's existence, length and
    /// modification time, so a file that did not exist at open is noticed
    /// when it appears and one deleted is noticed when it goes. Host-supplied
    /// agents have no file and are never reloaded by a watch; a host that
    /// changes its own list says so with [`reload_a2a`](Self::reload_a2a).
    ///
    /// A reload that fails is reported on stderr — naming the file and the
    /// field, never a value — and changes nothing; the poll carries on and
    /// tries again when the file next changes. The watch holds this
    /// workspace's A2A state weakly, so it never keeps a workspace alive.
    ///
    /// The poll is a task on the Tokio runtime this is called from; called on
    /// a thread with none, it is [`A2aWatchError`] rather than a panic. A
    /// one-shot run has no use for a watch, and basis starts none on its own.
    ///
    /// [`DEFAULT_A2A_WATCH_INTERVAL`]: crate::a2a::DEFAULT_A2A_WATCH_INTERVAL
    pub fn watch_a2a(&self, interval: Duration) -> Result<A2aWatch, A2aWatchError> {
        self.a2a.watch(interval)
    }
}
