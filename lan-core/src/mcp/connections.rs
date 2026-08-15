//! The connections a workspace owns, on a runtime it may share.
//!
//! ADR-0018: MCP *connections* are minted from repository config and die with
//! the workspace; the runtime never holds a connection whose config it cannot
//! see. mentra's builder-level registration (`with_mcp_server` + `build_async`)
//! cannot express that — servers registered there live as long as the runtime
//! — so lan connects through its own [`McpManager`] after the build, on both
//! the shared and the private path, and `build` stays synchronous everywhere.
//!
//! The runtime's tool registry is single and has no unregister, so:
//!
//! - each server's name is **claimed** on the runtime first
//!   ([`Runtime::claim_mcp_server`]), and a name another workspace holds comes
//!   back suffixed — the effective name is what tools are bridged under and
//!   what [`Workspace::mcp_servers`](crate::Workspace::mcp_servers) reports;
//! - entries left behind by a dropped workspace are inert rather than removed:
//!   every roster minted afterwards hides them (`Workspace` extends
//!   `hidden_tools` with every `mcp__*` tool it does not own), and the claim
//!   release makes the name reusable.
//!
//! A server that fails to connect degrades exactly as mentra's `build_async`
//! degraded: a warning names it, the open continues, and the name still
//! appears in the workspace's report — a client can see which servers are
//! configured whether or not they came up.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mentra::mcp::McpManager;

use crate::runtime::Runtime;

use super::McpServer;

/// One workspace's live MCP servers: the manager holding the child processes
/// and HTTP clients, and the names claimed on the shared registry.
pub(crate) struct McpConnections {
    runtime: Arc<Runtime>,
    /// `Option` so `Drop` can move the manager onto a task for its async
    /// shutdown.
    manager: Option<McpManager>,
    /// Claimed effective names, released on drop.
    claimed: Vec<String>,
    /// The claim owner; only this root can release its names.
    root: PathBuf,
    /// Every configured server's effective name, connected or not — what the
    /// workspace reports.
    names: Vec<String>,
}

impl McpConnections {
    /// Connects `servers` for the workspace at `root`, bridging each tool onto
    /// the runtime's registry under the claimed name.
    pub(crate) async fn connect(
        runtime: Arc<Runtime>,
        root: &Path,
        servers: Vec<McpServer>,
    ) -> Self {
        let mut manager = McpManager::new();
        let mut claimed = Vec::new();
        let mut names = Vec::new();

        for server in servers {
            let effective = runtime.claim_mcp_server(server.name(), root);
            claimed.push(effective.clone());

            // The claimed name is written into the config before connecting,
            // because the manager namespaces every bridged tool by it.
            let outcome = match server {
                McpServer::Stdio(mut config) => {
                    config.name = effective.clone();
                    manager.connect(&config).await.map_err(|e| e.to_string())
                }
                McpServer::Sse(mut config) => {
                    config.name = effective.clone();
                    manager
                        .connect_sse(&config)
                        .await
                        .map_err(|e| e.to_string())
                }
            };

            match outcome {
                Ok(bridged) => {
                    for tool in bridged {
                        runtime.mentra_runtime().register_tool(tool);
                    }
                }
                // Degraded mode, mentra's own wording: one unreachable server
                // must not sink the open.
                Err(error) => {
                    eprintln!("Warning: MCP server '{effective}' failed to connect: {error}");
                }
            }

            names.push(effective);
        }

        Self {
            runtime,
            manager: Some(manager),
            claimed,
            root: root.to_path_buf(),
            names,
        }
    }

    /// The effective server names, in configuration order.
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }
}

impl Drop for McpConnections {
    fn drop(&mut self) {
        for name in self.claimed.drain(..) {
            self.runtime.release_mcp_claim(&name, &self.root);
        }

        if let Some(mut manager) = self.manager.take() {
            // Shutdown is async (it sends each stdio server a farewell) and
            // `Drop` is not. Inside a tokio context the farewell runs on a
            // task; outside one, dropping the manager still ends every child —
            // mentra's stdio client kills its process when the handle drops —
            // it just skips the polite goodbye.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    manager.shutdown_all().await;
                });
            }
        }
    }
}
