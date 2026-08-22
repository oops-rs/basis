//! Prompting: what goes out, what streams back, that the next turn carries the
//! last, and that a session basis never opened is not one to prompt.
//!
//! Apart from `permission` because nothing here is consequential — these turns
//! only produce text, so the client is never asked anything, and what is
//! pinned is the prompt and the answer rather than what was allowed.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, ImageContent, PromptRequest, SessionUpdate, StopReason, TextContent,
};
use mentra::{RuntimePolicy, test::MockRuntime};

use crate::client::{connected, drive, open};
use crate::source::{MOCK_RUNTIME, MockSource, text_mock, workspace};

#[tokio::test]
async fn a_prompt_streams_back_and_ends_the_turn() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["Hel", "lo ", "world"]));

    let (stop_reasons, observed) =
        drive(MockSource::new(&mock, &workspace), vec!["say hello"], None).await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);

    let observed = observed.lock().expect("not poisoned");
    assert_eq!(
        observed.agent_text(),
        "Hello world",
        "the client must receive the answer as message chunks"
    );
    assert!(
        observed.permission_requests.is_empty(),
        "nothing consequential happened, so nothing should have been asked"
    );
}

#[tokio::test]
async fn a_session_with_no_known_context_window_sends_no_usage_update() {
    // `MockSource` prepares through `prepare_with_session`, which never
    // learns a context window — basis was not the party that resolved this
    // session's model. `UsageUpdate` must not appear at all rather than carry
    // a guessed `size`.
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["Hel", "lo ", "world"]));

    let (_, observed) = drive(MockSource::new(&mock, &workspace), vec!["say hello"], None).await;

    let observed = observed.lock().expect("not poisoned");
    assert!(
        !observed
            .updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::UsageUpdate(_))),
        "no known window means no usage update, not a guessed one"
    );
}

#[tokio::test]
async fn a_second_prompt_continues_the_same_session() {
    let workspace = workspace();
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            .runtime_identifier(MOCK_RUNTIME)
            .with_policy(RuntimePolicy::permissive())
            .text("first")
            .text("second")
            .build()
            .expect("mock runtime builds"),
    );

    let (stop_reasons, observed) =
        drive(MockSource::new(&mock, &workspace), vec!["one", "two"], None).await;

    assert_eq!(
        stop_reasons,
        vec![StopReason::EndTurn, StopReason::EndTurn],
        "both turns must complete on one session"
    );
    assert_eq!(
        observed.lock().expect("not poisoned").agent_text(),
        "firstsecond"
    );

    // The refactor this protocol rides on: turn two carried turn one.
    let requests = mock.recorded_requests().await;
    let second = requests.get(1).expect("a second request was made");
    assert!(
        second
            .messages
            .iter()
            .any(|message| message.text().contains("one")),
        "the second turn must still carry the first"
    );
}

#[tokio::test]
async fn an_image_in_a_prompt_reaches_the_provider_as_one() {
    // `initialize` claims `promptCapabilities.image`, and this is the claim
    // being honest: the bytes the client base64-encoded arrive at the provider
    // as an image block, in the place the client put them. Dropping them
    // silently — which basis did, with a `_ => None` — has the model answer
    // confidently about a screenshot it never saw.
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["I see it"]));

    let (stop_reasons, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            let response = connection
                .send_request(PromptRequest::new(
                    session,
                    vec![
                        // "AQID" is base64 for the three bytes asserted below.
                        ContentBlock::Image(ImageContent::new("AQID", "image/png")),
                        ContentBlock::Text(TextContent::new("what is this")),
                    ],
                ))
                .block_task()
                .await?;

            Ok(response.stop_reason)
        },
    )
    .await;

    assert_eq!(stop_reasons, StopReason::EndTurn);

    let requests = mock.recorded_requests().await;
    let sent = requests
        .first()
        .expect("the turn reached the provider")
        .messages
        .last()
        .expect("a user message was appended");

    assert!(
        sent.content.iter().any(|block| matches!(
            block,
            mentra::ContentBlock::Image {
                source: mentra::ImageSource::Bytes { media_type, data }
            } if media_type == "image/png" && data == &[1, 2, 3]
        )),
        "the image must survive the whole way down: {:?}",
        sent.content
    );
    assert!(
        sent.text().contains("what is this"),
        "and the caption with it"
    );
}

#[tokio::test]
async fn an_unknown_session_is_rejected_rather_than_served() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            Ok(connection
                .send_request(PromptRequest::new(
                    "no-such-session",
                    vec![ContentBlock::Text(TextContent::new("hello"))],
                ))
                .block_task()
                .await)
        },
    )
    .await;

    assert!(
        result.is_err(),
        "prompting a session that was never opened must be an error"
    );
}
