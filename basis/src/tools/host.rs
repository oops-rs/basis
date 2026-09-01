//! Workspace-scoped native host-tool registration.
//!
//! One holder serves both ordinary workspace builders and the reusable
//! generation's explicit bind step. It validates every name, claims every
//! name on Basis's shared workspace-tool ledger, then registers every value on
//! Mentra's process-wide registry. Its `Drop` reverses both sides, which makes
//! every failure and every workspace lifetime atomic.

use std::{path::Path, path::PathBuf, sync::Arc};

use crate::{RunError, runtime::Runtime};

#[derive(Clone, Copy)]
pub(crate) enum HostToolBinding {
    Workspace,
    Reusable,
}

pub(crate) struct WorkspaceHostTools {
    runtime: Arc<Runtime>,
    root: PathBuf,
    names: Vec<String>,
    registered: usize,
}

impl WorkspaceHostTools {
    pub(crate) fn empty(runtime: Arc<Runtime>, root: &Path) -> Self {
        Self {
            runtime,
            root: root.to_path_buf(),
            names: Vec::new(),
            registered: 0,
        }
    }

    pub(crate) fn register(
        runtime: Arc<Runtime>,
        root: &Path,
        tools: Vec<Box<dyn crate::tools::ExecutableTool>>,
        binding: HostToolBinding,
    ) -> Result<Self, RunError> {
        let names = tools
            .iter()
            .map(|tool| tool.descriptor().provider.name)
            .collect::<Vec<_>>();

        for name in &names {
            validate_name(name).map_err(|reason| binding.invalid_name(name, reason))?;
        }

        let mut held = Self::empty(runtime, root);
        for name in &names {
            held.runtime
                .claim_host_tool(name, root)
                .map_err(|reason| binding.name_taken(name, reason))?;
            held.names.push(name.clone());
        }

        for tool in tools {
            held.runtime
                .mentra_runtime_internal()
                .try_register_tool(tool)?;
            held.registered += 1;
        }

        Ok(held)
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }
}

impl HostToolBinding {
    fn invalid_name(self, name: &str, reason: &'static str) -> RunError {
        match self {
            Self::Workspace => RunError::WorkspaceHostToolName {
                name: name.to_string(),
                reason,
            },
            Self::Reusable => RunError::ReusableHostToolName {
                name: name.to_string(),
                reason,
            },
        }
    }

    fn name_taken(self, name: &str, reason: String) -> RunError {
        match self {
            Self::Workspace => RunError::WorkspaceHostToolNameTaken {
                name: name.to_string(),
                reason,
            },
            Self::Reusable => RunError::HostTool(mentra::tool::ToolNameCollision {
                name: name.to_string(),
            }),
        }
    }
}

fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        Err("a name cannot be empty")
    } else if name.len() > 64 {
        Err("a name cannot exceed 64 bytes")
    } else if name.starts_with("mcp__") {
        Err("the `mcp__` prefix is reserved for MCP bridges")
    } else if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        Err("a name may contain only ASCII letters, digits, `_`, and `-`")
    } else {
        Ok(())
    }
}

impl Drop for WorkspaceHostTools {
    fn drop(&mut self) {
        for (index, name) in self.names.drain(..).enumerate() {
            if index < self.registered {
                self.runtime.release_workspace_tool(&name, &self.root);
            } else {
                self.runtime.abandon_workspace_tool_claim(&name, &self.root);
            }
        }
    }
}
