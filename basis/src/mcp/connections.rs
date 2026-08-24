//! The connections a workspace owns, on a runtime it may share.
//!
//! ADR-0018: MCP *connections* are minted from repository config and die with
//! the workspace; the runtime never holds a connection whose config it cannot
//! see. mentra's builder-level registration (`with_mcp_server` + `build_async`)
//! cannot express that — servers registered there live as long as the runtime
//! — so basis connects through its own [`McpManager`] after the build, on both
//! the shared and the private path, and `build` stays synchronous everywhere.
//!
//! The runtime's tool registry is single, so:
//!
//! - each server's name is **claimed** on the runtime first
//!   ([`Runtime::claim_mcp_server`]), and a name another workspace holds comes
//!   back suffixed — the effective name is what tools are bridged under and
//!   what [`Workspace::mcp_servers`](crate::Workspace::mcp_servers) reports;
//! - a workspace's bridged tools come off the registry when it drops, together
//!   with the claim. While `unregister_tool` was `pub(crate)` upstream they
//!   could only be left where they were and hidden from every roster minted
//!   afterwards; the hiding stays, because it is what keeps a *live* sibling's
//!   tools out of this workspace's roster, but the registry no longer grows by
//!   one server's worth of entries per open.
//!
//! A server that fails to connect degrades exactly as mentra's `build_async`
//! degraded: a warning names it, the open continues, and the name still
//! appears in the workspace's report — a client can see which servers are
//! configured whether or not they came up. A tool whose name the registry
//! already answers to degrades the same way, for the same reason: one tool
//! that cannot be bridged must no more sink a workspace than one server that
//! cannot be reached.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mentra::mcp::McpManager;

use crate::runtime::Runtime;

use super::{ConfiguredServer, McpServer};

/// One workspace's live MCP servers: the manager holding the child processes
/// and HTTP clients, and the names claimed on the shared registry.
pub(crate) struct McpConnections {
    runtime: Arc<Runtime>,
    /// `Option` so `Drop` can move the manager onto a task for its async
    /// shutdown.
    manager: Option<McpManager>,
    /// Claimed effective names, released on drop.
    claimed: Vec<String>,
    /// The bridged tool names this workspace put on the shared registry, taken
    /// off again on drop. Not derivable from `claimed`: a server that failed to
    /// connect bridged nothing, and a name the registry already answered to was
    /// skipped.
    bridged: Vec<String>,
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
        servers: Vec<ConfiguredServer>,
    ) -> Self {
        let mut manager = McpManager::new();
        let mut claimed = Vec::new();
        let mut bridged = Vec::new();
        let mut names = Vec::new();

        for ConfiguredServer {
            server,
            sse_inferred,
        } in servers
        {
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
                McpServer::Http(mut config) => {
                    config.name = effective.clone();
                    manager
                        .connect_streamable_http(&config)
                        .await
                        .map_err(|e| e.to_string())
                }
            };

            match outcome {
                Ok(tools) => bridged.extend(bridge(&runtime, &effective, tools)),
                // Degraded mode, mentra's own wording: one unreachable server
                // must not sink the open.
                Err(error) => {
                    eprintln!("{}", connect_warning(&effective, &error, sse_inferred));
                }
            }

            names.push(effective);
        }

        Self {
            runtime,
            manager: Some(manager),
            claimed,
            bridged,
            root: root.to_path_buf(),
            names,
        }
    }

    /// The effective server names, in configuration order.
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }
}

/// The degraded-mode warning, and — when basis chose the transport itself —
/// the diagnosis.
///
/// A bare `url` means SSE for back-compat (see the module docs on
/// transports), so a server that actually speaks Streamable HTTP fails here
/// with an opaque HTTP error. The operator who never wrote `type` is told
/// what was chosen for them and which word fixes it. Recovery stays mentra's
/// business: a failed connect is reported once, never retried from here.
fn connect_warning(name: &str, error: &str, sse_inferred: bool) -> String {
    let mut warning = format!("Warning: MCP server '{name}' failed to connect: {error}");

    if sse_inferred {
        warning.push_str(
            "; basis inferred the HTTP+SSE transport from a bare `url` — if this server \
             speaks Streamable HTTP, say `type: \"http\"`",
        );
    }

    warning
}

