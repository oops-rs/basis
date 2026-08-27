//! Between-turn model and reasoning switching on one attached `PreparedRun`.
//!
//! This is deliberately a Basis-only public API oracle. It drives a real tool
//! round before switching phases so the synthesis request proves that the
//! session, agent, transcript, and complete request posture survived intact.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use basis::{
    AllowAll, CollectingSink, ContentBlock, Effort, Event, ModelInfo, Provider,
    ProviderRequestOptions, ReasoningOptions, ReasoningSummary, RunError, RunProfile, RunSpec,
    Runtime, Workspace, async_trait,
    runtime::{
        ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream, Request,
        Response, Role, provider_event_stream_from_response,
    },
    tools::{ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult},
};
use serde_json::json;

const PROVIDER: &str = "attached-switch-provider";
const MODEL_A: &str = "gather-model";
const MODEL_B: &str = "synthesis-model";
const TOOL: &str = "fake_retrieval";
const TOOL_CALL: &str = "gather-call";
const TOOL_RESULT: &str = "evidence from the fake retrieval tool";

#[derive(Clone, Default)]
struct Activity {
    listings: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Request<'static>>>>,
    tool_agents: Arc<Mutex<Vec<String>>>,
}

impl Activity {
    fn snapshot(&self) -> ActivitySnapshot {
        ActivitySnapshot {
            listings: self.listings.load(Ordering::SeqCst),
            requests: self.requests.lock().expect("request recorder").len(),
            tools: self.tool_agents.lock().expect("tool recorder").len(),
        }
    }

