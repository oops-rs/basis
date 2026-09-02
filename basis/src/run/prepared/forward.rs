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
    approval::{ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver},
    event::{Event, NoticeSeverity},
    run::EventSink,
};

/// Drains the session's event stream into the sink until the turn is done,
/// then drains whatever is still queued and hands the sink back.
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
) -> S {
    tokio::pin!(done);
    // Cleared by the first failed write, and never set again: a sink that
    // refused one event is not asked to take the rest.
    let mut writing = true;
    loop {
        tokio::select! {
            // Biased so queued events always win over the shutdown signal:
            // the turn finishing must not truncate the stream.
            biased;

            received = receiver.recv() => {
                match received {
                    Ok(event) => {
                        let followup = resolve_if_permission(&event, &mut approver, &permissions).await;
                        writing = writing && emit_session_event(&mut sink, &event);
                        if let Some(notice) = followup {
                            writing = writing && emit(&mut sink, notice);
                        }
                    }
                    // Lagging is recoverable — the receiver keeps working, it
                    // just skipped ahead. Say so and carry on. The lossless
                    // usage observer is independent of this presentation path.
                    Err(RecvError::Lagged(dropped)) => {
                        writing = writing && emit(&mut sink, lag_notice(dropped));
                    }
                    // A closed channel means the session is gone; nothing more
                    // can arrive, so stop without waiting for the signal.
                    Err(RecvError::Closed) => return sink,
                }
            }
            _ = &mut done => {
                drain(
                    &mut receiver,
                    &mut sink,
                    &mut approver,
                    &permissions,
                    writing,
                )
                .await;
                return sink;
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
    mut writing: bool,
) {
    loop {
        match receiver.try_recv() {
            Ok(event) => {
                let followup = resolve_if_permission(&event, approver, permissions).await;
                writing = writing && emit_session_event(sink, &event);
                if let Some(notice) = followup {
                    writing = writing && emit(sink, notice);
                }
            }
            Err(TryRecvError::Lagged(dropped)) => {
                writing = writing && emit(sink, lag_notice(dropped));
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => return,
        }
    }
}

/// Answers a pending permission request, and reports when the answer had to be
/// downgraded to a denial.
///
/// The turn is blocked inside mentra waiting for this, so failing to resolve
/// would hang the run — which is what happened before basis answered at all.
async fn resolve_if_permission<A: Approver>(
    event: &SessionEvent,
    approver: &mut A,
    permissions: &SessionPermissionHandle,
) -> Option<Event> {
    let SessionEvent::PermissionRequested {
        request_id,
        tool_call_id,
        tool_name,
        description,
        preview,
        classification,
    } = event
    else {
        return None;
    };

    let answer = approver
        .approve(&ApprovalRequest {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            description: description.clone(),
            input: serde_json::from_str(preview)
                .unwrap_or_else(|_| serde_json::Value::String(preview.clone())),
            // Read straight off the event (mentra#21). mentra documents
            // the classification as always present on a live request, but
            // that is mentra's invariant to keep, not basis's to unwrap — a
            // `None` reaches the approver as unknown, which it is told to
            // judge as the worst the call could be.
            side_effect_level: classification.as_ref().map(|c| c.side_effect_level),
        })
        .await;

    // Fallible since mentra 0.26, and in two different ways. A remembered
    // answer is persisted to the live rule store *before* the request is
    // resolved, and on a store failure mentra puts the pending request back
    // unanswered — so ignoring the error here would leave the turn blocked on
    // a oneshot nobody will ever answer. The other failure is the old one: the
    // request was already resolved or withdrawn (a timeout, a cancellation),
    // and there is nothing left to answer.
    //
    // The retry below tells the two apart by what it finds. A restored request
    // accepts a plain denial — nothing to persist, so it cannot fail the same
    // way — which is the fail-closed reading of an answer that could not be
    // recorded: the person's consent was conditional on being remembered, and
    // a run must end deterministically rather than hang on the store. A
    // request that is simply gone refuses the retry too, and silence is right
    // for that half: mentra already resolved it.
    let recorded = permissions.resolve_permission(request_id, permission_decision(answer));
    let Err(error) = recorded else {
        return None;
    };

    let denied = PermissionDecision::deny().with_reason(format!(
        "the approval for {tool_name} could not be recorded ({error}), so the call was denied"
    ));
    permissions
        .resolve_permission(request_id, denied)
        .is_ok()
        .then(|| Event::Notice {
            severity: NoticeSeverity::Warning,
            message: format!(
                "the answer for {tool_name} could not be recorded ({error}); the call was denied"
            ),
        })
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
