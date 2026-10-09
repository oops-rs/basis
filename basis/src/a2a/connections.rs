//! The remote agents a workspace bridged, on a runtime it may share.
//!
//! The MCP connections' shape (`crate::mcp::connections`), for ADR-0018's
//! reason: a bridged agent is minted from repository config and dies with the
//! workspace, so the runtime never holds a bridge whose config it cannot see.
//! On the runtime's single tool registry:
//!
//! - each agent's name is **claimed** first ([`Runtime::claim_bridged`] with
//!   [`Bridge::A2a`]), and a name another workspace holds comes back suffixed
//!   — the effective name is what the tool is called (`a2a__{effective}`) and
//!   what [`Workspace::a2a_agents`](crate::Workspace::a2a_agents) reports;
//! - the tool is registered for this workspace's audience alone, so a
//!   workspace in another directory can neither list nor call it;
//! - what took is recorded beside the claim ([`Runtime::record_bridged_tools`]),
//!   which is what [`Workspace`](crate::Workspace)'s mint hides from a second
//!   live open of *this* directory that did not name the agent — one
//!   directory is one audience, and the tool carries this open's credential;
//! - the registration and the claim come off when the workspace drops.
//!
//! An agent that cannot be reached degrades as an MCP server does: a warning
//! names it, the open continues, and the name is still reported, so a client
//! can see which agents are configured whether or not they came up. Its card
//! is fetched once, here; nothing retries it later.
//!
//! Nothing here needs shutting down: mentra's client holds no session and no
//! background task, so dropping the registration — which drops the last
//! `Arc` to the client — is the whole of closing it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mentra::a2a::{A2aAgentTool, A2aClient};
use mentra::tool::{AudienceToolRegistration, ToolAudience};

use crate::runtime::{Bridge, Runtime};

use super::A2aAgent;

/// One workspace's bridged agents: the names claimed on the shared registry
/// and mentra's holds on the tools registered under them.
pub(crate) struct A2aConnections {
    runtime: Arc<Runtime>,
    /// Claimed effective names, released on drop.
    claimed: Vec<String>,
    /// mentra's holds on the registered tools, which are what keep them
    /// answering: dropping one takes its tool off the registry. Not derivable
    /// from `claimed`, because an agent that failed to connect registered
    /// nothing.
    registered: Vec<AudienceToolRegistration>,
    /// The claim owner; only this root can release its names.
    root: PathBuf,
}

impl A2aConnections {
    /// Connects `agents` for the workspace at `root`, registering each one's
    /// tool under the claimed name.
    pub(crate) async fn connect(
        runtime: Arc<Runtime>,
        audience: &ToolAudience,
        root: &Path,
        agents: Vec<A2aAgent>,
    ) -> Self {
        let mut claimed = Vec::new();
        let mut registered = Vec::new();

        for agent in agents {
            let effective = runtime.claim_bridged(Bridge::A2a, agent.name(), root);
            claimed.push(effective.clone());

            // The claimed name is written into the config before connecting,
            // because the client names its tool by it.
            let mut config = agent.into_config();
            config.name = effective.clone();

            match A2aClient::connect(&config).await {
                Ok(client) => {
                    if let Some(registration) =
                        register(&runtime, audience, A2aAgentTool::new(Arc::new(client)))
                    {
                        // What took, not what was offered, for the MCP
                        // connections' reason: a name this workspace does not
                        // actually serve must not be hidden from a sibling
                        // that legitimately does.
                        runtime.record_bridged_tools(
                            Bridge::A2a,
                            &effective,
                            root,
                            vec![registration.descriptor().provider.name.clone()],
                        );
                        registered.push(registration);
                    }
                }
                // Degraded mode: one unreachable agent must not sink the
                // open. mentra's error carries no server text and no header
                // value, and a configuration error that would have quoted the
                // URL was refused at load (`a2a::file::refused`).
                Err(error) => {
                    eprintln!("Warning: A2A agent '{effective}' failed to connect: {error}");
                }
            }
        }

        Self {
            runtime,
            claimed,
            registered,
            root: root.to_path_buf(),
        }
    }

    /// The effective agent names, in configuration order, connected or not —
    /// what the workspace reports.
    pub(crate) fn names(&self) -> &[String] {
        &self.claimed
    }
}

/// Puts one agent's tool on the registry for this workspace's audience.
///
/// Audience-scoped and non-replacing for the reasons the MCP bridge gives: a
/// repository's agent is the repository's, and with the name claimed, the only
/// thing left to collide with is a global a host registered on the same
/// `mentra::Runtime` itself — which a repository's agent must not silently
/// take over.
fn register(
    runtime: &Runtime,
    audience: &ToolAudience,
    tool: A2aAgentTool,
) -> Option<AudienceToolRegistration> {
    let name = tool.name().to_string();
    match runtime
        .mentra_runtime()
        .try_register_tool_for_audience(audience.clone(), tool)
    {
        Ok(registration) => Some(registration),
        Err(_) => {
            eprintln!(
                "Warning: A2A agent tool '{name}' is a name this runtime already answers to; \
                 it was not bridged"
            );
            None
        }
    }
}

impl Drop for A2aConnections {
    fn drop(&mut self) {
        // Before the claims: a name freed while the tool under it is still
        // registered is a name the next claimant would bridge over. Dropping
        // each registration is the unregister.
        self.registered.clear();

        for name in self.claimed.drain(..) {
            self.runtime
                .release_bridged_claim(Bridge::A2a, &name, &self.root);
        }
    }
}
