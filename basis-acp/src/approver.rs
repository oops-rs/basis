//! Asking the ACP client for permission.
//!
//! basis's [`Approver`] answers while the turn is blocked inside mentra waiting
//! for it; ACP answers with a `session/request_permission` round trip to the
//! client. This module is the join between them.
//!
//! # Why this can await at all
//!
//! The approver runs on basis's forwarding task — an async task that exists only
//! to drain one session's events — so awaiting here parks that task and
//! nothing else. The ACP dispatch loop is a different task and stays free.
//!
//! That freedom is the invariant this depends on: `session/prompt` spawns
//! before driving a turn (see [`server`](crate::server)), so when this
//! module's request reaches the client, the loop can still read the answer.
//! Driving a turn inline from the loop instead would deadlock permanently —
//! the client answers, and nothing is listening.
//!
//! # Why the wait can be interrupted
//!
//! A person looking at a permission dialog can press stop instead of
//! answering, close the session, or delete it. All three cancel the turn, and
//! the turn is parked here, on an answer that is now never coming — mentra
//! reads its cancellation flag at round boundaries, and a question mid-tool
//! is not one (see [`session`](crate::session)). So the round trip is raced
//! against the session's [`Interrupt`]: a cancel wins the race, the request
//! to the client is withdrawn — dropping it sends `$/cancel_request`, which is
//! how ACP says "never mind" — and the answer is a denial that names the
//! reason, which carries the turn to the boundary where the flag ends it.

use std::time::Duration;

use agent_client_protocol::{
    Client, ConnectionTo,
    schema::v1::{
        PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
        SessionId, ToolCallUpdate, ToolCallUpdateFields,
    },
};

use crate::session::Interrupt;
use basis::approval::{ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver};

/// Option ids on the wire. Chosen by basis, echoed back by the client, and
/// matched here — so they are a contract with ourselves and belong in one
/// place.
const ALLOW_ONCE: &str = "allow-once";
const ALLOW_ALWAYS: &str = "allow-always";
const REJECT_ONCE: &str = "reject-once";
const REJECT_ALWAYS: &str = "reject-always";

/// How long to wait for a person to answer before giving up.
///
/// Generous, because reading a diff takes as long as it takes. Bounded anyway,
/// because a client that goes away mid-request would otherwise strand the turn
/// forever, and a stuck agent with no explanation is worse than a denial.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Puts approval requests to the ACP client.
pub struct AcpApprover {
    session_id: SessionId,
    connection: ConnectionTo<Client>,
    /// What cuts a wait short. `None` waits for the client or the timeout,
    /// whichever is first — a host driving this approver outside a session
    /// has nothing to interrupt it with.
    interrupt: Option<Interrupt>,
}

impl AcpApprover {
    pub fn new(session_id: SessionId, connection: ConnectionTo<Client>) -> Self {
        Self {
            session_id,
            connection,
            interrupt: None,
        }
    }

    /// Gives up on a pending request when `interrupt` fires — the turn's
    /// cancellation, see the module docs — rather than waiting out the client.
    pub fn interrupted_by(self, interrupt: Interrupt) -> Self {
        Self {
            interrupt: Some(interrupt),
            ..self
        }
    }
}

#[async_trait::async_trait]
impl Approver for AcpApprover {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        let outbound = RequestPermissionRequest::new(
            self.session_id.clone(),
            ToolCallUpdate::new(
                request.tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .title(request.description.clone())
                    .raw_input(request.input.clone()),
            ),
            options(),
        );

        let asked = round_trip(&self.connection, outbound);
        let outcome = match &mut self.interrupt {
            Some(interrupt) => tokio::select! {
                // Biased: an answer that has already arrived is a decision the
                // person made, and it is read before a cancel that raced it.
                biased;
                outcome = asked => outcome,
                () = interrupt.wait() => Err("the turn was cancelled".to_string()),
            },
            None => asked.await,
        };

        match outcome {
            Ok(outcome) => answer(&outcome, &request.tool_name),
            // A failed round trip, a closed connection, a client that never
            // answered, or a turn cancelled while it was deciding. Deny rather
            // than assume consent, and say which it was — the model reads
            // this, and "denied" alone invites a retry.
            Err(why) => refused(&request.tool_name, &why),
        }
    }
}

/// A refusal that names the reason, for the model to read.
fn refused(tool_name: &str, why: &str) -> ApprovalAnswer {
    ApprovalAnswer::new(ApprovalDecision::Deny)
        .because(format!("{tool_name} needs approval and {why}"))
}

/// Performs the round trip, describing the failure when there is one.
async fn round_trip(
    connection: &ConnectionTo<Client>,
    request: RequestPermissionRequest,
) -> Result<RequestPermissionOutcome, String> {
    let response = tokio::time::timeout(
        ANSWER_TIMEOUT,
        connection.send_request(request).block_task(),
    )
    .await;

    match response {
        Ok(Ok(response)) => Ok(response.outcome),
        Ok(Err(_)) => Err("the client could not be reached".to_string()),
        Err(_) => Err(format!(
            "the client did not answer within {} minutes",
            ANSWER_TIMEOUT.as_secs() / 60
        )),
    }
}

