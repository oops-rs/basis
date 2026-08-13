//! Local lifecycle service and its command client.

mod client;
mod protocol;
mod registry;
mod service;
mod store;

pub(crate) use client::{cancel, has_current_task, inbox, send, spawn, wait, watch};
pub(crate) use service::run_daemon;
