//! The task that carries a turn's events to the sink while the turn runs.
//!
//! Split out of [`prepared`](super) because it is a different job from driving
//! a turn: this runs concurrently with one, on its own task, and it is the only
//! place in basis where the approver is called and where a session event becomes
//! a basis [`Event`]. It also has the run's least obvious rule in it — the
//! forwarder must outlive a failed sink — and that rule is easier to keep true
//! when the code it governs is by itself.

use mentra::{
    SessionEvent, SessionEventReceiver, SessionPermissionHandle,
    session::{PermissionDecision, PermissionRuleScope},
};
use tokio::sync::{
    broadcast::error::{RecvError, TryRecvError},
    oneshot,
};

use crate::{
    approval::{ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, SideEffectLevels},
    event::{Event, NoticeSeverity},
    run::{EventSink, RunUsage},
};

/// Drains the session's event stream into the sink until the turn is done,
/// then drains whatever is still queued and hands the sink back — with what the
/// turn reported spending, tallied on the way past.
///
/// This task also answers permission requests, and mentra blocks the turn until
/// one is answered — so it runs to the end of the turn even when the sink stops
/// accepting events. A forwarder that returned on the first failed write would
/// hang the next consequential call rather than merely stop narrating it, which
/// is what `basis spawn --json | head` would do to every run.
pub(super) async fn forward_events<S: EventSink, A: Approver>(
    mut receiver: SessionEventReceiver,
    mut sink: S,
    done: oneshot::Receiver<()>,
    mut approver: A,
    permissions: SessionPermissionHandle,
    levels: SideEffectLevels,
) -> (S, RunUsage) {
    tokio::pin!(done);
    // Cleared by the first failed write, and never set again: a sink that
    // refused one event is not asked to take the rest.
    let mut writing = true;
    let mut usage = RunUsage::default();

    loop {
        tokio::select! {
            // Biased so queued events always win over the shutdown signal:
            // the turn finishing must not truncate the stream.
            biased;

            received = receiver.recv() => {
                match received {
                    Ok(event) => {
                        usage = usage.recording(&event);
                        resolve_if_permission(&event, &mut approver, &permissions, &levels).await;
                        writing = writing && emit_session_event(&mut sink, &event);
                    }
                    // Lagging is recoverable — the receiver keeps working, it
                    // just skipped ahead. Say so and carry on. Whatever usage
                    // those events reported is lost with them, which is why
                    // `RunUsage` promises a tally rather than an invoice.
                    Err(RecvError::Lagged(dropped)) => {
                        writing = writing && emit(&mut sink, lag_notice(dropped));
                    }
                    // A closed channel means the session is gone; nothing more
                    // can arrive, so stop without waiting for the signal.
                    Err(RecvError::Closed) => return (sink, usage),
                }
            }
            _ = &mut done => {
                let usage = drain(
                    &mut receiver,
                    &mut sink,
                    &mut approver,
                    &permissions,
                    &levels,
                    writing,
                    usage,
                )
                .await;
                return (sink, usage);
            }
        }
    }
}

/// Empties whatever the broadcast channel still holds.
async fn drain<S: EventSink, A: Approver>(
    receiver: &mut SessionEventReceiver,
    sink: &mut S,
    approver: &mut A,
    permissions: &SessionPermissionHandle,
    levels: &SideEffectLevels,
    mut writing: bool,
    mut usage: RunUsage,
) -> RunUsage {
    loop {
        match receiver.try_recv() {
            Ok(event) => {
                usage = usage.recording(&event);
                resolve_if_permission(&event, approver, permissions, levels).await;
                writing = writing && emit_session_event(sink, &event);
            }
            Err(TryRecvError::Lagged(dropped)) => {
                writing = writing && emit(sink, lag_notice(dropped));
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => return usage,
        }
    }
}

/// Answers a pending permission request.
///
/// The turn is blocked inside mentra waiting for this, so failing to resolve
/// would hang the run — which is what happened before basis answered at all.
async fn resolve_if_permission<A: Approver>(
    event: &SessionEvent,
    approver: &mut A,
    permissions: &SessionPermissionHandle,
    levels: &SideEffectLevels,
) {
    let SessionEvent::PermissionRequested {
        request_id,
        tool_call_id,
        tool_name,
        description,
        preview,
    } = event
    else {
        return;
    };

    let answer = approver
        .approve(&ApprovalRequest {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            description: description.clone(),
            input: serde_json::from_str(preview)
                .unwrap_or_else(|_| serde_json::Value::String(preview.clone())),
            // Taken, not read: this request is about to be resolved and never
            // comes round again, so an entry left behind is a leak. `None` —
            // an unwired host, an evicted entry — reaches the approver as
            // unknown, which it is told to judge as the worst it could be.
            side_effect_level: levels.take(tool_call_id),
        })
        .await;

    // A failure here means the request was already resolved or withdrawn;
    // there is nothing useful left to do about it.
    let _ = permissions.resolve_permission(request_id, permission_decision(answer));
}

/// Restates an approver's answer in the terms mentra resolves with.
///
/// The reason rides along on refusals, because mentra puts it in the tool
/// result the model reads; a refusal that gives none keeps mentra's own
/// wording.
fn permission_decision(answer: ApprovalAnswer) -> PermissionDecision {
    let decision = match answer.decision {
        ApprovalDecision::Allow => PermissionDecision::allow(),
        ApprovalDecision::Deny => PermissionDecision::deny(),
        ApprovalDecision::AllowForSession => {
            PermissionDecision::allow_and_remember(PermissionRuleScope::Session)
        }
        ApprovalDecision::DenyForSession => {
            PermissionDecision::deny_and_remember(PermissionRuleScope::Session)
        }
    };

    match answer.reason {
        Some(reason) => decision.with_reason(reason),
        None => decision,
    }
}

/// Maps and emits one session event. Returns `false` when the sink has failed
/// and forwarding should stop.
fn emit_session_event<S: EventSink>(sink: &mut S, event: &SessionEvent) -> bool {
    match Event::from_session_event(event) {
        Some(mapped) => emit(sink, mapped),
        None => true,
    }
}

fn emit<S: EventSink>(sink: &mut S, event: Event) -> bool {
    sink.emit(event).is_ok()
}

/// A dropped-event notice. The alternative — staying quiet — would leave a
/// client with a stream that silently disagrees with what happened.
fn lag_notice(dropped: u64) -> Event {
    Event::Notice {
        severity: NoticeSeverity::Warning,
        message: format!("event stream lagged; {dropped} event(s) dropped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lag_notice_says_how_many_were_lost() {
        let Event::Notice { severity, message } = lag_notice(12) else {
            panic!("expected a notice");
        };

        assert_eq!(severity, NoticeSeverity::Warning);
        assert!(message.contains("12"));
    }
}
