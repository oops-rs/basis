//! basis's own tools — the native binding of ADR-0012's tool contract.
//!
//! One tool lives here: [`spawn`], which ADR-0016 made the model's only route
//! to a command and to a subagent. It is registered on every runtime
//! [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open) builds, and the
//! two doors it replaces — mentra's `shell` and `task` — leave the model's
//! roster at the same time.
//!
//! # What is deliberately not here yet
//!
//! There is no `WorkspaceBuilder::with_tool`. Registering *basis's* tool needs
//! no public surface, and a public one taking `impl ExecutableTool` is a
//! larger commitment than it looks: mentra's `RuntimeBuilder::with_tool` takes
//! its tool by value, nothing upstream implements the trait for `Box` or
//! `Arc`, and so basis would have to hand-forward all seven of `ToolExecutor`'s
//! methods — where forgetting `authorization_preview` would leave a host's
//! tool presenting to the approver as something other than what it is. That is
//! not a shim to write on the way past a security feature, and adding the
//! method later is additive. See ADR-0012.

pub mod spawn;

pub use spawn::{MAX_DEPTH, SPAWN, SpawnTool};
