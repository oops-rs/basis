//! Branching: going back to an earlier point and taking a different path.
//!
//! The assertion that matters is not that `branch_from` returns a number. It is
//! that the *provider request* for the next turn no longer contains the
//! abandoned exchange, and that the abandoned exchange is still in the
//! transcript — checked against `MockRuntime::recorded_requests`, so a
//! regression that quietly truncates history, or quietly keeps sending it,
//! fails here rather than looking fine.
//!
//! Scripted throughout: no provider, no network, no cost.

use std::path::Path;

use lan::{
    AllowAll, BranchError, CollectingSink, EntryKind, Event, RunConfig, run::prepare_with_session,
};
use mentra::{
    Role, RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules").expect("write AGENTS.md");
    dir
}

/// Pinned to the workspace, so an `AGENTS.md` above the temp dir cannot leak
/// in and change what the transcript contains.
fn config(workspace: &Path, prompt: &str) -> RunConfig {
    RunConfig::new(workspace, prompt).with_context(lan::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

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
async fn the_transcript_is_the_active_path_oldest_first() {
    let workspace = workspace();
    let mock = mock(&["hi there"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "hello");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    assert!(
        prepared.transcript().is_empty() && prepared.leaf().is_none(),
        "a conversation with nothing said yet has nowhere to go back to"
    );

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("the turn completes");

    let transcript = prepared.transcript();
    let said: Vec<(&EntryKind, &str)> = transcript
        .iter()
        .map(|entry| (&entry.kind, entry.text.as_str()))
        .collect();

    assert_eq!(
        said,
        vec![
            (&EntryKind::UserTurn, "hello"),
            (&EntryKind::AssistantTurn, "hi there"),
        ]
    );
    assert_eq!(
        prepared.leaf().as_deref(),
        Some(transcript.last().expect("entries").id.as_str()),
        "the leaf is where the next turn continues from"
    );
    assert!(
        prepared.abandoned().is_empty(),
        "nothing has been abandoned yet"
    );
}

#[tokio::test]
async fn branching_back_takes_the_abandoned_exchange_out_of_the_next_request() {
    let workspace = workspace();
    let mock = mock(&["first answer", "second answer", "different answer"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "first");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("the first turn");

    // The end of turn one: everything up to here is kept, everything after it
    // is what "try something else" throws off the path.
    let branch_point = prepared.leaf().expect("a leaf after one turn");

    prepared
        .send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("the second turn");

    let abandoned = prepared
        .branch_from(&branch_point)
        .expect("the branch point is on the active path");
    assert_eq!(
        abandoned, 2,
        "the second turn's prompt and answer both leave the path"
    );

    prepared
        .send("something else", CollectingSink::new(), AllowAll)
        .await
        .expect("the third turn");

    let sent = user_messages(&mock, 2).await;
    assert!(
        sent.contains(&"first".to_string()),
        "everything before the branch point is still the conversation: {sent:?}"
    );
    assert!(
        !sent.contains(&"second".to_string()),
        "the abandoned turn must not be sent again: {sent:?}"
    );
    assert!(
        sent.contains(&"something else".to_string()),
        "the new path continues from the branch point: {sent:?}"
    );
}

#[tokio::test]
async fn the_abandoned_path_stays_reachable() {
    let workspace = workspace();
    let mock = mock(&["first answer", "second answer", "different answer"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "first");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("the first turn");
    let branch_point = prepared.leaf().expect("a leaf");

    prepared
        .send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("the second turn");
    prepared
        .branch_from(&branch_point)
        .expect("the branch succeeds");
    prepared
        .send("something else", CollectingSink::new(), AllowAll)
        .await
        .expect("the third turn");

    // This is the whole difference between branching and truncating: both
    // paths hang off the same entry, and a client can show either.
    let children = prepared.children(&branch_point);
    let texts: Vec<&str> = children.iter().map(|entry| entry.text.as_str()).collect();

    assert_eq!(
        children.len(),
        2,
        "two paths were explored from one point: {texts:?}"
    );
    assert!(
        texts.contains(&"second"),
        "the abandoned path is still there"
    );
    assert!(texts.contains(&"something else"), "so is the new one");

    let abandoned: Vec<String> = prepared
        .abandoned()
        .into_iter()
        .map(|entry| entry.text)
        .collect();
    assert!(
        abandoned.contains(&"second".to_string())
            && abandoned.contains(&"second answer".to_string()),
        "the abandoned exchange is kept, not deleted: {abandoned:?}"
    );
}

#[tokio::test]
async fn an_entry_this_conversation_does_not_have_is_refused() {
    let workspace = workspace();
    let mock = mock(&["answer"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "first");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("the first turn");
    let before = prepared.transcript();

    let error = prepared
        .branch_from("entry-nobody-issued")
        .expect_err("an id lan cannot find is an id lan will not act on");

    assert_eq!(
        error,
        BranchError::UnknownEntry("entry-nobody-issued".to_string())
    );
    assert_eq!(
        prepared.transcript(),
        before,
        "a refused branch must leave the conversation alone"
    );
    assert!(
        prepared.children("entry-nobody-issued").is_empty(),
        "asking about an id lan does not have is not an error, just an empty answer"
    );
}

#[tokio::test]
async fn an_abandoned_entry_is_offered_and_accepted() {
    let workspace = workspace();
    let mock = mock(&["first answer", "second answer"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "first");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("the first turn");
    let branch_point = prepared.leaf().expect("a leaf");

    prepared
        .send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("the second turn");
    prepared
        .branch_from(&branch_point)
        .expect("the branch succeeds");

    let left_behind = prepared
        .abandoned()
        .first()
        .expect("the second turn left the path")
        .id
        .clone();

    // What `abandoned()` lists is what a client offers, so it has to be what
    // `branch_from` accepts. Until mentra 0.16 it was not: lan showed the
    // entry and then refused it, which is the worst of both.
    prepared
        .branch_from(&left_behind)
        .expect("an entry a client can see is an entry it can return to");
}

#[tokio::test]
async fn a_branch_announces_itself_on_the_session_stream() {
    let workspace = workspace();
    let mock = mock(&["answer"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "first");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("the first turn");

    let first_entry = prepared.transcript()[0].id.clone();
    let mut events = prepared.session().subscribe();

    let abandoned = prepared
        .branch_from(&first_entry)
        .expect("the first entry is on the path");

    // Nothing is streaming between turns, so a host that wants to hear about a
    // branch subscribes itself. The event it gets is lan's own, mapped by the
    // same code every other event goes through.
    let branched = loop {
        let event = events.try_recv().expect("the branch was announced");
        if let Some(event @ Event::Branched { .. }) = Event::from_session_event(&event) {
            break event;
        }
    };

    assert_eq!(
        branched,
        Event::Branched {
            entry_id: first_entry,
            abandoned_entries: abandoned,
        }
    );
}

#[tokio::test]
async fn a_tool_round_is_one_entry_per_exchange() {
    let workspace = workspace();
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .tool_calls(vec![MockToolCall::new(
            "files",
            serde_json::json!({"operations": [{"op": "list", "path": "."}]}),
        )])
        .text("listed them")
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

    let config = config(workspace.path(), "list the files");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("the turn completes");

    // A tool round is a point the conversation can be rewound to, so it has to
    // appear in the transcript as its own entry rather than being folded into
    // the assistant's answer.
    assert!(
        prepared
            .transcript()
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::ToolExchange { .. })),
        "a tool exchange is a branch point like any other: {:?}",
        prepared.transcript()
    );
}

#[tokio::test]
async fn a_conversation_can_return_to_an_abandoned_branch() {
    let workspace = workspace();
    let mock = mock(&[
        "first answer",
        "second answer",
        "different answer",
        "back again",
    ]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "first");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    prepared
        .execute(CollectingSink::new())
        .await
        .expect("the first turn");
    let fork = prepared.leaf().expect("a leaf after one turn");

    prepared
        .send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("the second turn");
    // Where the original line of work ended, before anything was abandoned.
    let original = prepared.leaf().expect("a leaf after two turns");

    // Leave it and explore elsewhere.
    prepared.branch_from(&fork).expect("branches away");
    prepared
        .send("elsewhere", CollectingSink::new(), AllowAll)
        .await
        .expect("the third turn");

    // Now go back. This is what lan refused to attempt before mentra 0.16:
    // the entry is archived, and `branch_from` could only reach the active
    // path.
    prepared
        .branch_from(&original)
        .expect("an abandoned branch can be returned to");

    prepared
        .send("carry on", CollectingSink::new(), AllowAll)
        .await
        .expect("the fourth turn");

    // The request is the proof: the original path is the conversation again,
    // and the exploration is not in it.
    let sent = user_messages(&mock, 3).await;
    assert!(
        sent.contains(&"first".to_string()) && sent.contains(&"second".to_string()),
        "the returned-to path carries both of its turns: {sent:?}"
    );
    assert!(
        !sent.contains(&"elsewhere".to_string()),
        "the abandoned exploration must not come back with it: {sent:?}"
    );
    assert!(
        sent.contains(&"carry on".to_string()),
        "and the conversation continues from there: {sent:?}"
    );
}
