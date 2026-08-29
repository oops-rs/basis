//! Cancellation of a built-in `/compact` while its summarizer is in flight.
//!
//! This drives both sides of the real ACP connection. The provider is held at
//! the compaction request, the client sends `session/cancel` while the prompt
//! response is still pending, and a later prompt inspects the wire history to
//! prove that no replacement transcript was committed.

use agent_client_protocol::schema::v1::{CancelNotification, SetSessionModeRequest, StopReason};
use basis_acp::ServeConfig;

use crate::{
    client::{connected_with, open, say, start_say},
    source::{blocking_compact_source, workspace},
};

#[tokio::test]
async fn cancelling_an_in_flight_compact_leaves_the_transcript_untouched() {
    let workspace = workspace();
    let (source, blocker) = blocking_compact_source(&workspace);

    let (result, _observed) = connected_with(
        ServeConfig::with_source(source),
        crate::client::answering(None),
        move |connection| async move {
            let session = open(&connection).await?;
            assert_eq!(
                say(&connection, &session, "hello").await?,
                StopReason::EndTurn
            );
            assert_eq!(
                say(&connection, &session, "continue").await?,
                StopReason::EndTurn
            );

            // Keep the request future alive while the provider waits on its
            // compaction gate. This is the wire-level overlap a synchronous
            // `say` helper cannot express.
            let compact = start_say(&connection, &session, "/compact").block_task();
            tokio::pin!(compact);
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                blocker.wait_until_compaction_started(),
            )
            .await
            .expect("the summarizing provider request must be in flight");

            connection.send_notification(CancelNotification::new(session.clone()))?;

            // A request sent after the notification is an ordering barrier:
            // once this response arrives, the server dispatch loop has
            // processed the cancel before we await the compact response.
            connection
                .send_request(SetSessionModeRequest::new(session.clone(), "prompt"))
                .block_task()
                .await?;

            // Leave the provider request blocked. A fixed implementation
            // observes the token and drops this in-flight future; the old
            // route has no token to observe and therefore hits the harness
            // timeout instead of returning.
            let compact_response =
                tokio::time::timeout(std::time::Duration::from_secs(5), &mut compact)
                    .await
                    .expect("cancelled compact must return while its provider is blocked")?;

            let follow_up = say(&connection, &session, "after cancel").await?;
            let requests = blocker.requests();

            Ok((compact_response.stop_reason, follow_up, requests))
        },
    )
    .await;

    let (compact_reason, follow_up_reason, requests) = result;
    assert_eq!(
        compact_reason,
        StopReason::Cancelled,
        "session/cancel must cancel the built-in command, not only ordinary turns"
    );
    assert_eq!(follow_up_reason, StopReason::EndTurn);

    // The third provider request is the follow-up turn. An applied compaction
    // would replace the opening `hello` user item with the summary; seeing both
    // original messages here proves the cancelled pass left the transcript
    // unchanged.
    let follow_up_request = requests.last().expect("follow-up reached the provider");
    assert!(
        follow_up_request
            .messages
            .iter()
            .any(|message| message.text().contains("hello")),
        "the original user turn must survive a cancelled compaction: {:?}",
        follow_up_request.messages
    );
    assert!(
        follow_up_request
            .messages
            .iter()
            .any(|message| message.text().contains("seed answer")),
        "the original assistant turn must survive a cancelled compaction: {:?}",
        follow_up_request.messages
    );
    assert!(
        follow_up_request
            .messages
            .iter()
            .any(|message| message.text().contains("after cancel")),
        "the follow-up prompt must be appended after the unchanged history: {:?}",
        follow_up_request.messages
    );
}
