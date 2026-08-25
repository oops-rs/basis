//! The durable lifecycle adapter, over [`basis_tasks`].
//!
//! ADR-0019 retired the per-workspace daemon; `basis_tasks::Tasks` is the
//! crate that now owns the filesystem coordination this module used to. What
//! is left here is CLI-only: the JSON shapes and exit-code mapping
//! (ADR-0015), the terminal rendering, and the [`basis_tasks::PromptHost`]
//! this binary supplies so `--approve prompt` has a terminal to ask at.

mod error;
mod list;
mod prompt_host;
mod render;
mod verbs;

pub(crate) use error::ClientError;
pub(crate) use list::list;
pub(crate) use verbs::{ask, cancel, has_current_task, inbox, send, spawn, wait, watch};
