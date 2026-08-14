//! Durable task state transitions and structured-concurrency controls.

mod graph;
mod messages;
mod payload;
mod policy;
mod transition;
mod wait;

#[cfg(test)]
mod tests;

pub(super) use self::{
    graph::{WaitGraph, WaitLease, begin_wait},
    messages::{enqueue_message, inbox, message_payload_for_dispatch},
    payload::{accepted_payload, terminal_payload},
    policy::send_next_hint,
    transition::{
        cancel_task, finish_cancelled, finish_failed, orphan_running, persist,
        settle_or_take_message,
    },
    wait::{
        await_message, await_task, deadline_of, duration_from_ms, is_cancel_requested, watch_task,
    },
};
