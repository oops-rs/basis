//! A turn cut off while its call waited on the approver, picked back up.
//!
//! The process "dies" by dropping the turn's future mid-approval, which
//! leaves the store exactly where a crash would: the call committed, its
//! result never written. A resumed run must see that turn as interrupted —
//! not answered — and finishing it must go through the same approver,
//! events and report as any turn.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use basis::{
    AllowAll, ApprovalAnswer, ApprovalRequest, Approver, CollectingSink, Event, RunOutcome,
    TurnOptions, run::prepare_with_session,
};
use mentra::{ContentBlock, ModelInfo, Runtime, RuntimePolicy, runtime::VolatileRuntimeStore};
use serde_json::json;

use super::{NOT_STUCK, Recording, ScriptedProvider, asked_about, context, tool_failed};

/// Never answers: the turn parks on it until its future is dropped.
struct Parked;

#[async_trait]
impl Approver for Parked {
    async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
        std::future::pending().await
    }
}

fn write_call(id: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::ToolUse {
        id: id.to_string(),
        name: "files".to_string(),
        input: json!({
            "operations": [{ "op": "create", "path": "made.txt", "content": "hi" }]
        }),
    }]
}

fn runtime(
    store: &VolatileRuntimeStore,
    workspace: &std::path::Path,
    model: &ModelInfo,
    turns: Vec<Vec<ContentBlock>>,
) -> Runtime {
    Runtime::builder()
        .with_provider_instance(ScriptedProvider::new(model.clone(), turns))
        .with_store(store.clone())
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(basis::approval::ApprovalGate::new())
        .build()
        .expect("runtime builds")
}

#[tokio::test]
async fn a_turn_cut_off_while_asking_is_continued_and_asks_again() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let store = VolatileRuntimeStore::new();
    let model = ModelInfo::new("scripted-model", mentra::BuiltinProvider::OpenAI);

    let agent_id = {
        let first = runtime(&store, workspace.path(), &model, vec![write_call("call-1")]);
        let session = super::session(&first, workspace.path(), model.clone());
        let agent_id = session.agent_id().to_string();
        let mut prepared = prepare_with_session(
            session,
            workspace.path(),
            "make a file",
            &context(),
            "openai",
            "scripted-model",
        )
        .expect("prepared");
        let parked = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            prepared.execute_with_approver(CollectingSink::new(), Parked),
        )
        .await;
        assert!(parked.is_err(), "the turn parks on the approver");
        agent_id
    };
    assert!(!workspace.path().join("made.txt").exists());

    let second = runtime(
        &store,
        workspace.path(),
        &model,
        vec![write_call("call-2"), vec![ContentBlock::text("made it")]],
    );
    let session = second.resume_session(&agent_id).expect("resumes");
    let mut resumed = prepare_with_session(
        session,
        workspace.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");
    assert!(resumed.has_interrupted_turn());
    assert_eq!(
        resumed.answered_turns(),
        1,
        "the count still sees the call's message, which is why it is not the test"
    );

    let seen = Arc::new(Mutex::new(Vec::new()));
    let report = tokio::time::timeout(
        NOT_STUCK,
        resumed.continue_interrupted_with_options(
            CollectingSink::new(),
            Recording {
                inner: AllowAll,
                seen: Arc::clone(&seen),
            },
            TurnOptions::default(),
        ),
    )
    .await
    .expect("the continuation does not hang")
    .expect("the continuation runs");

    assert_eq!(report.outcome, RunOutcome::Ok);
    assert_eq!(report.final_message.as_deref(), Some("made it"));
    assert_eq!(
        asked_about(&seen.lock().expect("not poisoned")),
        ["files"],
        "the call is asked about afresh, never assumed approved"
    );
    let events = report.sink.into_events();
    assert!(matches!(events.first(), Some(Event::RunStarted { .. })));
    assert!(matches!(events.last(), Some(Event::RunFinished { .. })));
    assert_eq!(tool_failed(&events, "files"), Some(false));
    assert!(workspace.path().join("made.txt").exists());
    assert!(!resumed.has_interrupted_turn());
    assert!(
        resumed
            .history()
            .iter()
            .any(|message| {
                message.content.iter().any(|block| matches!(
            block,
            ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "call-1"
        ))
            }),
        "the cut-off call was closed as unknown, not run"
    );
}

#[tokio::test]
async fn continuing_a_finished_conversation_fails_the_turn() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let store = VolatileRuntimeStore::new();
    let model = ModelInfo::new("scripted-model", mentra::BuiltinProvider::OpenAI);
    let runtime = runtime(&store, workspace.path(), &model, Vec::new());
    let session = super::session(&runtime, workspace.path(), model);
    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");
    assert!(!prepared.has_interrupted_turn());

    let report = prepared
        .continue_interrupted_with_options(CollectingSink::new(), AllowAll, TurnOptions::default())
        .await
        .expect("reports");
    assert!(
        matches!(&report.outcome, RunOutcome::Error { message } if message.contains("no interrupted turn")),
        "{:?}",
        report.outcome
    );
}
