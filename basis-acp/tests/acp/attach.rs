//! One conversation, one connection at a time.
//!
//! A process serving more than one connection — the websocket bridge does —
//! serves them all from one `ServeConfig` and so from one runtime. Two
//! connections that both `session/load` the same id would each mint a session
//! over the same persisted agent, with separate turn locks and separate
//! modes, and drive it concurrently — except that mentra leases an agent to
//! the session holding it, so the second load is refused by the runtime.
//! That is ADR-0019's attach lock, already in place; what these tests pin is
//! basis's side of it: the refusal names the conflict rather than reading as
//! an internal error, closing releases the conversation, and so does a
//! connection going away without closing.
//!
//! Two servers over two duplex pairs, sharing the config, is that situation
//! minus the socket.

use std::{path::PathBuf, sync::Arc};

use agent_client_protocol::schema::v1::{CloseSessionRequest, ErrorCode, LoadSessionRequest};
use basis_acp::ServeConfig;
use mentra::{RuntimePolicy, test::MockRuntime};
use tokio::sync::{mpsc, oneshot};

use crate::client::{answering, connected_with, open, say};
use crate::source::{MOCK_RUNTIME, MockSource, workspace};

/// A runtime with one answer per turn, persisting between them — a
/// conversation has to have been written before another connection can
/// load it.
fn persisting_mock(answers: &[&str]) -> MockRuntime {
    let mut builder = MockRuntime::builder()
        .model("mock-model", "openai")
        .runtime_identifier(MOCK_RUNTIME)
        .with_policy(RuntimePolicy::permissive());
    for answer in answers {
        builder = builder.text(*answer);
    }
    builder.build().expect("mock runtime builds")
}

#[tokio::test]
async fn a_conversation_open_on_one_connection_cannot_be_loaded_on_another() {
    let workspace = workspace();
    let mock = Arc::new(persisting_mock(&["one", "two"]));
    let config = ServeConfig::with_source(MockSource::new(&mock, &workspace));

    // `first` opens a conversation and holds it until told; `second` tries to
    // load it meanwhile, then again once it has been closed.
    let (opened_tx, opened_rx) = oneshot::channel();
    let (refused_tx, refused_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

    let first = connected_with(config.clone(), answering(None), |connection| async move {
        let session = open(&connection).await?;
        // One turn, so the agent is persisted and there is something to load.
        say(&connection, &session, "hello").await?;
        let _ = opened_tx.send(session.clone());

        // Hold the conversation open until the other side has been refused.
        let _ = refused_rx.await;
        connection
            .send_request(CloseSessionRequest::new(session))
            .block_task()
            .await?;
        let _ = closed_tx.send(());

        // And stay connected until the other side is done, so closing — not
        // disconnecting — is what released the conversation.
        let _ = done_rx.recv().await;
        Ok(())
    });

    let second = connected_with(config, answering(None), |connection| async move {
        let session = opened_rx.await.expect("the first connection opened one");

        let refused = connection
            .send_request(LoadSessionRequest::new(session.clone(), PathBuf::from("/")))
            .block_task()
            .await
            .expect_err("a conversation another connection holds must be refused");
        let _ = refused_tx.send(());

        closed_rx.await.expect("the first connection closed it");
        connection
            .send_request(LoadSessionRequest::new(session.clone(), PathBuf::from("/")))
            .block_task()
            .await?;

        // Released to this connection: it can be driven here now.
        say(&connection, &session, "hello").await?;
        drop(done_tx);
        Ok(refused)
    });

    let ((), (refused, _)) = tokio::join!(async { first.await.0 }, second);

    assert_eq!(refused.code, ErrorCode::InvalidParams, "{refused:?}");
    let reason = refused
        .data
        .map(|data| data.to_string())
        .unwrap_or_default();
    assert!(
        reason.contains("another connection"),
        "the error must name the conflict, not just refuse: {reason}"
    );
}

#[tokio::test]
async fn a_connection_that_goes_away_releases_what_it_held() {
    // The browser tab that was closed never sent `session/close`. Its
    // conversation must not stay claimed by a connection that no longer
    // exists, or the person who reopens the tab is locked out of their own
    // conversation until the process restarts.
    let workspace = workspace();
    let mock = Arc::new(persisting_mock(&["one"]));
    let config = ServeConfig::with_source(MockSource::new(&mock, &workspace));

    let (session, _) = connected_with(config.clone(), answering(None), |connection| async move {
        let session = open(&connection).await?;
        say(&connection, &session, "hello").await?;
        Ok(session)
    })
    .await;

    let (loaded, _) = connected_with(config, answering(None), |connection| async move {
        Ok(connection
            .send_request(LoadSessionRequest::new(session, PathBuf::from("/")))
            .block_task()
            .await)
    })
    .await;

    assert!(
        loaded.is_ok(),
        "the first connection is gone, and with it its claim: {loaded:?}"
    );
}
