//! The tool contract's bindings that live in basis.
//!
//! ADR-0012 names three ways to reach one contract, mentra's `ExecutableTool`.
//! Two of them are here:
//!
//! - [`spawn`] — basis's own tool, the **native** binding's only instance. ADR-0016
//!   made it the model's only route to a command and to a subagent. It is
//!   registered on every runtime [`RuntimeBuilder`](crate::RuntimeBuilder)
//!   builds, and the two doors it replaces — mentra's `shell` and `task` —
//!   leave the model's roster at the same time.
//! - [`declared`] — the **subprocess** binding: `.basis/tools.json` declares a
//!   name, a description, a JSON schema and a command, and basis wraps that
//!   command as a tool speaking JSON over stdio. Registered per workspace, at
//!   [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open), since a
//!   manifest is a repository's data rather than a process's.
//!
//! The third is MCP (`crate::mcp`), which is mentra's client behind a cargo
//! feature and registers nothing of basis's own.
//!
//! # The fourth: a host's own native tool
//!
//! [`RuntimeBuilder::with_tool`](crate::RuntimeBuilder::with_tool) registers a
//! type the *embedding program* implements, in its own process — for a tool
//! that needs context [`declared`]'s subprocesses cannot have: a client
//! handle, a connection, which caller or conversation this call belongs to.
//! Unlike [`declared`], nothing about the tool is data a repository reviews;
//! the host's compiled code is the whole of what it does.
//!
//! What `with_tool` stores is a `Box<dyn ExecutableTool>`, handed whole to
//! mentra's own by-value `RuntimeBuilder::with_tool` at
//! [`build`](crate::RuntimeBuilder::build) time.
//! [oops-rs/mentra#22](https://github.com/oops-rs/mentra/issues/22) closed
//! the gap that once made that impossible: mentra implements `ToolDefinition`
//! and `ToolExecutor` for `Box<T>` and `Arc<T>` (`T: ?Sized`) at the traits'
//! owner, forwarding every method explicitly — `authorization_preview`
//! included, the method a hand-written shim could silently drop and thereby
//! present a host's tool to the approver as its static descriptor.
//!
//! Registered on the runtime, like `spawn` (ADR-0018's host scope): visible to
//! every workspace and subagent that runtime opens, not to one session.
//!
//! Building one needs mentra's own tool-authoring types, re-exported below
//! rather than left for a host to reach through basis to a `mentra`
//! dependency it would otherwise have no reason to declare directly. The set
//! mirrors exactly what [`spawn`]'s own `ExecutableTool` impl uses — proof
//! it is complete enough to write a real tool against, not a guess at what
//! one might need.

pub mod declared;
pub mod spawn;

pub use mentra::tool::{
    ExecutableTool, ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory,
    ToolAuthorizationPreview, ToolCapability, ToolContext, ToolDefinition, ToolDurability,
    ToolExecutionCategory, ToolExecutor, ToolResult, ToolSideEffectLevel,
};
pub use spawn::{DEFAULT_DELEGATION_DEPTH, SPAWN, SpawnTool};
