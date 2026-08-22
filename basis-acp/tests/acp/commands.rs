//! The commands a session offers, and the one basis answers itself.
//!
//! ACP has no method for invoking a command: a client renders the names from
//! `AvailableCommandsUpdate` and sends back what the person typed, as an
//! ordinary `session/prompt`. So the whole of `/compact` over the wire is two
//! claims, and both are checked here — that the name is advertised at all, and
//! that a prompt opening with it is answered by basis rather than forwarded to
//! the model.
//!
//! What compaction actually *does* is `basis`'s own `tests/compact.rs`, driven
//! against a scripted provider. This file is about the routing.

use std::sync::Arc;

use agent_client_protocol::schema::v1::StopReason;
use mentra::{RuntimePolicy, test::MockRuntime};

use crate::client::{connected, drive, open, say};
use crate::source::{MOCK_RUNTIME, MockSource, text_mock, workspace};

#[tokio::test]
async fn every_session_is_offered_the_command_basis_answers_itself() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (_session, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move { open(&connection).await },
    )
    .await;

    // This workspace has no templates at all, which is the case that used to
    // send nothing: `/compact` acts on the session rather than on the
    // workspace, so there is no repository where it does not apply.
    assert_eq!(
        observed.lock().expect("not poisoned").command_names(),
        vec!["compact".to_string()]
    );
}

#[tokio::test]
async fn slash_compact_is_answered_rather_than_sent_to_the_model() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["this answer must never be needed"]));

    let (stop_reasons, observed) = drive(
        MockSource::new(&mock, &workspace),
        vec!["/compact keep the migration plan"],
        None,
    )
    .await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);

    // The discriminating assertion. Without the routing this prompt would have
    // gone to the model as the literal text `/compact keep the migration plan`
    // and been answered — billed for, and answering nothing anyone asked.
    assert!(
        mock.recorded_requests().await.is_empty(),
        "no model request should have been made"
    );

    let observed = observed.lock().expect("not poisoned");
    assert!(
        observed.agent_text().is_empty(),
        "a command is not a prompt: {}",
        observed.agent_text()
    );
    assert!(
        observed.thought_text().contains("nothing to compact"),
        "a session with no history still owes the client an answer: {}",
        observed.thought_text()
    );
}

#[tokio::test]
async fn a_prompt_that_merely_mentions_a_command_is_still_a_prompt() {
    // The other side of the rule the parser draws: only the first token is
    // read, so prose about compacting reaches the model like any other prose.
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["answered"]));

    let (stop_reasons, observed) = drive(
        MockSource::new(&mock, &workspace),
        vec!["compact the log output before you read it"],
        None,
    )
    .await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);
    assert_eq!(
        observed.lock().expect("not poisoned").agent_text(),
        "answered"
    );
    assert_eq!(mock.recorded_requests().await.len(), 1);
}

#[tokio::test]
async fn a_command_on_a_conversation_with_history_compacts_it() {
    // Routing that only looked at the first prompt of a session would pass
    // every test above and fail the way people actually use it: `/compact`
    // arrives in the middle of a long conversation, never at its start. The
    // second scripted answer is the summary — a compacting pass is a model
    // call, and a mock with one turn in it runs out here.
    let workspace = workspace();
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            .runtime_identifier(MOCK_RUNTIME)
            .with_policy(RuntimePolicy::permissive())
            .text("answered")
            .text("a summary of the above")
            .build()
            .expect("mock runtime builds"),
    );
    let source = MockSource::new(&mock, &workspace);

    let (stop_reasons, observed) = connected(source, None, |connection| async move {
        let session = open(&connection).await?;

        Ok(vec![
            say(&connection, &session, "hello").await?,
            say(&connection, &session, "/compact").await?,
        ])
    })
    .await;

    assert_eq!(
        stop_reasons,
        vec![StopReason::EndTurn, StopReason::EndTurn],
        "both a prompt and a command end the turn cleanly"
    );

    let observed = observed.lock().expect("not poisoned");
    assert_eq!(
        observed.agent_text(),
        "answered",
        "the summary is not something the agent said to the user"
    );
    assert!(
        observed.thought_text().contains("context compacted"),
        "the pass has to narrate itself, or the user cannot explain the \
         conversation's memory changing: {}",
        observed.thought_text()
    );
}
