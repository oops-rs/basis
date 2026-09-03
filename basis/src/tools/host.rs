//! Putting one host's own native tools on the runtime a workspace borrows,
//! and taking the claim back when the workspace goes.
//!
//! The shape is [`declared`](super::declared::registry)'s, because the problem
//! is the same one: the tool registry is the *runtime's* and single, while what
//! is being registered belongs to one open. So a name is claimed before
//! anything is registered, and the claim — with the tool under it — is released
//! when the workspace holding it goes.
//!
//! Where it differs is who may join. A declaration is data, so a second live
//! open of one directory that declares the same thing is provably asking for
//! the same program and joins the registration already there. A native tool is
//! compiled code closing over whatever the host had at the call site — a client
//! handle, a connection, which caller this open is for — so two of them under
//! one name cannot be compared and the second open is refused rather than
//! silently served the first one's closure. `crate::runtime::claims` carries
//! the whole argument.
//!
//! # The name rules are the declared binding's
//!
//! [`declared::manifest::check_name`](super::declared::manifest::check_name)
//! is called rather than restated: what may be a tool name on this runtime
//! does not depend on which binding put it there, and two copies of one rule
//! answer differently the first time either is edited — the mistake
//! [`crate::runtime::probe`] exists to remember.
//!
//! One of those rules is load bearing here in a way it is not there. A
//! workspace-scoped registration is invisible to
//! [`Runtime::foreign_mcp_tools`](crate::runtime::Runtime::foreign_mcp_tools),
//! which walks the *global* registry to find names shaped like a bridged tool
//! of a server nobody configured — so a host tool wearing `mcp__` would be the
//! one such name basis could not catch after the fact, offered to this
//! workspace's model as though a server it never configured were connected.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mentra::tool::ToolAudience;

use crate::{
    RunError,
    runtime::Runtime,
    tools::{ExecutableTool, declared::manifest::check_name},
};

/// One workspace's host tools, registered on a runtime it may share.
///
/// Names and a root only, so `Debug` says what is held without reaching into
/// compiled code that has no business being formatted.
#[derive(Debug)]
pub(crate) struct WorkspaceHostTools {
    runtime: Arc<Runtime>,
    /// The claim owner; only this root can release its names.
    root: PathBuf,
    /// Claimed names, released on drop.
    names: Vec<String>,
}

impl WorkspaceHostTools {
    /// A holder over nothing, for the workspace that supplied no host tools.
    pub(crate) fn none(runtime: Arc<Runtime>, root: &Path) -> Self {
        Self {
            runtime,
            root: root.to_path_buf(),
            names: Vec::new(),
        }
    }

    /// Claims every name, then registers every tool for `audience`.
    ///
    /// Two passes rather than one, and the order is the point — the same one
    /// [`DeclaredTools::register_with_supplied`](super::declared::registry::DeclaredTools::register_with_supplied)
    /// makes: a set whose fourth tool collides must leave the first three
    /// unregistered, because on a shared runtime a half-registered set from a
    /// workspace that failed to open would still be answering for every other
    /// workspace that shares its audience.
    ///
    /// Each descriptor is read once, at the top, and the name it yields is what
    /// is validated, claimed, and released. mentra reads its own descriptor
    /// again when it registers — that is upstream's call and outside basis's
    /// reach — but nothing basis decides is decided twice off two reads.
    pub(crate) fn register(
        runtime: Arc<Runtime>,
        audience: &ToolAudience,
        root: &Path,
        tools: Vec<Box<dyn ExecutableTool>>,
    ) -> Result<Self, RunError> {
        let named = tools
            .into_iter()
            .map(|tool| (tool.descriptor().provider.name, tool))
            .collect::<Vec<_>>();

        let mut held = Self::none(runtime, root);
        let mut permissions = Vec::new();

        for (name, _) in &named {
            // On failure `held` drops, releasing whatever it had taken, so a
            // refused open leaves the runtime as it found it.
            check_name(name).map_err(|reason| RunError::WorkspaceHostToolName {
                name: name.clone(),
                reason,
            })?;
            let permit = held
                .runtime
                .claim_native_tool(root, name)
                .map_err(|reason| RunError::WorkspaceHostToolNameTaken {
                    name: name.clone(),
                    reason,
                })?;
            held.names.push(name.clone());
            permissions.push(permit);
        }

        for ((name, tool), permit) in named.into_iter().zip(permissions) {
            held.runtime
                .install_claimed_tool(audience, permit, tool)
                .map_err(|reason| RunError::WorkspaceHostToolNameTaken { name, reason })?;
        }

        Ok(held)
    }

    /// The names registered, in the order the host supplied them.
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }
}

impl Drop for WorkspaceHostTools {
    fn drop(&mut self) {
        for name in self.names.drain(..) {
            self.runtime.release_native_tool(&name, &self.root);
        }
    }
}
