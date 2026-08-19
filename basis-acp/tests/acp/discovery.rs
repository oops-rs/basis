//! What a client can find out without touching a session.
//!
//! `session/list` and what `initialize` advertises, together because they are
//! the same question asked twice: which conversations are there, and which
//! methods may be used on one. The capability test asserts over exactly the
//! list/close/resume/delete set the rest of this crate drives, so an
//! advertisement that drifts from the answers fails here.

use std::sync::Arc;

use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{InitializeRequest, ListSessionsRequest},
};

use crate::client::{connected, open, say};
use crate::source::{MockSource, text_mock, workspace};

/// A conversation the client just opened comes back from `session/list`.
///
/// The protocol half only: this source enumerates its own store, so what is
/// pinned here is that the request reaches it, that the answer arrives as
/// `SessionInfo`, and that the `cwd` a client is told is the one it asked
/// about. Whether the *right* conversations are enumerated for a workspace is
/// `basis-core`'s question, and `basis-core/tests/workspace.rs` answers it.
#[tokio::test]
async fn a_conversation_just_opened_comes_back_from_listing() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["hello"]));
    let cwd = workspace.path().to_path_buf();

    let ((opened, listed), _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        move |connection| async move {
            let session = open(&connection).await?;
            say(&connection, &session, "one").await?;

            let response = connection
                .send_request(ListSessionsRequest::new().cwd(cwd))
                .block_task()
                .await?;

            Ok((session, response))
        },
    )
    .await;

    let found = listed
        .sessions
        .iter()
        .find(|info| info.session_id == opened)
        .expect("the conversation just had must come back from listing");

    assert_eq!(
        found.cwd,
        workspace.path(),
        "a client is told the workspace it asked about"
    );
    assert!(
        listed.next_cursor.is_none(),
        "one workspace's conversations arrive in one read, so there is no second page"
    );
}

#[tokio::test]
async fn listing_without_a_workspace_says_so_rather_than_guessing() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            Ok(connection
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await)
        },
    )
    .await;

    assert!(
        result.is_err(),
        "basis scopes conversations per workspace; answering with one workspace's \
         sessions as though they were all of them would be a lie"
    );
}

#[tokio::test]
async fn the_advertised_session_methods_are_the_ones_lan_answers() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (capabilities, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let initialized = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            Ok(initialized.agent_capabilities.session_capabilities)
        },
    )
    .await;

    // This mock can enumerate, so `list` is claimed here. The source that
    // cannot, and therefore must not claim it, is a unit test — building one
    // is cheaper than serving one.
    assert!(capabilities.list.is_some());
    assert!(capabilities.close.is_some());
    assert!(capabilities.resume.is_some());
    assert!(
        capabilities.delete.is_none(),
        "mentra's store cannot delete, so basis must not claim it can"
    );
}