    fn requests(&self) -> Vec<Request<'static>> {
        self.requests.lock().expect("request recorder").clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivitySnapshot {
    listings: usize,
    requests: usize,
    tools: usize,
}

struct ScriptedProvider {
    activity: Activity,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(PROVIDER)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.activity.listings.fetch_add(1, Ordering::SeqCst);
        Ok(vec![model_a(), model_b()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let model = request.model.to_string();
        let call = {
            let mut requests = self.activity.requests.lock().expect("request recorder");
            requests.push(request.into_owned());
            requests.len()
        };
        let content = match call {
            1 => vec![ContentBlock::ToolUse {
                id: TOOL_CALL.to_string(),
                name: TOOL.to_string(),
                input: json!({"topic": "runtime adoption"}),
            }],
            2 => vec![ContentBlock::text("gather complete")],
            _ => vec![ContentBlock::text("synthesis complete")],
        };

        Ok(provider_event_stream_from_response(Response {
            id: format!("attached-switch-{call}"),
            model,
            role: Role::Assistant,
            content,
            stop_reason: None,
            usage: None,
        }))
    }
}

struct FakeRetrieval {
    activity: Activity,
}

impl ToolDefinition for FakeRetrieval {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(TOOL)
            .description("returns fixed evidence for the attached-switch oracle")
            .input_schema(json!({
                "type": "object",
                "properties": {"topic": {"type": "string"}},
                "required": ["topic"]
            }))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for FakeRetrieval {
    async fn execute(&self, ctx: ParallelToolContext, _input: serde_json::Value) -> ToolResult {
        self.activity
            .tool_agents
            .lock()
            .expect("tool recorder")
            .push(ctx.agent_id);
        Ok(TOOL_RESULT.to_string())
    }
}

fn model_a() -> ModelInfo {
    let mut model = ModelInfo::new(MODEL_A, PROVIDER).with_context_window(32_768);
    model.display_name = Some("Gather display metadata".to_string());
    model.description = Some("metadata handed to Mentra whole".to_string());
    model
}

fn model_b() -> ModelInfo {
    let mut model = ModelInfo::new(MODEL_B, PROVIDER).with_context_window(262_144);
    model.display_name = Some("Synthesis display metadata".to_string());
    model.description = Some("metadata handed to Mentra whole".to_string());
    model
}

fn low_reasoning() -> ReasoningOptions {
    ReasoningOptions {
        effort: Some(Effort::Low.into()),
        summary: None,
    }
}

fn high_reasoning() -> ReasoningOptions {
    ReasoningOptions {
        effort: Some(Effort::High.into()),
        summary: Some(ReasoningSummary::Detailed),
    }
}

fn gather_options() -> ProviderRequestOptions {
    let mut options = ProviderRequestOptions {
        reasoning: Some(low_reasoning()),
        ..Default::default()
    };
    options.responses.parallel_tool_calls = Some(false);
    options.responses.include = vec!["reasoning.encrypted_content".to_string()];
    options.responses.service_tier = Some("priority".to_string());
    options.responses.prompt_cache_key = Some("attached-switch-cache".to_string());
    options.anthropic.disable_parallel_tool_use = Some(true);
    options.gemini.thoughts = Some(true);
    options.session.sticky_turn_state = Some("attached-switch-turn".to_string());
    options.session.turn_metadata = Some("attached-switch-metadata".to_string());
    options.session.prefer_connection_reuse = Some(true);
    options
}

async fn workspace(path: &std::path::Path, activity: Activity) -> Workspace {
    Workspace::builder(path)
        .without_discovery()
        .with_runtime_builder(
            Runtime::builder()
                .with_provider_instance(ScriptedProvider {
                    activity: activity.clone(),
                })
                .with_tool(FakeRetrieval { activity })
                .with_ephemeral_history(),
        )
        .with_resolved_model(model_a())
        .open()
        .await
        .expect("the private discovery-free workspace opens")
}

#[tokio::test]
async fn one_attached_run_switches_full_model_and_reasoning_without_losing_gather_context() {
    let activity = Activity::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = workspace(dir.path(), activity.clone()).await;
    let options = gather_options();
    let mut run = workspace
        .prepare(
            RunSpec::new("gather evidence")
                .with_profile(RunProfile::new().with_provider_request_options(options.clone())),
        )
        .expect("the attached run mints without provider activity");
    let session_id = run.session_id();
    let agent_id = run.agent_id().to_string();

    let gather = run
        .execute(CollectingSink::default())
        .await
        .expect("the gather tool exchange completes");
    assert_eq!(gather.model, MODEL_A);
    assert_eq!(gather.session_id, session_id);
    assert_eq!(run.context_window(), Some(32_768));
    assert_eq!(run.reasoning(), Some(&low_reasoning()));
    let gather_history = run.history().to_vec();
    assert!(gather_history.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolUse { id, name, .. } if id == TOOL_CALL && name == TOOL
            )
        })
    }));
    assert!(gather_history.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error: false,
                } if tool_use_id == TOOL_CALL && content == TOOL_RESULT
            )
        })
    }));
    assert_eq!(
        activity
            .tool_agents
            .lock()
            .expect("tool recorder")
            .as_slice(),
        [agent_id.as_str()]
    );
    let after_gather = activity.snapshot();
    assert_eq!(
        after_gather,
        ActivitySnapshot {
            listings: 0,
            requests: 2,
            tools: 1,
        }
    );

    run.set_resolved_model(model_b())
        .expect("the exact-provider model switch persists");
    run.set_reasoning(Some(high_reasoning()))
        .expect("the complete reasoning switch persists");

    assert_eq!(
        activity.snapshot(),
        after_gather,
        "switching itself must not list, request, or run a tool"
    );
    assert_eq!(run.session_id(), session_id);
    assert_eq!(run.agent_id(), agent_id);
    assert_eq!(run.session().metadata().model, MODEL_B);
    assert_eq!(run.context().provider, PROVIDER);
    assert_eq!(run.context().model, MODEL_B);
    assert_eq!(run.context_window(), Some(262_144));
    assert_eq!(run.reasoning(), Some(&high_reasoning()));
    assert!(matches!(
        run.header(),
        Event::RunStarted {
            ref model,
            ref provider,
            ..
        } if model == MODEL_B && provider == PROVIDER
    ));

    let synthesis = run
        .send(
            "synthesize from the gathered evidence",
            CollectingSink::default(),
            AllowAll,
        )
        .await
        .expect("synthesis completes on the same attached run");
    assert_eq!(synthesis.session_id, session_id);
    assert_eq!(synthesis.model, MODEL_B);
    assert_eq!(synthesis.provider, PROVIDER);
    assert_eq!(gather.model, MODEL_A, "the earlier report stays historical");
    assert_eq!(run.agent_id(), agent_id);

    let requests = activity.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].model, MODEL_A);
    assert_eq!(requests[1].model, MODEL_A);
    assert_eq!(requests[0].provider_request_options, options);
    assert_eq!(requests[1].provider_request_options, options);
    let synthesis_request = &requests[2];
    assert_eq!(synthesis_request.model, MODEL_B);
    assert!(
        synthesis_request
            .messages
            .as_ref()
            .starts_with(&gather_history),
        "the model-B request must replay the exact committed gather transcript"
    );
    let mut expected_options = options;
    expected_options.reasoning = Some(high_reasoning());
    assert_eq!(synthesis_request.provider_request_options, expected_options);
}

