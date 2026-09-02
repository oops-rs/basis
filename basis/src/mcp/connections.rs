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
//!   with the claim, so the registry no longer grows by one server's worth of
//!   entries per open;
//! - what each connection actually bridged is recorded beside the claim
//!   ([`Runtime::record_bridged_tools`]). mentra's audience ladder keeps a
//!   workspace in *another* directory from seeing these names, and cannot keep
//!   a second live open of *this* directory from seeing them — one directory is
//!   one audience — so the names are what
//!   [`Workspace::minted_agent`](crate::Workspace) hides from a sibling that
//!   configured a different set of servers.
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

use mentra::{mcp::McpManager, tool::AudienceToolRegistration, tool::ToolAudience};

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
    /// mentra's holds on the tools this workspace bridged, which are what keep
    /// them answering: dropping one takes its tool off the registry, so
    /// dropping this vector is the unregister. Not derivable from `claimed`: a
    /// server that failed to connect bridged nothing, and a name the registry
    /// already answered to was skipped.
    bridged: Vec<AudienceToolRegistration>,
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
        audience: &ToolAudience,
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
                Ok(tools) => {
                    let registered = bridge(&runtime, audience, &effective, tools);
                    // What took, not what was offered: a name the registry
                    // already answered to was skipped, and a name this
                    // workspace does not actually serve must not be hidden
                    // from a sibling that legitimately does.
                    runtime.record_bridged_tools(&effective, root, registered_names(&registered));
                    bridged.extend(registered);
                }
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

/// Puts one server's tools on the registry for this workspace's audience,
/// handing back the registrations that took.
///
/// Audience-scoped because a `.mcp.json` server is a *repository's*: a global
/// registration would offer one repository's `mcp__prod-db__query` to every
/// other workspace on the runtime, which is exactly what every mint used to
/// have to undo by hiding the name. mentra's ladder answers it instead — a
/// name held only by a foreign audience resolves to `Hidden`, so it is neither
/// listed nor reachable by a model that guesses it.
///
/// `try_register_tool_for_audience` rather than the replacing `register_tool`:
/// the server name is claimed, so mentra's `mcp__<server>__<tool>` namespacing
/// already keeps two live workspaces apart, and anything left to collide with
/// is a name basis's claim map never granted — a global a host registered on
/// the same `mentra::Runtime` itself. Replacing that silently would send calls
/// meant for the host's tool to somebody's MCP server.
fn bridge<T>(
    runtime: &Runtime,
    audience: &ToolAudience,
    server: &str,
    tools: Vec<T>,
) -> Vec<AudienceToolRegistration>
where
    T: mentra::tool::ExecutableTool + 'static,
{
    let mut registered = Vec::new();

    for tool in tools {
        match runtime
            .mentra_runtime()
            .try_register_tool_for_audience(audience.clone(), tool)
        {
            Ok(registration) => registered.push(registration),
            Err(collision) => eprintln!(
                "Warning: MCP server '{server}' offers a tool called '{}', which this runtime \
                 already answers to; it was not bridged",
                collision.name
            ),
        }
    }

    registered
}

/// The names a batch of registrations put on the registry, in order.
///
/// Read off basis's own holds rather than asked of mentra, which exposes no
/// reader for one audience's registrations (upstream `mentra#55`).
fn registered_names(registered: &[AudienceToolRegistration]) -> Vec<String> {
    registered
        .iter()
        .map(|registration| registration.descriptor().provider.name.clone())
        .collect()
}

impl Drop for McpConnections {
    fn drop(&mut self) {
        // Before the claims: a name freed while the tools under it are still
        // registered is a name the next claimant would bridge over. Dropping
        // each registration is the unregister.
        self.bridged.clear();

        for name in self.claimed.drain(..) {
            self.runtime.release_mcp_claim(&name, &self.root);
        }

        if let Some(mut manager) = self.manager.take() {
            // Shutdown is async (it sends each stdio server a farewell, and a
            // Streamable HTTP session its ending DELETE) and `Drop` is not.
            // Inside a tokio context the farewell runs on a task; outside
            // one, dropping the manager still cleans up what a drop can —
            // mentra's stdio client kills its process when the handle drops,
            // and since 0.21 (upstream 37ff807) its Streamable HTTP client
            // spawns its DELETE best-effort on drop too — but best-effort is
            // the word: nothing sends outside a tokio context, and a task
            // spawned during runtime teardown may never be polled, so
            // `shutdown_all` here remains the only path that waits.
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

    fn audience() -> ToolAudience {
        ToolAudience::new("basis:/repo")
    }

    /// The names a batch of registrations put on the registry, in order.
    fn named(registered: &[AudienceToolRegistration]) -> Vec<&str> {
        registered
            .iter()
            .map(|registration| registration.descriptor().provider.name.as_str())
            .collect()
    }

    /// Whether `audience` already answers to `name`.
    ///
    /// mentra exposes no reader for one audience's registrations —
    /// `Runtime::tools` lists globals only (an upstream candidate) — so this
    /// asks the question the surface does answer: a registration that collides
    /// is a name already held, globally or in this audience. The probe's own
    /// registration drops on the spot, so asking changes nothing.
    fn answers(runtime: &Runtime, name: &'static str) -> bool {
        runtime
            .mentra_runtime()
            .try_register_tool_for_audience(audience(), Bridged(name))
            .is_err()
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

        let registered = bridge(
            &runtime,
            &audience(),
            "docs",
            vec![Bridged("mcp__docs__search"), Bridged("mcp__docs__fetch")],
        );

        assert_eq!(
            named(&registered),
            ["mcp__docs__search", "mcp__docs__fetch"]
        );
        assert!(answers(&runtime, "mcp__docs__search"));
        assert!(answers(&runtime, "mcp__docs__fetch"));
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

        let registered = bridge(
            &runtime,
            &audience(),
            "docs",
            vec![Bridged("mcp__docs__search"), Bridged("mcp__docs__fetch")],
        );

        assert_eq!(
            named(&registered),
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
            bridged: bridge(
                &runtime,
                &audience(),
                "docs",
                vec![Bridged("mcp__docs__search")],
            ),
            root: PathBuf::from("/repo"),
            names: vec!["docs".to_string()],
        };
        assert!(answers(&runtime, "mcp__docs__search"));

        drop(connections);

        assert!(
            !answers(&runtime, "mcp__docs__search"),
            "a registry a host keeps for its whole process must not grow by one \
             server's worth of tools per workspace open"
        );
    }
}
