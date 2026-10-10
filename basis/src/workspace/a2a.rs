//! A workspace's remote agents, from the outside: what a host reads and what
//! it can ask for (ADR-0029, ADR-0031).
//!
//! Split from [`workspace`](super) for its size, along the seam the feature
//! already draws: everything here exists only with `a2a` on, and the live set
//! itself is [`crate::a2a::live`]'s.

use std::time::Duration;

use mentra::Session;

use super::Workspace;
use crate::{
    a2a::{A2aError, A2aReload, A2aWatch},
    event::ContextFile,
};

impl Workspace {
    /// The remote A2A agents this workspace configures now, by name — each
    /// one a target the model reaches as `spawn "@<name> <task>"` (ADR-0029).
    /// Verbatim: a remote agent is not a tool on the shared registry, so
    /// nothing is claimed and no name is suffixed.
    ///
    /// Every configured agent is listed, reachable or not, so a host can see
    /// what the workspace asked for. Live: after a
    /// [`reload_a2a`](Self::reload_a2a) — or a change the
    /// [`watch_a2a`](Self::watch_a2a) poll picked up — it is the new set.
    /// Names only, for [`mcp_servers`](Self::mcp_servers)'s reason: nothing
    /// here echoes a URL or a credential.
    pub fn a2a_agents(&self) -> Vec<String> {
        self.a2a.names()
    }

    /// The A2A files in effect now, most specific first — what the run header
    /// reports beside [`a2a_agents`](Self::a2a_agents).
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
    /// Must be called from within a Tokio runtime: the poll is a task on it.
    /// A one-shot run has no use for one, and basis starts none on its own.
    ///
    /// [`DEFAULT_A2A_WATCH_INTERVAL`]: crate::a2a::DEFAULT_A2A_WATCH_INTERVAL
    pub fn watch_a2a(&self, interval: Duration) -> A2aWatch {
        self.a2a.watch(interval)
    }

    /// Gives a freshly minted or resumed session the copy of `spawn` bound to
    /// this open's live set of remote agents (ADR-0029 §4, ADR-0031 §3).
    ///
    /// Always, even when the set is empty: the copy's description is read
    /// from the set whenever mentra captures it, so it is the plain `spawn`
    /// until an agent is configured, and an agent added by a reload is taught
    /// to this session from its next model request on.
    ///
    /// Registered on the session's agent alone, which is the isolation: a
    /// sibling open of this directory shares this audience but not this agent,
    /// so it is neither told about these agents nor routed to them.
    pub(super) fn teach_remote_agents(&self, session: &mut Session) {
        let copy = self
            .runtime
            .spawn_tool()
            .for_remote_agents(self.a2a.remote())
            .remembering();
        session.register_tool(copy);
    }
}
