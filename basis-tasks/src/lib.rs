//! The durable task layer over [`basis`]: a filesystem-coordinated lifecycle
//! for spawned agents, reachable from Rust and not only from a CLI.
//!
//! ADR-0019 retired the per-workspace daemon. A task is a directory under one
//! global data directory; execution belongs to whichever process holds its
//! `attach.lock` — one writer, ever; liveness belongs to the OS. No verb here
//! leaves a resident process behind, and no verb takes a lock it does not
//! need: `send`, `cancel`, `watch`, `list`, and the read-only accessors never
//! attach at all. [`Tasks`] is the crate's front door; open one and see its
//! docs for what each verb does and costs.
//!
//! Every rule ADR-0017 states is enforced exactly as it was inside the CLI
//! this crate was extracted from, unchanged: 16 messages to a durable inbox,
//! 4 KiB bounded summaries, a finite deadline on every unattended task,
//! downward-only cancellation, and the wait-edge policy that keeps a
//! `basis wait`-shaped call from ever cycling (a descendant or an independent
//! root is a safe edge; an ancestor or a peer is not).
//!
//! # The environment protocol
//!
//! A task's runtime sets three environment variables for every command its
//! turns run (ADR-0018), and this is the one place their names and meanings
//! are declared — they are read by tools this crate never sees run, so this
//! is a protocol this crate publishes rather than an implementation detail
//! it could rename:
//!
//! - [`BASIS_TASK_ID`] — this task's own handle. A tool that wants to `send`
//!   or `ask` its own task — a subagent reporting progress upward, for one —
//!   reads this rather than being told.
//! - [`BASIS_DATA_DIR`] — the data directory root [`Tasks::open`] would
//!   resolve to on its own, named explicitly so a recursive `basis` (or any
//!   other binary built on this crate) invoked from inside a turn resolves
//!   the same data directory its parent did, rather than rediscovering one
//!   from a possibly different environment.
//! - [`BASIS_PARENT_TASK_ID`] — the parent's handle, present only when the
//!   task has one. What lets a deeply nested spawn still name its whole
//!   ownership chain without walking `meta.json` files to find it.
//!
//! [`current_task`] reads the first of these back, for a host that wants to
//! know whether *it* is running as a task's tool call.

mod approve;
mod attach;
mod builders;
mod client;
mod data_dir;
mod error;
mod events;
mod handle;
mod inbox;
mod live;
mod lock;
mod policy;
mod spec;
mod state;
mod tasks;
mod watch;

pub use approve::{Approve, PromptHost, validate_approval};
pub use attach::POLL;
pub use builders::configure_builders;
pub use client::{Reply, Tasks, WaitOutcome};
pub use error::{Error, Hint};
pub use handle::TaskHandle;
pub use live::LiveSink;
pub use spec::{Continuation, DEFAULT_DEADLINE, RunSpec};
pub use state::{
    InboxRecord, MAX_TASKS, MessageRecord, MessageReply, MessageState, Terminal, TerminalRecord,
    now_ms,
};
pub use tasks::{TaskSummary, probe_state};
pub use watch::{EventCursor, WatchRecord};

/// This task's own handle — see [`BASIS_TASK_ID`].
pub const BASIS_TASK_ID: &str = "BASIS_TASK_ID";
/// The data directory root this task's runtime resolved — see
/// [`BASIS_DATA_DIR`].
pub const BASIS_DATA_DIR: &str = "BASIS_DATA_DIR";
/// This task's parent, when it has one — see [`BASIS_PARENT_TASK_ID`].
pub const BASIS_PARENT_TASK_ID: &str = "BASIS_PARENT_TASK_ID";

/// This process's own task, if it is itself running as one — read from
/// [`BASIS_TASK_ID`]. `None` for a value that is absent, blank, or does not
/// fit the task-handle grammar: basis's own runtime always sets a well-formed
/// one, so a malformed value is not basis's, and treating it as "no current
/// task" is the safer reading of somebody else's environment variable.
pub fn current_task() -> Option<TaskHandle> {
    let value = std::env::var(BASIS_TASK_ID).ok()?;
    if value.trim().is_empty() {
        return None;
    }
    TaskHandle::parse(value).ok()
}
