//! The approval loop, end to end.
//!
//! The property under test is that a prompted call is *answered*. mentra's
//! session authorizer blocks the turn on a oneshot until someone resolves the
//! request, so a harness that emits `permission_requested` without resolving
//! it does not merely lose a feature — it hangs. These tests fail by timing
//! out, which is exactly the failure they exist to catch.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use lan::{
    ApprovalDecision, ApprovalPolicy, ApprovalRequest, Approver, CollectingSink, Event, RunConfig,
    approval::PolicyAuthorizer, run::prepare_with_session,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
};
use serde_json::json;

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

/// A runtime whose first turn writes a file — a consequential call, so the
/// policy has something to decide about.
fn runtime_writing_a_file(workspace: &Path, policy: ApprovalPolicy) -> (Runtime, ModelInfo) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            vec![ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "files".to_string(),
                input: json!({
                    "operations": [
                        { "op": "create", "path": "made.txt", "content": "hi" }
                    ]
                }),
            }],
            vec![ContentBlock::text("done")],
        ],
    );

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(PolicyAuthorizer::new(policy))
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
    RunConfig::new(workspace, "make a file").with_context(lan::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

/// Answers with a fixed decision and records what it was asked.
struct ScriptedApprover {
    decision: ApprovalDecision,
    seen: Arc<Mutex<Vec<ApprovalRequest>>>,
}

impl Approver for ScriptedApprover {
    fn approve(&mut self, request: &ApprovalRequest) -> ApprovalDecision {
        self.seen
            .lock()
            .expect("not poisoned")
            .push(request.clone());
        self.decision
    }
}

async fn run_with(
    workspace: &Path,
    policy: ApprovalPolicy,
    decision: ApprovalDecision,
) -> (Vec<Event>, Vec<ApprovalRequest>) {
    let (runtime, model) = runtime_writing_a_file(workspace, policy);
    let session = session(&runtime, workspace, model);
    let seen = Arc::new(Mutex::new(Vec::new()));

    let prepared = prepare_with_session(session, &config(workspace), "openai", "scripted-model")
        .expect("prepared");

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(
            CollectingSink::new(),
            ScriptedApprover {
                decision,
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

fn tool_failed(events: &[Event]) -> Option<bool> {
    events.iter().find_map(|event| match event {
        Event::ToolCompleted { is_error, .. } => Some(*is_error),
        _ => None,
    })
}

#[tokio::test]
async fn a_prompted_call_is_answered_rather_than_left_hanging() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, asked) = run_with(
        workspace.path(),
        ApprovalPolicy::Prompt,
        ApprovalDecision::Allow,
    )
    .await;

    assert_eq!(
        asked.len(),
        1,
        "the write should have been put to the approver"
    );
    assert_eq!(asked[0].tool_name, "files");
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
    assert_eq!(tool_failed(&events), Some(false), "an approved call runs");
    assert!(
        workspace.path().join("made.txt").exists(),
        "an approved write must actually happen"
    );
}

#[tokio::test]
async fn a_refused_call_does_not_happen() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, asked) = run_with(
        workspace.path(),
        ApprovalPolicy::Prompt,
        ApprovalDecision::Deny,
    )
    .await;

    assert_eq!(asked.len(), 1);
    assert_eq!(tool_failed(&events), Some(true), "a refused call fails");
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a refused write must not reach the disk"
    );
}

#[tokio::test]
async fn allowing_everything_asks_nothing() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, asked) = run_with(
        workspace.path(),
        ApprovalPolicy::Always,
        ApprovalDecision::Deny,
    )
    .await;

    assert!(
        asked.is_empty(),
        "nothing should be put to an approver under Always"
    );
    assert_eq!(tool_failed(&events), Some(false));
    assert!(workspace.path().join("made.txt").exists());
}

#[tokio::test]
async fn refusing_everything_needs_no_approver_and_still_terminates() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, asked) = run_with(
        workspace.path(),
        ApprovalPolicy::Never,
        ApprovalDecision::Allow,
    )
    .await;

    assert!(
        asked.is_empty(),
        "Never refuses outright rather than asking and ignoring the answer"
    );
    assert_eq!(tool_failed(&events), Some(true));
    assert!(!workspace.path().join("made.txt").exists());
}

#[tokio::test]
async fn the_approver_is_told_what_the_tool_would_do() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (_, asked) = run_with(
        workspace.path(),
        ApprovalPolicy::Prompt,
        ApprovalDecision::Allow,
    )
    .await;

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
