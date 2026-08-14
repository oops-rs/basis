//! Bounded waiting: deadlines, timeouts, and the watch snapshot loop.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::{self, Instant};

use super::{
    graph::{WaitLease, begin_wait},
    messages::message_payload_for_dispatch,
    payload::{decorate_terminal, terminal_payload},
};
use crate::local::service::Shared;

pub(super) const DEFAULT_WAIT: Duration = Duration::from_secs(30 * 60);
pub(super) const MAX_WAIT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub(in crate::local::service) async fn await_task(
    shared: &Shared,
    task: &str,
    timeout: Duration,
    _lease: WaitLease,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    let mut updates = shared.changed.subscribe();
    loop {
        if let Some(payload) = terminal_payload(shared, task)? {
            return Ok(payload);
        }
        if time::timeout_at(deadline, updates.changed()).await.is_err() {
            return Err(format!(
                "wait for {task} timed out after {}; the task is still running",
                human_duration(timeout)
            ));
        }
    }
}

pub(in crate::local::service) async fn await_message(
    shared: &Shared,
    task: &str,
    message: &str,
    timeout: Duration,
    _lease: WaitLease,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    let mut updates = shared.changed.subscribe();
    loop {
        if let Some(payload) = message_payload_for_dispatch(shared, task, message)? {
            return Ok(payload);
        }
        if time::timeout_at(deadline, updates.changed()).await.is_err() {
            return Err(format!(
                "message {message} on {task} timed out after {}; retry with `lan wait {task} --message {message}` or inspect `lan inbox {task}`",
                human_duration(timeout)
            ));
        }
    }
}

pub(in crate::local::service) async fn watch_task(
    shared: &Shared,
    caller: Option<&str>,
    task: &str,
    since: u64,
    timeout: Duration,
) -> Result<Value, String> {
    // A snapshot that is already useful does not need a live wait lease. In
    // particular, a child may inspect a terminal ancestor without creating an
    // impossible upward wait edge.
    let initial = watch_snapshot(shared, task, since)?;
    let initial_has_events = initial["events"]
        .as_array()
        .is_some_and(|events| !events.is_empty());
    if initial_has_events || initial["terminal"].as_bool().unwrap_or(false) {
        return Ok(initial);
    }

    let _lease = begin_wait(shared, caller, task)?;
    let deadline = Instant::now() + timeout;
    let mut updates = shared.changed.subscribe();
    loop {
        let snapshot = watch_snapshot(shared, task, since)?;
        let has_events = snapshot["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty());
        let terminal = snapshot["terminal"].as_bool().unwrap_or(false);
        if has_events || terminal {
            return Ok(snapshot);
        }
        if time::timeout_at(deadline, updates.changed()).await.is_err() {
            return Ok(snapshot);
        }
    }
}

pub(super) fn watch_snapshot(shared: &Shared, task: &str, since: u64) -> Result<Value, String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    let record = journal
        .get(task)
        .ok_or_else(|| format!("task {task} does not exist"))?;
    let events: Vec<_> = record
        .events
        .iter()
        .filter(|event| event.seq > since)
        .cloned()
        .collect();
    let result = record
        .terminal_result()
        .map(|payload| decorate_terminal(task, payload));
    Ok(json!({
        "task": task,
        "events": events,
        "next_seq": record.next_event.saturating_sub(1),
        "terminal": record.state.is_terminal(),
        "state": record.state,
        "result": result,
    }))
}

pub(in crate::local::service) fn deadline_of(shared: &Shared, task: &str) -> Option<u64> {
    shared
        .journal
        .lock()
        .expect("task journal poisoned")
        .get(task)
        .and_then(|record| record.deadline_at_ms)
}

pub(in crate::local::service) fn is_cancel_requested(shared: &Shared, task: &str) -> bool {
    shared
        .journal
        .lock()
        .expect("task journal poisoned")
        .get(task)
        .is_none_or(|record| record.cancel_requested)
}

pub(in crate::local::service) fn duration_from_ms(value: Option<u64>) -> Duration {
    value
        .map(|milliseconds| Duration::from_millis(milliseconds).min(MAX_WAIT))
        .unwrap_or(DEFAULT_WAIT)
}

fn human_duration(duration: Duration) -> String {
    if duration.as_secs().is_multiple_of(60) {
        format!("{}m", duration.as_secs() / 60)
    } else {
        format!("{}s", duration.as_secs())
    }
}
