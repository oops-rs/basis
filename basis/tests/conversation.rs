//! Conversation: more than one turn on one session.
//!
//! Until the session survived a turn, `execute` consumed it and every run was
//! a single prompt — mentra's `resume_session` was unreachable and there was no
//! way to say a second thing. These tests pin the property that unlocked:
//! the model sees the whole conversation, because the session was never thrown
//! away.
//!
//! The interesting assertion is not that a second call returns something. It is
//! that the *provider request* for turn two contains turn one — checked against
//! `MockRuntime::recorded_requests`, so a regression that quietly starts a fresh
//! conversation fails here rather than looking fine.

use basis::{AllowAll, CollectingSink, Event, RunOutcome, TurnOptions, run::prepare_with_session};
use mentra::{
    Role, RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules").expect("write AGENTS.md");
    dir
}

/// Pinned to the workspace, with the parent walk and global file off so an
/// `AGENTS.md` above the temp dir cannot leak in.
fn context() -> basis::ContextConfig {
    basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    }
}

/// A runtime that answers each turn with the next scripted reply.
fn mock(replies: &[&str]) -> MockRuntime {
    let mut builder = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive());
    for reply in replies {
        builder = builder.text(*reply);
    }
    builder.build().expect("mock runtime builds")
}

/// Every user message the provider was sent on the given request, in order.
async fn user_messages(mock: &MockRuntime, request: usize) -> Vec<String> {
    mock.recorded_requests()
        .await
        .get(request)
        .expect("the request was made")
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.text())
        .collect()
}