/// Puts one server's tools on the shared registry, reporting the names that
/// took.
///
/// `try_register_tool` rather than the replacing `register_tool`: the server
/// name is claimed, so mentra's `mcp__<server>__<tool>` namespacing already
/// keeps two live workspaces apart, and anything left to collide with is a
/// name basis's claim map never granted — a host that registered on the same
/// `mentra::Runtime` itself. Replacing that silently would send calls meant
/// for the host's tool to somebody's MCP server.
fn bridge<T>(runtime: &Runtime, server: &str, tools: Vec<T>) -> Vec<String>
where
    T: mentra::tool::ExecutableTool + 'static,
{
    let mut registered = Vec::new();

    for tool in tools {
        let name = tool.descriptor().provider.name;
        match runtime.mentra_runtime().try_register_tool(tool) {
            Ok(()) => registered.push(name),
            Err(collision) => eprintln!(
                "Warning: MCP server '{server}' offers a tool called '{}', which this runtime \
                 already answers to; it was not bridged",
                collision.name
            ),
        }
    }

    registered
}

impl Drop for McpConnections {
    fn drop(&mut self) {
        // Before the claims: a name freed while the tools under it are still
        // registered is a name the next claimant would bridge over.
        for name in self.bridged.drain(..) {
            self.runtime.mentra_runtime().unregister_tool(&name);
        }

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

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use mentra::tool::{
        ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
    };
    use serde_json::{Value, json};

    use super::*;

    /// Stands in for what a connected server hands back: something registrable
    /// under a name, without a child process to reach for it.
    struct Bridged(&'static str);

    impl ToolDefinition for Bridged {
        fn descriptor(&self) -> RuntimeToolDescriptor {
            RuntimeToolDescriptor::builder(self.0)
                .description("a bridged tool")
                .input_schema(json!({"type": "object"}))
                .build()
        }
    }

    #[async_trait]
    impl ToolExecutor for Bridged {
        async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
            Ok("bridged".to_string())
        }
    }

    fn runtime() -> Arc<Runtime> {
        Arc::new(
            Runtime::builder()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_api_key("test-key")
                .with_ephemeral_history()
                .build()
                .expect("builds"),
        )
    }

    fn registers(runtime: &Runtime, name: &str) -> bool {
        runtime
            .mentra_runtime()
            .tools()
            .iter()
            .any(|tool| tool.provider.name == name)
    }

    #[test]
    fn a_failed_inferred_transport_names_the_inference_and_the_fix() {
        let warning = connect_warning("api", "404 Not Found", true);
        assert!(
            warning.contains("failed to connect: 404 Not Found"),
            "{warning}"
        );
        assert!(warning.contains("bare `url`"), "{warning}");
        assert!(warning.contains("type: \"http\""), "{warning}");

        let explicit = connect_warning("api", "404 Not Found", false);
        assert!(
            !explicit.contains("inferred"),
            "an explicit choice gets no lecture: {explicit}"
        );
    }

    #[test]
    fn every_tool_a_server_offers_reaches_the_registry() {
        let runtime = runtime();

        let names = bridge(
            &runtime,
            "docs",
            vec![Bridged("mcp__docs__search"), Bridged("mcp__docs__fetch")],
        );

        assert_eq!(names, ["mcp__docs__search", "mcp__docs__fetch"]);
        assert!(registers(&runtime, "mcp__docs__search"));
        assert!(registers(&runtime, "mcp__docs__fetch"));
    }

    #[test]
    fn a_name_the_runtime_already_answers_to_is_left_where_it_is() {
        // The claim map namespaces every *server*, so anything still colliding
        // was registered by whoever holds the same `mentra::Runtime` — and a
        // repository's server must not take over a name the host chose.
        let runtime = runtime();
        runtime
            .mentra_runtime()
            .register_tool(Bridged("mcp__docs__search"));

        let names = bridge(
            &runtime,
            "docs",
            vec![Bridged("mcp__docs__search"), Bridged("mcp__docs__fetch")],
        );

        assert_eq!(
            names,
            ["mcp__docs__fetch"],
            "the collision is skipped and the rest of the server still bridges"
        );
    }

    #[test]
    fn what_a_workspace_bridged_comes_off_when_it_drops() {
        let runtime = runtime();
        let connections = McpConnections {
            runtime: Arc::clone(&runtime),
            manager: None,
            claimed: vec!["docs".to_string()],
            bridged: bridge(&runtime, "docs", vec![Bridged("mcp__docs__search")]),
            root: PathBuf::from("/repo"),
            names: vec!["docs".to_string()],
        };
        assert!(registers(&runtime, "mcp__docs__search"));

        drop(connections);

        assert!(
            !registers(&runtime, "mcp__docs__search"),
            "a registry a host keeps for its whole process must not grow by one \
             server's worth of tools per workspace open"
        );
    }
}
