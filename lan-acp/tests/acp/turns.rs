//! Prompting: the answer streams back, the next turn carries the last, and a
//! session lan never opened is not one to prompt.
//!
//! Apart from `permission` because nothing here is consequential — these turns
//! only produce text, so the client is never asked anything, and what is
//! pinned is what arrived rather than what was allowed.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, StopReason, TextContent};
use mentra::{RuntimePolicy, test::MockRuntime};

use crate::client::{connected, drive};
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
