//! Asking the registry a question more than one test module needs to ask.
//!
//! `Runtime::tools_for_audience` (oops-rs/mentra#55) is a real reader now —
//! before it landed, a test that needed this answer had to write to get it: a
//! registration that *collided* was a name already held, and the probe's own
//! registration dropped on the spot, so asking changed nothing. That write-based
//! shim is gone; what stays is the one reason it was a single module and not
//! copy-pasted scaffolding in each test file that wanted it. Two of these had
//! grown independently before — the MCP bridge's and the declared-tool
//! registry's — and they had already drifted: one derived the audience from
//! [`crate::store::runtime_identifier`] and the other hardcoded the string
//! `"basis:/repo"`, which would have gone on answering after a change to the
//! identifier's prefix and silently stopped meaning anything. Deriving it in
//! one place is what makes that impossible.

use std::path::Path;

use mentra::tool::ToolAudience;

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
        .tools_for_audience(Some(&audience_for(root)))
        .iter()
        .any(|descriptor| descriptor.provider.name == name)
}
