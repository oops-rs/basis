//! Starting a host's task (ADR-0033 §1): the host runs it, under an A2A id
//! this server mints.

use a2a::A2AError;
use std::time::Duration;

use super::{Started, spawn::after};
use crate::{
    host::{HostedStart, TaskHost},
    index::Key,
    service::{from_host, hops::Inbound, hops::effective_deadline},
};

/// The instant a hosted task must have settled by: the server's task
/// deadline, clamped to its caller's.
pub(super) fn deadline_ms(
    task_deadline: Duration,
    inbound: Inbound,
    accepted_ms: u64,
) -> Result<u64, A2AError> {
    let deadline = effective_deadline(task_deadline, inbound, accepted_ms)?;
    Ok(after(accepted_ms, deadline))
}

/// Asks `host` to start the task, and mints the A2A id it is served under:
/// 32 lowercase hex digits, the shape of a `basis-tasks` one, never the
/// host's own.
pub(super) async fn start(host: &dyn TaskHost, start: HostedStart) -> Result<Started, A2AError> {
    let deadline_ms = start.deadline_ms;
    let host_id = host.start(start).await.map_err(from_host)?;
    Ok(Started {
        id: uuid::Uuid::new_v4().simple().to_string(),
        key: Key::Hosted(host_id),
        deadline_ms,
    })
}
