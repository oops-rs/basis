//! The durable lifecycle adapter: files as the coordination surface.
//!
//! ADR-0019 retired the per-workspace daemon. An agent is a checkpoint on
//! disk under one global data directory; execution belongs to whichever
//! process holds its attach lock; liveness belongs to the OS. No verb leaves
//! a resident process behind.

mod attach;
mod data_dir;
mod error;
mod events;
mod inbox;
mod lock;
mod policy;
mod render;
mod state;
mod tasks;
mod verbs;

pub(crate) use error::ClientError;
pub(crate) use tasks::list;
pub(crate) use verbs::{ask, cancel, has_current_task, inbox, send, spawn, wait, watch};
