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
//! This does *not* hand-forward `ToolExecutor`'s methods to some boxed
//! `dyn ExecutableTool` basis stores — nothing upstream implements the trait
//! for `Box` or `Arc`, so a stored trait object was never how this works, and
//! [oops-rs/mentra#22](https://github.com/oops-rs/mentra/issues/22) (adding
//! those forwarding impls at the trait's owner) stays open and unneeded. What
//! `with_tool` actually stores is a closure that captures the caller's
//! concrete tool by value and applies it to mentra's own by-value
//! `RuntimeBuilder::with_tool` at [`build`](crate::RuntimeBuilder::build)
//! time — the concrete type is erased behind `FnOnce`, not behind the trait,
//! so there is no forwarding impl to get right or wrong.
//!
//! Registered on the runtime, like `spawn` (ADR-0018's host scope): visible to
//! every workspace and subagent that runtime opens, not to one session.

pub mod declared;
pub mod spawn;

pub use spawn::{MAX_DEPTH, SPAWN, SpawnTool};
