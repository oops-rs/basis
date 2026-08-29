//! Who decides a consequential call: the client, or the mode.
//!
//! One module because all four tests run the same scripted write and differ
//! only in who answers for it — the client, asked and either allowing or
//! refusing, or a mode that answers in its place so the client is never asked.
//! The two mode tests are here rather than in `sessions` for the same reason:
//! what they pin is whether the write happened, not what `session/set_mode`
//! does to a session.
//!
//! These are the tests the crate doc calls the important ones. Only a turn
//! that really asks can catch a handler that awaits the answer inside the
//! dispatch loop.

use std::sync::Arc;

use agent_client_protocol::{
    Agent, ConnectionTo, Error,
    schema::v1::{
        CancelNotification, CloseSessionRequest, DeleteSessionRequest, RequestPermissionOutcome,
        RequestPermissionResponse, SelectedPermissionOutcome, SessionId, SetSessionModeRequest,
        StopReason,
    },
};
use basis_acp::ServeConfig;

use crate::client::{OnPermission, connected, connected_with, drive, open, say};
use crate::source::{MockSource, workspace, writing_mock};

#[tokio::test]
async fn a_consequential_call_asks_the_client_and_does_not_deadlock() {
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));

    let (stop_reasons, observed) = drive(
        MockSource::new(&mock, &workspace),
        vec!["make a file"],
        Some("allow-once"),
    )
    .await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);

    let observed = observed.lock().expect("not poisoned");
    assert_eq!(
        observed.permission_requests.len(),
        1,
        "the write should have been put to the client"
    );
    assert!(
        observed.tool_calls() >= 1,
        "the call itself must also reach the client as a tool call"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "an approved write must actually happen"
    );
}

#[tokio::test]
async fn a_refused_call_does_not_happen() {
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));

    let (stop_reasons, observed) = drive(
        MockSource::new(&mock, &workspace),
        vec!["make a file"],
        Some("reject-once"),
    )
    .await;

    assert_eq!(
        stop_reasons,
        vec![StopReason::EndTurn],
        "a refusal is a normal turn, not a protocol failure"
    );
    assert_eq!(
        observed
            .lock()
            .expect("not poisoned")
            .permission_requests
            .len(),
        1
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a refused write must not reach the disk"
    );
}

#[tokio::test]
async fn switching_to_always_stops_asking_and_says_so() {
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));

    // `None` means the client is not prepared to answer: if basis asked anyway,
    // the request would be cancelled and the write would not happen.
    let (stop_reason, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            connection
                .send_request(SetSessionModeRequest::new(session.clone(), "always"))
                .block_task()
                .await?;

            say(&connection, &session, "make a file").await
        },
    )
    .await;

    assert_eq!(stop_reason, StopReason::EndTurn);

    let observed = observed.lock().expect("not poisoned");
    assert!(
        observed.permission_requests.is_empty(),
        "the mode answered; the client should not have been asked"
    );
    assert_eq!(
        observed.mode_changes(),
        vec!["always"],
        "a mode change is session state, so every view of the session hears it"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "an allowed write must still happen"
    );
}

#[tokio::test]
async fn switching_to_read_only_refuses_without_asking() {
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));

    let (stop_reason, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            connection
                .send_request(SetSessionModeRequest::new(session.clone(), "never"))
                .block_task()
                .await?;

            say(&connection, &session, "make a file").await
        },
    )
    .await;

    assert_eq!(
        stop_reason,
        StopReason::EndTurn,
        "a refusal is a normal turn"
    );

    let observed = observed.lock().expect("not poisoned");
    assert!(
        observed.permission_requests.is_empty(),
        "read-only has nothing to ask about"
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a mode that refuses must actually refuse"
    );
}

