//! Lossless in-process observation through Basis's public run surface.
//!
//! Basis must not translate this stream: hosts use it precisely when the
//! bounded, UI-shaped [`basis::Event`] stream is too lossy for durable evidence.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use basis::{
    AgentEvent, AgentEventTapGuard, AllowAll, CollectingSink, ContentBlock, PreparedRun,
    RunFailure, TurnOptions, provider_core::ToolResultContent, run::prepare_with_session,
};
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockToolCall},
    tool::{
        ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor,
        ToolOutput, ToolSideEffectLevel, ToolSpec,
    },
};
use serde_json::{Value, json};

const FAILED_RESULT: &str = "evidence tool failed with its complete diagnostic";

struct StructuredEvidenceTool;

#[async_trait]
impl ToolDefinition for StructuredEvidenceTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("structured_evidence")
            .description("Return a structured evidence payload")
            .input_schema(json!({ "type": "object" }))
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for StructuredEvidenceTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::structured(structured_result()))
    }
}

struct FailedEvidenceTool;

#[async_trait]
impl ToolDefinition for FailedEvidenceTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("failed_evidence")
            .description("Return a complete tool-level failure")
            .input_schema(json!({ "type": "object" }))
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for FailedEvidenceTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Err(FAILED_RESULT.to_string())
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "preserve evidence").expect("write context");
    dir
}

fn context() -> basis::ContextConfig {
    basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    }
}

fn prepared(mock: &MockRuntime, workspace: &tempfile::TempDir) -> PreparedRun {
    let session = mock
        .runtime()
        .create_session("observer", mock.model())
        .expect("session");
    prepare_with_session(
        session,
        workspace.path(),
        "collect the evidence",
        &context(),
        "openai",
        "mock-model",
    )
    .expect("prepared run")
}

fn observed_events() -> (
    Arc<Mutex<Vec<AgentEvent>>>,
    impl Fn(&AgentEvent) + Send + Sync + 'static,
) {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_tap = Arc::clone(&observed);
    let tap = move |event: &AgentEvent| {
        observed_for_tap
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(event.clone());
    };
    (observed, tap)
}

#[tokio::test]
async fn observer_preserves_complete_tool_payloads_and_occurrence_order() {
    let structured_input = json!({
        "query": "why did the invariant fail?",
        "filters": { "scope": ["docs", "code"], "exact": true }
    });
    let failed_input = json!({ "evidence_id": "ev-2" });
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .tool_calls([
            MockToolCall::new("structured_evidence", structured_input.clone())
                .with_id("call-structured"),
            MockToolCall::new("failed_evidence", failed_input.clone()).with_id("call-failed"),
        ])
        .text("done")
        .build()
        .expect("mock runtime builds");
    mock.runtime().register_tool(StructuredEvidenceTool);
    mock.runtime().register_tool(FailedEvidenceTool);

    let dir = workspace();
    let mut run = prepared(&mock, &dir);
    let (observed, tap) = observed_events();
    let _guard: AgentEventTapGuard = run.register_agent_event_tap(tap);

    let report = run
        .execute(CollectingSink::new())
        .await
        .expect("run completes");
    assert!(report.succeeded());

    let events = observed
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let lifecycle = events
        .iter()
        .filter_map(tool_lifecycle_label)
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        [
            "ready:call-structured",
            "ready:call-failed",
            "started:call-structured",
            "finished:call-structured",
            "started:call-failed",
            "finished:call-failed",
        ],
        "the Basis observer must preserve Mentra's occurrence order"
    );

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolUseReady { call, .. }
            if call.id == "call-structured"
                && call.name == "structured_evidence"
                && call.input == structured_input
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolUseReady { call, .. }
            if call.id == "call-failed"
                && call.name == "failed_evidence"
                && call.input == failed_input
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Structured(content),
                is_error: false,
            },
        } if tool_use_id == "call-structured" && content == &structured_result()
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Text(content),
                is_error: true,
            },
        } if tool_use_id == "call-failed" && content == FAILED_RESULT
    )));
}

fn structured_result() -> Value {
    json!({
        "payload": ["x".repeat(300), "observer-only-structured-tail"],
        "source_refs": ["doc:alpha#L1", "code:beta.rs#L2"]
    })
}

fn tool_lifecycle_label(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::ToolUseReady { call, .. } => Some(format!("ready:{}", call.id)),
        AgentEvent::ToolExecutionStarted { call } => Some(format!("started:{}", call.id)),
        AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult { tool_use_id, .. },
        } => Some(format!("finished:{tool_use_id}")),
        _ => None,
    }
}

#[tokio::test]
async fn cancellation_is_the_terminal_observer_event() {
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .text("not reached")
        .build()
        .expect("mock runtime builds");
    let dir = workspace();
    let mut run = prepared(&mock, &dir);
    let (observed, tap) = observed_events();
    let _guard = run.register_agent_event_tap(tap);
    let (options, cancellation) = TurnOptions::cancellable();
    cancellation.cancel();

    let report = run
        .execute_with_options(CollectingSink::new(), options)
        .await
        .expect("Basis reports the cancelled turn");
    assert!(matches!(report.failure, Some(RunFailure::Cancelled)));

    let events = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert!(matches!(events.first(), Some(AgentEvent::RunStarted)));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFailed { error }) if error == "operation cancelled"
    ));
}

#[tokio::test]
async fn dropping_the_basis_guard_unregisters_the_observer() {
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .text("first")
        .text("second")
        .build()
        .expect("mock runtime builds");
    let dir = workspace();
    let mut run = prepared(&mock, &dir);
    let observed = Arc::new(AtomicUsize::new(0));
    let observed_for_tap = Arc::clone(&observed);
    let guard: AgentEventTapGuard = run.register_agent_event_tap(move |_| {
        observed_for_tap.fetch_add(1, Ordering::SeqCst);
    });

    run.execute(CollectingSink::new())
        .await
        .expect("first turn completes");
    let after_first_turn = observed.load(Ordering::SeqCst);
    assert!(after_first_turn > 0);

    drop(guard);
    run.send("second", CollectingSink::new(), AllowAll)
        .await
        .expect("second turn completes");
    assert_eq!(observed.load(Ordering::SeqCst), after_first_turn);
}