/// The four choices basis offers, matching its four [`ApprovalDecision`]s.
///
/// ACP lets an agent name its own options; offering exactly the decisions basis
/// can act on means no answer can arrive that basis has to reinterpret.
fn options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(ALLOW_ONCE, "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            ALLOW_ALWAYS,
            "Allow for this session",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(REJECT_ONCE, "Deny", PermissionOptionKind::RejectOnce),
        PermissionOption::new(
            REJECT_ALWAYS,
            "Deny for this session",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Reads the client's answer.
///
/// Matched on the option id basis itself sent. An id basis does not recognize is a
/// client bug, and the safe reading of an answer we do not understand is a
/// denial.
fn answer(outcome: &RequestPermissionOutcome, tool_name: &str) -> ApprovalAnswer {
    let RequestPermissionOutcome::Selected(selected) = outcome else {
        // `Cancelled` — the turn is being torn down; there is nothing to allow.
        return refused(tool_name, "the request was cancelled");
    };

    match &*selected.option_id.0 {
        ALLOW_ONCE => ApprovalDecision::Allow.into(),
        ALLOW_ALWAYS => ApprovalDecision::AllowForSession.into(),
        REJECT_ONCE => ApprovalAnswer::new(ApprovalDecision::Deny)
            .because(format!("{tool_name} was refused by the client")),
        REJECT_ALWAYS => ApprovalAnswer::new(ApprovalDecision::DenyForSession).because(format!(
            "{tool_name} was refused by the client, for the rest of this session"
        )),
        _ => refused(tool_name, "the client's answer could not be read"),
    }
}

/// Whether an id is one basis offered, for tests and for callers checking a
/// client's echo.
#[cfg(test)]
fn is_known_option(id: &str) -> bool {
    matches!(id, ALLOW_ONCE | ALLOW_ALWAYS | REJECT_ONCE | REJECT_ALWAYS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOptionId, SelectedPermissionOutcome};

    fn selected(id: &str) -> RequestPermissionOutcome {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(PermissionOptionId::new(
            id.to_string(),
        )))
    }

    #[test]
    fn every_offered_option_maps_to_a_decision() {
        assert_eq!(
            answer(&selected(ALLOW_ONCE), "shell").decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            answer(&selected(ALLOW_ALWAYS), "shell").decision,
            ApprovalDecision::AllowForSession
        );
        assert_eq!(
            answer(&selected(REJECT_ONCE), "shell").decision,
            ApprovalDecision::Deny
        );
        assert_eq!(
            answer(&selected(REJECT_ALWAYS), "shell").decision,
            ApprovalDecision::DenyForSession
        );
    }

    #[test]
    fn allowing_needs_no_reason_and_refusing_gives_one() {
        // The model reads a refusal's reason as the tool result; an allowed
        // call explains itself by happening.
        assert_eq!(answer(&selected(ALLOW_ONCE), "shell").reason, None);

        for id in [REJECT_ONCE, REJECT_ALWAYS] {
            let reason = answer(&selected(id), "shell")
                .reason
                .unwrap_or_else(|| panic!("{id} must explain itself"));
            assert!(reason.starts_with("shell "), "{reason}");
        }
    }

    #[test]
    fn the_offered_options_are_exactly_the_ones_understood() {
        // A client can only answer with what it was offered, so an option basis
        // sends but cannot read would be a silent denial.
        for option in options() {
            assert!(
                is_known_option(&option.option_id.0),
                "offered {} but cannot map it",
                option.option_id.0
            );
        }
        assert_eq!(options().len(), 4);
    }

    #[test]
    fn a_cancelled_request_denies() {
        let answer = answer(&RequestPermissionOutcome::Cancelled, "shell");

        assert_eq!(
            answer.decision,
            ApprovalDecision::Deny,
            "a cancelled turn has nothing left to authorize"
        );
        assert_eq!(
            answer.reason.as_deref(),
            Some("shell needs approval and the request was cancelled")
        );
    }

    #[test]
    fn an_unrecognized_answer_denies() {
        let answer = answer(&selected("something-else"), "shell");

        assert_eq!(
            answer.decision,
            ApprovalDecision::Deny,
            "an answer basis cannot read must not be treated as consent"
        );
        assert_eq!(
            answer.reason.as_deref(),
            Some("shell needs approval and the client's answer could not be read")
        );
    }

    #[test]
    fn a_client_that_never_answers_says_so_in_minutes() {
        // The wording quotes the timeout, so it cannot drift away from the
        // constant that actually bounds the wait.
        assert_eq!(
            refused("shell", "the client did not answer within 30 minutes")
                .reason
                .as_deref(),
            Some("shell needs approval and the client did not answer within 30 minutes")
        );
        assert_eq!(ANSWER_TIMEOUT.as_secs() / 60, 30);
    }
}