#[tokio::test]
async fn a_second_turn_sees_the_first() {
    let workspace = workspace();
    let mock = mock(&["Nice to meet you.", "You said hello."]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "hello",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    let first = prepared
        .execute(CollectingSink::new())
        .await
        .expect("the first turn completes");
    assert_eq!(first.final_message.as_deref(), Some("Nice to meet you."));

    let second = prepared
        .send("what did I say?", CollectingSink::new(), AllowAll)
        .await
        .expect("the second turn completes");
    assert_eq!(second.final_message.as_deref(), Some("You said hello."));

    // The property that matters: turn two carried turn one to the model.
    // Asserted as a prefix rather than an exact list because mentra also
    // injects its own recalled-memory block — that is mentra's business, and
    // pinning it here would make this test fail on an unrelated change.
    let sent = user_messages(&mock, 1).await;
    assert!(
        sent.starts_with(&["hello".to_string(), "what did I say?".to_string()]),
        "the second turn must send the whole conversation, not just its own prompt: {sent:?}"
    );
}

#[tokio::test]
async fn each_turn_gets_its_own_bookends() {
    let workspace = workspace();
    let mock = mock(&["one", "two"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "first",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    let first = prepared
        .execute(CollectingSink::new())
        .await
        .expect("first turn");
    let second = prepared
        .send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("second turn");

    for (label, report) in [("first", &first), ("second", &second)] {
        let events = report.sink.events();
        assert!(
            matches!(events.first(), Some(Event::RunStarted { .. })),
            "the {label} turn must open with a header"
        );
        assert!(
            matches!(
                events.last(),
                Some(Event::RunFinished {
                    outcome: RunOutcome::Ok,
                    ..
                })
            ),
            "the {label} turn must close with an outcome"
        );
    }

    // A turn is a complete stream, so a client reading the second one never
    // sees the first one's events replayed into it.
    assert!(
        second
            .sink
            .events()
            .iter()
            .all(|event| !matches!(event, Event::AssistantMessage { text } if text == "one")),
        "the second turn's stream must not repeat the first turn's message"
    );
}

#[tokio::test]
async fn the_session_survives_and_reports_its_history() {
    let workspace = workspace();
    let mock = mock(&["ack", "ack again"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "first",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    assert!(
        prepared.history().is_empty(),
        "nothing is committed before a turn runs"
    );
    let agent_id = prepared.agent_id().to_string();

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("first turn");
    prepared
        .send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("second turn");

    let said: Vec<String> = prepared
        .text_history()
        .filter_map(|(role, text)| match role {
            basis::HistoryRole::User => Some(text),
            basis::HistoryRole::Assistant => None,
        })
        .collect();
    assert_eq!(said, vec!["first".to_string(), "second".to_string()]);

    assert_eq!(
        prepared.agent_id(),
        agent_id,
        "the agent id must be stable across turns — it is what resume takes"
    );
}

/// `answered_turns` is the fact a crash-recovering host takes a watermark of:
/// two turns each add one assistant message, never one of the user's own.
#[tokio::test]
async fn answered_turns_counts_the_assistant_messages_only() {
    let workspace = workspace();
    let mock = mock(&["one", "two"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "first",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    assert_eq!(
        prepared.answered_turns(),
        0,
        "nothing is committed before a turn runs"
    );

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("first turn");
    assert_eq!(prepared.answered_turns(), 1);

    prepared
        .send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("second turn");
    assert_eq!(
        prepared.answered_turns(),
        2,
        "one count per turn, not per message: each turn also committed a user message"
    );
}

/// The text half of the same crash-recovery question `answered_turns` counts:
/// once the watermark says the recorded prompt was already answered, the
/// standing answer is what the crashed process never got to record. Reading it
/// off `history()` means matching `mentra::Role` in the caller, which is the
/// dependency `basis-tasks` carried a whole `mentra` entry for.
#[tokio::test]
async fn last_assistant_text_is_the_newest_committed_answer() {
    let workspace = workspace();
    let mock = mock(&["one", "two"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "first",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    assert_eq!(
        prepared.last_assistant_text(),
        None,
        "nothing is committed before a turn runs"
    );

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("first turn");
    assert_eq!(prepared.last_assistant_text().as_deref(), Some("one"));

    prepared
        .send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("second turn");
    assert_eq!(
        prepared.last_assistant_text().as_deref(),
        Some("two"),
        "the newest answer, not the first — a user message committed after it \
         must not become the answer"
    );
}

#[tokio::test]
async fn an_empty_follow_up_prompt_is_refused() {
    let workspace = workspace();
    let mock = mock(&["ok"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "first",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("first turn");

    let error = prepared
        .send("  \n\t ", CollectingSink::new(), AllowAll)
        .await
        .expect_err("an empty follow-up is rejected");

    assert!(matches!(error, basis::RunError::EmptyPrompt));
}

#[tokio::test]
async fn a_failed_turn_does_not_end_the_conversation() {
    let workspace = workspace();
    // The first turn fails; the session must still take a second prompt.
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .failure(mentra::ProviderError::UnsupportedCapability(
            "scripted failure".to_string(),
        ))
        .text("recovered")
        .build()
        .expect("mock runtime builds");
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "first",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    let failed = prepared
        .execute(CollectingSink::new())
        .await
        .expect("the run reports rather than erroring");
    assert!(!failed.succeeded());

    let recovered = prepared
        .send("try again", CollectingSink::new(), AllowAll)
        .await
        .expect("the session still takes a turn after a failure");

    assert!(
        recovered.succeeded(),
        "a failed turn must not poison the session"
    );
    assert_eq!(recovered.final_message.as_deref(), Some("recovered"));
}

#[tokio::test]
async fn a_cancelled_turn_ends_rather_than_running() {
    let workspace = workspace();
    let mock = mock(&["never reached"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "go",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    // Tripped before the turn starts, so the outcome does not depend on
    // winning a race with the provider. This is the shape a protocol server's
    // stop button uses; only the timing differs.
    let (options, cancel) = TurnOptions::cancellable();
    cancel.cancel();

    let report = prepared
        .send_with_options("go", CollectingSink::new(), AllowAll, options)
        .await
        .expect("a cancelled turn reports rather than erroring");

    assert!(
        matches!(report.outcome, RunOutcome::Error { .. }),
        "a cancelled turn must not report success"
    );
    assert!(
        matches!(report.sink.events().last(), Some(Event::RunFinished { .. })),
        "a cancelled turn still closes its stream"
    );
}

#[tokio::test]
async fn tool_calls_from_an_earlier_turn_stay_in_the_conversation() {
    let workspace = workspace();
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .tool_calls(vec![MockToolCall::new(
            "files",
            serde_json::json!({"operations": [{"op": "list", "path": "."}]}),
        )])
        .text("listed them")
        .text("as I said, I listed them")
        .build()
        .expect("mock runtime builds");
    let session = mock
        .runtime()
        .create_session_with_config(
            "test",
            mock.model(),
            mentra::agent::AgentConfig {
                workspace: mentra::agent::WorkspaceConfig {
                    base_dir: workspace.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session");

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "list the files",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("first turn");
    prepared
        .send("what did you do?", CollectingSink::new(), AllowAll)
        .await
        .expect("second turn");

    // The tool round is part of the conversation, so the last request carries
    // the assistant's tool use and its result, not just the prose.
    let requests = mock.recorded_requests().await;
    let last = requests.last().expect("a request was made");
    let has_tool_use = last.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, mentra::ContentBlock::ToolUse { .. }))
    });
    let has_tool_result = last.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, mentra::ContentBlock::ToolResult { .. }))
    });

    assert!(
        has_tool_use && has_tool_result,
        "a later turn must still see the earlier turn's tool round"
    );
}
