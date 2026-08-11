//! The approval loop, end to end.
//!
//! The property under test is that a consequential call is *answered*. mentra's
//! session authorizer blocks the turn on a oneshot until someone resolves the
//! request, so a harness that emits `permission_requested` without resolving
//! it does not merely lose a feature — it hangs. These tests fail by timing
//! out, which is exactly the failure they exist to catch.
//!
//! There is no policy to configure any more (ADR-0010): the gate surfaces every
//! consequential call and the approver answers all of it. So these drive the
//! approvers lan actually ships, rather than a stand-in for an enum that no
//! longer exists.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use lan_core::{
    AllowAll, ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, CollectingSink, DenyAll,
    Event, RunConfig, approval::ApprovalGate, run::prepare_with_session,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::SqliteRuntimeStore,
};
use serde_json::json;

mod common;

/// Every run here must finish well inside this; exceeding it means a request
/// went unanswered and the turn is stuck.
const NOT_STUCK: Duration = Duration::from_secs(10);

/// Replays a fixed script of assistant turns.
struct ScriptedProvider {
    model: ModelInfo,
    turns: Mutex<VecDeque<Vec<ContentBlock>>>,
}

impl ScriptedProvider {
    fn new(model: ModelInfo, turns: Vec<Vec<ContentBlock>>) -> Self {
        Self {
            model,
            turns: Mutex::new(turns.into()),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let content = self
            .turns
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| vec![ContentBlock::text("done")]);

        Ok(provider_event_stream_from_response(Response {
            id: "scripted".to_string(),
            model: self.model.id.clone(),
            role: Role::Assistant,
            content,
            stop_reason: None,
            usage: None,
        }))
    }
}

/// A runtime whose first turn reads something and writes a file — one call the
/// gate lets through and one it must put to the approver.
fn runtime_writing_a_file(workspace: &Path) -> (Runtime, ModelInfo) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            vec![
                ContentBlock::ToolUse {
                    id: "call-0".to_string(),
                    name: "check_background".to_string(),
                    input: json!({}),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "files".to_string(),
                    input: json!({
                        "operations": [
                            { "op": "create", "path": "made.txt", "content": "hi" }
                        ]
                    }),
                },
            ],
            vec![ContentBlock::text("done")],
        ],
    );

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(SqliteRuntimeStore::new(
            common::scratch_store().join("runtime.sqlite"),
        ))
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(ApprovalGate::new())
        .build()
        .expect("runtime builds");

    (runtime, model)
}

fn session(runtime: &Runtime, workspace: &Path, model: ModelInfo) -> Session {
    runtime
        .create_session_with_config(
            "test",
            model,
            mentra::agent::AgentConfig {
                workspace: mentra::agent::WorkspaceConfig {
                    base_dir: workspace.to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session")
}

fn config(workspace: &Path) -> RunConfig {
    RunConfig::new(workspace, "make a file").with_context(lan_core::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

/// Records what it was asked, then lets the approver under test answer.
struct Recording<A> {
    inner: A,
    seen: Arc<Mutex<Vec<ApprovalRequest>>>,
}

#[async_trait]
impl<A: Approver> Approver for Recording<A> {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        self.seen
            .lock()
            .expect("not poisoned")
            .push(request.clone());
        self.inner.approve(request).await
    }
}

/// Runs the scripted turn under `approver`, reporting the stream and every
/// request the approver was put.
async fn run_with<A: Approver>(
    workspace: &Path,
    approver: A,
) -> (Vec<Event>, Vec<ApprovalRequest>) {
    let (runtime, model) = runtime_writing_a_file(workspace);
    let session = session(&runtime, workspace, model);
    let seen = Arc::new(Mutex::new(Vec::new()));

    let mut prepared =
        prepare_with_session(session, &config(workspace), "openai", "scripted-model")
            .expect("prepared");

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(
            CollectingSink::new(),
            Recording {
                inner: approver,
                seen: Arc::clone(&seen),
            },
        ),
    )
    .await
    .expect("the run must not hang waiting on an unanswered approval")
    .expect("the run completes");

    let asked = seen.lock().expect("not poisoned").clone();
    (report.sink.into_events(), asked)
}

/// Whether the named tool reported an error, or `None` if it never completed.
fn tool_failed(events: &[Event], tool: &str) -> Option<bool> {
    events.iter().find_map(|event| match event {
        Event::ToolCompleted {
            tool_name,
            is_error,
            ..
        } if tool_name == tool => Some(*is_error),
        _ => None,
    })
}

/// The result text the named tool produced — the same string the model reads
/// back as that call's outcome.
fn tool_result(events: &[Event], tool: &str) -> Option<String> {
    events.iter().find_map(|event| match event {
        Event::ToolCompleted {
            tool_name, summary, ..
        } if tool_name == tool => Some(summary.clone()),
        _ => None,
    })
}

fn asked_about(asked: &[ApprovalRequest]) -> Vec<&str> {
    asked.iter().map(|request| &*request.tool_name).collect()
}

#[tokio::test]
async fn an_approved_call_happens_rather_than_hanging() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, asked) = run_with(workspace.path(), AllowAll).await;

    assert_eq!(
        asked_about(&asked),
        vec!["files"],
        "the write should have been put to the approver"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "the request must also reach the stream"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::PermissionResolved { .. })),
        "and its resolution must too"
    );
    assert_eq!(
        tool_failed(&events, "files"),
        Some(false),
        "an approved call runs"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "an approved write must actually happen"
    );
}

