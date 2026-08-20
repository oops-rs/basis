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
//! # What is deliberately not here yet
//!
//! There is still no `WorkspaceBuilder::with_tool`, and [`declared`] does not
//! change that. Registering a tool basis *constructs* needs no public surface;
//! a public one taking `impl ExecutableTool` is a larger commitment than it
//! looks. mentra's `RuntimeBuilder::with_tool` takes its tool by value, nothing
//! upstream implements the trait for `Box` or `Arc`, and so basis would have to
//! hand-forward all seven of `ToolExecutor`'s methods — where forgetting
//! `authorization_preview` would leave a host's tool presenting to the approver
//! as something other than what it is. That is not a shim to write on the way
//! past a security feature, and adding the method later is additive. The
//! forwarding impls are asked of mentra itself
//! ([oops-rs/mentra#22](https://github.com/oops-rs/mentra/issues/22)), where
//! the trait's owner writes them once, beside the trait.
//!
//! The by-value signature costs [`declared`] nothing, because basis builds the
//! `DeclaredTool` itself and hands mentra a value it owns. A host that wants
//! its *own* type registered is the case still waiting, and ADR-0012's answer
//! for now is that it declares the tool in a file, or lends basis a runtime and
//! registers on mentra's surface directly.

pub mod declared;
pub mod spawn;

pub use spawn::{MAX_DEPTH, SPAWN, SpawnTool};
