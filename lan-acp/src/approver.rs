//! Asking the ACP client for permission.
//!
//! lan's [`Approver`] answers while the turn is blocked inside mentra waiting
//! for it; ACP answers with a `session/request_permission` round trip to the
//! client. This module is the join between them.
//!
//! # Why this can await at all
//!
//! The approver runs on lan's forwarding task — an async task that exists only
//! to drain one session's events — so awaiting here parks that task and
//! nothing else. The ACP dispatch loop is a different task and stays free.
//!
//! That freedom is the invariant this depends on: `session/prompt` spawns
//! before driving a turn (see [`server`](crate::server)), so when this
//! module's request reaches the client, the loop can still read the answer.
//! Driving a turn inline from the loop instead would deadlock permanently —
//! the client answers, and nothing is listening.

use std::time::Duration;

use agent_client_protocol::{
    Client, ConnectionTo,
    schema::v1::{
        PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
        SessionId, ToolCallUpdate, ToolCallUpdateFields,
    },
};

use lan_core::approval::{ApprovalDecision, ApprovalRequest, Approver};

/// Option ids on the wire. Chosen by lan, echoed back by the client, and
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
}

impl AcpApprover {
    pub fn new(session_id: SessionId, connection: ConnectionTo<Client>) -> Self {
        Self {
            session_id,
            connection,
        }
    }
}

#[async_trait::async_trait]
impl Approver for AcpApprover {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalDecision {
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

        match round_trip(&self.connection, outbound).await {
            Some(outcome) => decision(&outcome),
            // A failed round trip, a closed connection, or a client that never
            // answered. Deny rather than assume consent.
            None => ApprovalDecision::Deny,
        }
    }
}

/// Performs the round trip, returning `None` if it failed or timed out.
async fn round_trip(
    connection: &ConnectionTo<Client>,
    request: RequestPermissionRequest,
) -> Option<RequestPermissionOutcome> {
    let response = tokio::time::timeout(
        ANSWER_TIMEOUT,
        connection.send_request(request).block_task(),
    )
    .await;

    match response {
        Ok(Ok(response)) => Some(response.outcome),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// The four choices lan offers, matching its four [`ApprovalDecision`]s.
///
/// ACP lets an agent name its own options; offering exactly the decisions lan
/// can act on means no answer can arrive that lan has to reinterpret.
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
/// Matched on the option id lan itself sent. An id lan does not recognize is a
/// client bug, and the safe reading of an answer we do not understand is a
/// denial.
fn decision(outcome: &RequestPermissionOutcome) -> ApprovalDecision {
    let RequestPermissionOutcome::Selected(selected) = outcome else {
        // `Cancelled` — the turn is being torn down; there is nothing to allow.
        return ApprovalDecision::Deny;
    };

    match &*selected.option_id.0 {
        ALLOW_ONCE => ApprovalDecision::Allow,
        ALLOW_ALWAYS => ApprovalDecision::AllowForSession,
        REJECT_ALWAYS => ApprovalDecision::DenyForSession,
        _ => ApprovalDecision::Deny,
    }
}

/// Whether an id is one lan offered, for tests and for callers checking a
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
        assert_eq!(decision(&selected(ALLOW_ONCE)), ApprovalDecision::Allow);
        assert_eq!(
            decision(&selected(ALLOW_ALWAYS)),
            ApprovalDecision::AllowForSession
        );
        assert_eq!(decision(&selected(REJECT_ONCE)), ApprovalDecision::Deny);
        assert_eq!(
            decision(&selected(REJECT_ALWAYS)),
            ApprovalDecision::DenyForSession
        );
    }

    #[test]
    fn the_offered_options_are_exactly_the_ones_understood() {
        // A client can only answer with what it was offered, so an option lan
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
        assert_eq!(
            decision(&RequestPermissionOutcome::Cancelled),
            ApprovalDecision::Deny,
            "a cancelled turn has nothing left to authorize"
        );
    }

    #[test]
    fn an_unrecognized_answer_denies() {
        assert_eq!(
            decision(&selected("something-else")),
            ApprovalDecision::Deny,
            "an answer lan cannot read must not be treated as consent"
        );
    }
}
