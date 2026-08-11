//! One session's life: opened, asked to switch modes, picked back up by
//! another connection, closed.
//!
//! The `session/*` methods that are bookkeeping rather than work — everything a
//! client can ask about a session without the agent running. What the modes
//! here pin is the vocabulary, not its effect: which modes a new session
//! offers, and that one lan never offered is refused. Whether a mode actually
//! changes who answers for a write is `permission`'s question.
//!
//! `session/load` and `session/resume` are one test because the only
//! difference between them is the replay, and reading them apart would make
//! that difference invisible.

use std::{path::PathBuf, sync::Arc};

use agent_client_protocol::schema::v1::{
    CloseSessionRequest, LoadSessionRequest, NewSessionRequest, ResumeSessionRequest,
    SetSessionModeRequest,
};
use mentra::{RuntimePolicy, test::MockRuntime};

use crate::client::{connected, open, say};
use crate::source::{MOCK_RUNTIME, MockSource, text_mock, workspace};

#[tokio::test]
async fn a_new_session_offers_the_modes_it_can_switch_between() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (modes, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let response = connection
                .send_request(NewSessionRequest::new(PathBuf::from("/")))
                .block_task()
                .await?;

            Ok(response.modes)
        },
    )
    .await;

    let modes = modes.expect("a session reports the modes it has");
    assert_eq!(
        &*modes.current_mode_id.0, "prompt",
        "over ACP there is a client to ask"
    );

    let offered: Vec<String> = modes
        .available_modes
        .iter()
        .map(|mode| mode.id.0.to_string())
        .collect();
    assert_eq!(offered, vec!["always", "prompt", "never"]);
}

#[tokio::test]
async fn a_mode_lan_never_offered_is_refused() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            Ok(connection
                .send_request(SetSessionModeRequest::new(session, "architect"))
                .block_task()
                .await)
        },
    )
    .await;

    assert!(
        result.is_err(),
        "a mode lan cannot act on must be an error, not a silent no-op"
    );
}

#[tokio::test]
async fn loading_a_session_replays_the_conversation_and_resuming_does_not() {
    let workspace = workspace();
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            .runtime_identifier(MOCK_RUNTIME)
            .with_policy(RuntimePolicy::permissive())
            .text("41")
            .build()
            .expect("mock runtime builds"),
    );

    // One connection has the conversation and goes away, as a client that was
    // closed would.
    let (session_id, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;
            say(&connection, &session, "remember 41").await?;
            Ok(session)
        },
    )
    .await;

    // A second connection over the same store picks it up — the cross-process
    // case, minus the process.
    let loading = session_id.clone();
    let (modes, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let response = connection
                .send_request(LoadSessionRequest::new(loading, PathBuf::from("/")))
                .block_task()
                .await?;
            Ok(response.modes)
        },
    )
    .await;

    assert!(
        modes.is_some(),
        "a loaded session reports its mode like a new one"
    );

    // Copied out rather than asserted under the guard: a `std::sync::Mutex`
    // guard alive across an await is a deadlock waiting for a reason, and
    // clippy is right to refuse it even where this test happens not to await.
    let (replayed, agent_text, updates) = {
        let observed = observed.lock().expect("not poisoned");
        (
            observed.replayed_user_text(),
            observed.agent_text(),
            format!("{:?}", observed.updates),
        )
    };
    assert!(
        replayed.contains("remember 41"),
        "loading must replay what the user said: {updates}"
    );
    assert!(agent_text.contains("41"), "and what the agent answered");

    // Resuming is the same pickup without the replay, for a client that keeps
    // its own history.
    let (resumed, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let response = connection
                .send_request(ResumeSessionRequest::new(session_id, PathBuf::from("/")))
                .block_task()
                .await?;
            Ok(response.modes)
        },
    )
    .await;

    assert!(resumed.is_some());
    assert!(
        observed.lock().expect("not poisoned").updates.is_empty(),
        "resuming replays nothing, which is the whole difference from loading"
    );
}

#[tokio::test]
async fn a_closed_session_is_forgotten() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            connection
                .send_request(CloseSessionRequest::new(session.clone()))
                .block_task()
                .await?;

            Ok(say(&connection, &session, "still there?").await)
        },
    )
    .await;

    assert!(
        result.is_err(),
        "closing frees the session, so prompting it afterwards must fail"
    );
}