#[tokio::test]
async fn a_refused_call_does_not_happen() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, asked) = run_with(workspace.path(), DenyAll).await;

    assert_eq!(asked_about(&asked), vec!["files"]);
    assert_eq!(
        tool_failed(&events, "files"),
        Some(true),
        "a refused call fails, and the model reads why"
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a refused write must not reach the disk"
    );
}

#[tokio::test]
async fn a_refusal_tells_the_model_what_the_run_does_not_allow() {
    // The whole point of the reason: it is the tool result the model reads,
    // so a read-only run says so once instead of watching the model retry the
    // same write. Pinned verbatim because paraphrase here is a silent
    // regression — the string is the interface.
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, _asked) = run_with(workspace.path(), DenyAll).await;

    assert_eq!(
        tool_result(&events, "files").as_deref(),
        Some(
            "Tool execution denied: files changes state outside this process, \
             which this run does not allow"
        )
    );
}

#[tokio::test]
async fn a_refusal_with_nothing_to_say_still_refuses() {
    // An approver that gives no reason is still fail-closed; the model just
    // gets mentra's standing wording rather than lan's.
    struct Silent;

    #[async_trait]
    impl Approver for Silent {
        async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
            ApprovalAnswer::new(ApprovalDecision::Deny)
        }
    }

    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, _asked) = run_with(workspace.path(), Silent).await;

    assert_eq!(
        tool_result(&events, "files").as_deref(),
        Some("Tool execution denied: denied by session approver")
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a refused write must not reach the disk, reason or no reason"
    );
}

#[tokio::test]
async fn a_read_only_call_is_never_put_to_the_approver() {
    let workspace = tempfile::tempdir().expect("tempdir");

    // Under the strictest approver there is: a read that reached it would be
    // denied, so this catches both halves of the rule at once.
    let (events, asked) = run_with(workspace.path(), DenyAll).await;

    assert!(
        !asked_about(&asked).contains(&"check_background"),
        "prompting for reads trains people to approve without reading: {:?}",
        asked_about(&asked)
    );
    assert_eq!(
        tool_failed(&events, "check_background"),
        Some(false),
        "and a read must still run while everything else is refused"
    );
}

#[tokio::test]
async fn a_run_with_no_approver_of_its_own_allows_what_it_cannot_ask_about() {
    // What `run` gives a headless caller: nobody to ask, so nothing is refused
    // for want of an answer. `execute` is `execute_with_approver(_, AllowAll)`.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (runtime, model) = runtime_writing_a_file(workspace.path());
    let session = session(&runtime, workspace.path(), model);

    let mut prepared = prepare_with_session(
        session,
        &config(workspace.path()),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let report = tokio::time::timeout(NOT_STUCK, prepared.execute(CollectingSink::new()))
        .await
        .expect("the run must not hang waiting on an unanswered approval")
        .expect("the run completes");

    assert_eq!(
        tool_failed(&report.sink.into_events(), "files"),
        Some(false)
    );
    assert!(workspace.path().join("made.txt").exists());
}

#[tokio::test]
async fn a_broken_sink_stops_the_narration_and_not_the_turn() {
    // `lan run --json | head`: stdout closes mid-run. Every consequential call
    // now waits on the task that writes those events, so a forwarder that gave
    // up on the first failed write would leave the turn blocked on a permission
    // nobody was left to answer.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (runtime, model) = runtime_writing_a_file(workspace.path());
    let session = session(&runtime, workspace.path(), model);

    let mut written = 0;
    let sink = lan_core::run::FnSink::new(move |_event| {
        written += 1;
        match written {
            // The header goes through: a run whose first write fails never
            // starts, which is a different story than this one.
            1 => Ok(()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the reader went away",
            )),
        }
    });

    let mut prepared = prepare_with_session(
        session,
        &config(workspace.path()),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let result = tokio::time::timeout(NOT_STUCK, prepared.execute_with_approver(sink, AllowAll))
        .await
        .expect("a dead reader must not hang the run");

    assert!(
        matches!(result, Err(lan_core::RunError::Sink(_))),
        "the broken pipe is still reported"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "and the approved write happened anyway"
    );
}

#[tokio::test]
async fn the_approver_is_told_what_the_tool_would_do() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (_, asked) = run_with(workspace.path(), AllowAll).await;

    let request = &asked[0];
    assert!(!request.request_id.is_empty());
    assert_eq!(request.tool_call_id, "call-1");
    assert!(
        request.description.contains("files"),
        "the description should name the tool: {:?}",
        request.description
    );
    assert_eq!(
        request.input["operations"][0]["op"], "create",
        "input must arrive as JSON so an approver can show what changes"
    );
}
