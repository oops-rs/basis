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

use agent_client_protocol::schema::v1::{SetSessionModeRequest, StopReason};

use crate::client::{connected, drive, open, say};
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

    // `None` means the client is not prepared to answer: if lan asked anyway,
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