/// A client that, asked permission, never answers and instead does `interrupt`
/// to the session — presses stop, closes it, deletes it. What is pinned is that
/// the turn ends anyway: the request to the client is abandoned, the write does
/// not happen, and the prompt comes back `Cancelled`.
async fn interrupted_while_deciding(
    interrupt: fn(&ConnectionTo<Agent>, &SessionId) -> Result<(), Error>,
) -> (StopReason, tempfile::TempDir) {
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));

    let on_permission: OnPermission = Arc::new(move |request, responder, connection, observed| {
        interrupt(&connection, &request.session_id)?;
        // Held, not answered: the person is still looking at the dialog.
        observed
            .lock()
            .expect("not poisoned")
            .unanswered
            .push(responder);
        Ok(())
    });

    let (stop_reason, observed) = connected_with(
        ServeConfig::with_source(MockSource::new(&mock, &workspace)),
        on_permission,
        |connection| async move {
            let session = open(&connection).await?;
            say(&connection, &session, "make a file").await
        },
    )
    .await;

    let observed = observed.lock().expect("not poisoned");
    assert_eq!(
        observed.permission_requests.len(),
        1,
        "the write was put to the client before anything interrupted it"
    );
    assert_eq!(
        observed.unanswered.len(),
        1,
        "and the client never answered"
    );

    (stop_reason, workspace)
}

#[tokio::test]
async fn a_cancel_while_the_client_is_deciding_ends_the_turn() {
    let (stop_reason, workspace) = interrupted_while_deciding(|connection, session| {
        connection.send_notification(CancelNotification::new(session.clone()))
    })
    .await;

    assert_eq!(
        stop_reason,
        StopReason::Cancelled,
        "the client pressed stop, and ACP says the turn must say so"
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a call nobody approved must not happen"
    );
}

#[tokio::test]
async fn closing_the_session_while_the_client_is_deciding_ends_the_turn() {
    let (stop_reason, workspace) = interrupted_while_deciding(|connection, session| {
        connection
            .send_request(CloseSessionRequest::new(session.clone()))
            .detach();
        Ok(())
    })
    .await;

    assert_eq!(stop_reason, StopReason::Cancelled);
    assert!(!workspace.path().join("made.txt").exists());
}

#[tokio::test]
async fn deleting_the_session_while_the_client_is_deciding_ends_the_turn() {
    // Delete waits out the turn by taking its lock, and the turn is waiting on
    // the client: without the interrupt this is the deadlock ADR-0007 names,
    // one hop longer.
    let (stop_reason, workspace) = interrupted_while_deciding(|connection, session| {
        connection
            .send_request(DeleteSessionRequest::new(session.clone()))
            .detach();
        Ok(())
    })
    .await;

    assert_eq!(stop_reason, StopReason::Cancelled);
    assert!(!workspace.path().join("made.txt").exists());
}

/// A client that, asked permission, first switches the session to `mode` and
/// then answers `answer`. Returns whether the write happened.
async fn switched_while_deciding(mode: &'static str, answer: &'static str) -> bool {
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));

    let on_permission: OnPermission = Arc::new(move |request, responder, connection, _observed| {
        // Sent before the answer, so the server reads the switch first: the
        // dispatch loop handles messages in the order they were written.
        connection
            .send_request(SetSessionModeRequest::new(request.session_id.clone(), mode))
            .detach();
        responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(answer)),
        ))
    });

    let (stop_reason, observed) = connected_with(
        ServeConfig::with_source(MockSource::new(&mock, &workspace)),
        on_permission,
        |connection| async move {
            let session = open(&connection).await?;
            say(&connection, &session, "make a file").await
        },
    )
    .await;

    assert_eq!(stop_reason, StopReason::EndTurn);
    let observed = observed.lock().expect("not poisoned");
    assert_eq!(observed.permission_requests.len(), 1);
    assert_eq!(
        observed.mode_changes(),
        vec![mode.to_string()],
        "the switch itself must have landed"
    );

    workspace.path().join("made.txt").exists()
}

#[tokio::test]
async fn a_request_already_put_to_the_client_is_answered_by_the_client() {
    // The rule `mode` documents: a dialog on screen is the authority for the
    // call it is about, and a mode switched while it is open governs the
    // *next* call. Both directions, because either could be the one that
    // quietly got the other treatment.
    assert!(
        switched_while_deciding("never", "allow-once").await,
        "the person allowed it, in a dialog basis put up under `prompt`"
    );
    assert!(
        !switched_while_deciding("always", "reject-once").await,
        "and the person refused it, in the same dialog"
    );
}
