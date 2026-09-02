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
    error::RuntimeError,
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
                        writing = handle(&event, &mut sink, &mut approver, &permissions, writing)
                            .await;
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
                writing = handle(&event, sink, approver, permissions, writing).await;
            }
            Err(TryRecvError::Lagged(dropped)) => {
                writing = writing && emit(sink, lag_notice(dropped));
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => return,
        }
    }
}

/// One session event, whole: resolve it if it is a permission request, map it
/// onto the sink, and follow with whatever notice the resolution produced.
///
/// The one place this sequence exists, shared by the live loop and the
/// shutdown drain so the two cannot disagree about whether a notice reaches
/// the sink. Resolution always runs — the turn is blocked on it — while the
/// emits obey `writing`'s failed-sink short-circuit like every other write.
async fn handle<S: EventSink, A: Approver>(
    event: &SessionEvent,
    sink: &mut S,
    approver: &mut A,
    permissions: &SessionPermissionHandle,
    writing: bool,
) -> bool {
    let followup = resolve_if_permission(event, approver, permissions).await;
    let mut writing = writing && emit_session_event(sink, event);
    if let Some(notice) = followup {
        writing = writing && emit(sink, notice);
    }
    writing
}

/// Answers a pending permission request, and reports when the store failed to
/// record the answer as given.
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
    // way — and a request that is simply gone refuses the retry too, and
    // silence is right for that half: mentra already resolved it.
    //
    // What the plain denial *says* depends on what was answered. A refusal
    // that could not be remembered is still exactly the outcome the person
    // chose, so it keeps their own reason — the model must read a human "no",
    // not a store error dressed as one — and the notice says only that the
    // remembering failed. An allow was conditional on being remembered, so it
    // is downgraded fail-closed and both the reason and the notice say the
    // store is why.
    let refused = matches!(
        answer.decision,
        ApprovalDecision::Deny | ApprovalDecision::DenyForSession
    );
    let reason = answer.reason.clone();
    let recorded = permissions.resolve_permission(request_id, permission_decision(answer));
    let Err(error) = recorded else {
        return None;
    };

    let (denied, notice) = if refused {
        let denied = match reason {
            Some(reason) => PermissionDecision::deny().with_reason(reason),
            None => PermissionDecision::deny(),
        };
        (denied, unremembered_notice(tool_name, &error))
    } else {
        let denied = PermissionDecision::deny().with_reason(format!(
            "the approval for {tool_name} could not be recorded ({error}), so the call was denied"
        ));
        (denied, downgraded_notice(tool_name, &error))
    };
    permissions
        .resolve_permission(request_id, denied)
        .is_ok()
        .then_some(notice)
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

/// A refusal stood, but its "…for this session" half was lost: the store
/// could not persist the remembered rule, so this denial applied to this call
/// only and the next call asks again.
fn unremembered_notice(tool_name: &str, error: &RuntimeError) -> Event {
    Event::Notice {
        severity: NoticeSeverity::Warning,
        message: format!(
            "the refusal of {tool_name} could not be remembered ({error}); \
             it applied to this call only"
        ),
    }
}

/// An allow that was conditional on being remembered could not be recorded,
/// so it was downgraded to a denial.
fn downgraded_notice(tool_name: &str, error: &RuntimeError) -> Event {
    Event::Notice {
        severity: NoticeSeverity::Warning,
        message: format!(
            "the answer for {tool_name} could not be recorded ({error}); the call was denied"
        ),
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

    #[test]
    fn the_store_failure_notices_say_which_half_failed() {
        // The two notices answer different questions — "your refusal held but
        // will not be remembered" against "your approval was downgraded" —
        // and a reader must be able to tell them apart from the words alone.
        let error = RuntimeError::Store("disk full".to_string());

        let Event::Notice { severity, message } = unremembered_notice("spawn", &error) else {
            panic!("expected a notice");
        };
        assert_eq!(severity, NoticeSeverity::Warning);
        assert!(message.contains("spawn") && message.contains("could not be remembered"));
        assert!(message.contains("disk full"));

        let Event::Notice { severity, message } = downgraded_notice("spawn", &error) else {
            panic!("expected a notice");
        };
        assert_eq!(severity, NoticeSeverity::Warning);
        assert!(message.contains("spawn") && message.contains("the call was denied"));
        assert!(message.contains("disk full"));
    }
}
