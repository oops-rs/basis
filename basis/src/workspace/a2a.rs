//! A workspace's remote agents, from the outside: what a host reads and what
//! it can ask for (ADR-0029, ADR-0031).
//!
//! Split from [`workspace`](super) for its size, along the seam the feature
//! already draws: everything here exists only with `a2a` on, and the live set
//! itself is [`crate::a2a::live`]'s.

use mentra::Session;

use super::Workspace;
use crate::{
    a2a::{A2aError, A2aReload},
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

    /// Gives a freshly minted or resumed session the copy of `spawn` that
    /// knows this open's remote agents (ADR-0029 §4) — or nothing, when none
    /// answered at open, so its model keeps the `spawn` every agent sees.
    ///
    /// Registered on the session's agent alone, which is the isolation: a
    /// sibling open of this directory shares this audience but not this agent,
    /// so it is neither told about these agents nor routed to them.
    pub(super) fn teach_remote_agents(&self, session: &mut Session) {
        if let Some(copy) = self
            .runtime
            .spawn_tool()
            .for_remote_agents(self.a2a.remote())
            .map(crate::tools::SpawnTool::remembering)
        {
            session.register_tool(copy);
        }
    }
}
