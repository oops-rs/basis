//! Asking the registry a question it has no reader for.
//!
//! mentra exposes no way to enumerate one audience's tool registrations —
//! `Runtime::tools` and `Runtime::tool_descriptor` walk the global map only, so
//! an audience-registered tool is invisible to both. The upstream fix is
//! `Runtime::tools_for_audience` ([mentra#55]); until it lands, a test that
//! needs the answer has to write to get it: a registration that *collides* is a
//! name already held, globally or in that audience, and the probe's own
//! registration drops on the spot, so asking changes nothing.
//!
//! One helper rather than one per module. Two of these had grown — the MCP
//! bridge's and the declared-tool registry's — and they had already drifted:
//! one derived the audience from [`crate::store::runtime_identifier`] and the
//! other hardcoded the string `"basis:/repo"`, which would have gone on
//! answering after a change to the identifier's prefix and silently stopped
//! meaning anything. Deriving it in one place is what makes that impossible.
//!
//! [mentra#55]: https://github.com/oops-rs/mentra/issues/55

use std::path::Path;

use async_trait::async_trait;
use mentra::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolAudience, ToolDefinition, ToolExecutor,
    ToolResult,
};
use serde_json::{Value, json};

use super::Runtime;

/// The audience a workspace rooted at `root` resolves in — derived exactly as
/// [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open) derives it, so a
/// test and an open cannot disagree about which namespace they are talking
/// about.
pub(crate) fn audience_for(root: &Path) -> ToolAudience {
    ToolAudience::new(crate::store::runtime_identifier(root))
}

/// Whether `root`'s audience already answers to `name`.
///
/// The honest answer to "is this tool on the runtime for this workspace",
/// which basis's own claim ledgers cannot give: they are separate bookkeeping
/// and could disagree with mentra's registry.
pub(crate) fn answers(runtime: &Runtime, root: &Path, name: &str) -> bool {
    runtime
        .mentra_runtime()
        .try_register_tool_for_audience(audience_for(root), Probe(name.to_string()))
        .is_err()
}

/// Something registrable under an arbitrary name, with nothing behind it.
struct Probe(String);

impl ToolDefinition for Probe {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(&self.0)
            .description("a probe")
            .input_schema(json!({"type": "object"}))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for Probe {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Ok("probed".to_string())
    }
}