#[tokio::test]
async fn legacy_switch_wrappers_are_lossy_only_for_model_metadata_and_reasoning_summary() {
    const WRAPPER_MODEL: &str = "wrapper-model";

    let activity = Activity::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = workspace(dir.path(), activity.clone()).await;
    let mut options = gather_options();
    options.reasoning = Some(ReasoningOptions {
        effort: Some(Effort::Low.into()),
        summary: Some(ReasoningSummary::Concise),
    });
    let mut run = workspace
        .prepare(
            RunSpec::new("wrapper gather")
                .with_profile(RunProfile::new().with_provider_request_options(options.clone())),
        )
        .expect("the wrapper run mints without provider activity");
    let before = activity.snapshot();

    run.set_model(WRAPPER_MODEL)
        .expect("the legacy model-id wrapper switches on the same provider");
    run.set_effort(Some(Effort::Medium))
        .expect("the legacy effort wrapper switches reasoning");

    assert_eq!(
        activity.snapshot(),
        before,
        "legacy wrappers must not list, request, or run a tool"
    );
    assert_eq!(
        before,
        ActivitySnapshot {
            listings: 0,
            requests: 0,
            tools: 0,
        }
    );
    assert_eq!(run.context_window(), None, "an id carries no model window");
    let expected_reasoning = ReasoningOptions {
        effort: Some(Effort::Medium.into()),
        summary: None,
    };
    assert_eq!(run.reasoning(), Some(&expected_reasoning));
    assert_eq!(run.effort(), Some(Effort::Medium));
    assert!(matches!(
        run.header(),
        Event::RunStarted {
            ref model,
            ref provider,
            ..
        } if model == WRAPPER_MODEL && provider == PROVIDER
    ));

    let report = run
        .execute(CollectingSink::default())
        .await
        .expect("the wrapper-model gather completes");
    assert_eq!(report.model, WRAPPER_MODEL);
    assert_eq!(report.provider, PROVIDER);
    let mut expected_options = options;
    expected_options.reasoning = Some(expected_reasoning);
    let requests = activity.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.model, WRAPPER_MODEL);
        assert_eq!(request.provider_request_options, expected_options);
    }
}

#[tokio::test]
async fn a_foreign_provider_switch_fails_before_touching_the_attached_run() {
    let activity = Activity::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = workspace(dir.path(), activity.clone()).await;
    let options = gather_options();
    let mut run = workspace
        .prepare(
            RunSpec::new("not sent")
                .with_profile(RunProfile::new().with_provider_request_options(options)),
        )
        .expect("the attached run mints without provider activity");
    let session_id = run.session_id();
    let agent_id = run.agent_id().to_string();
    let history = run.history().to_vec();
    let reasoning = run.reasoning().cloned();
    let before = activity.snapshot();

    let error = run
        .set_resolved_model(
            ModelInfo::new(MODEL_B, "attached-switch-provider-lookalike")
                .with_context_window(999_999),
        )
        .expect_err("provider identity is exact, not inferred from the model id");

    assert!(matches!(
        error,
        RunError::ResolvedModelProviderMismatch {
            ref model,
            ref model_provider,
            ref runtime_provider,
        } if model == MODEL_B
            && model_provider == "attached-switch-provider-lookalike"
            && runtime_provider == PROVIDER
    ));
    assert_eq!(activity.snapshot(), before);
    assert_eq!(
        before,
        ActivitySnapshot {
            listings: 0,
            requests: 0,
            tools: 0,
        }
    );
    assert_eq!(run.session_id(), session_id);
    assert_eq!(run.agent_id(), agent_id);
    assert_eq!(run.session().metadata().model, MODEL_A);
    assert_eq!(run.context().model, MODEL_A);
    assert_eq!(run.context().provider, PROVIDER);
    assert_eq!(run.context_window(), Some(32_768));
    assert_eq!(run.reasoning().cloned(), reasoning);
    assert_eq!(run.history(), history.as_slice());
    assert!(matches!(
        run.header(),
        Event::RunStarted {
            ref model,
            ref provider,
            ..
        } if model == MODEL_A && provider == PROVIDER
    ));
}
