//! A turn's locals reach its tools (ADR-0044): one runtime and one toolset
//! serve many callers, and each turn's tools read that turn's caller — the
//! run's configured locals as the floor, the turn's on top, and a delegated
//! child's tools reading what its parent's did.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use basis::{
    AllowAll, CollectingSink, RunLocals, SpawnTool, TurnOptions,
    approval::ApprovalGate,
    run::prepare_with_session,
    tools::{SPAWN, ToolContext, ToolDefinition, ToolExecutor, ToolResult},
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy,
    agent::{AgentConfig, ToolProfile, WorkspaceConfig},
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::VolatileRuntimeStore,
    tool::ToolSpec,
};
use serde_json::json;

const WHO: &str = "who_am_i_for";

/// Who a turn works for: the host's own type, so no other host's value can
/// be mistaken for it.
#[derive(Debug)]
struct Caller(&'static str);

/// Another of the host's facts, to show layering keeps what it does not
/// replace.
#[derive(Debug)]
struct Tenant(&'static str);

/// Answers with the caller and tenant its run carries, and keeps each answer.
#[derive(Clone, Default)]
struct WhoTool(Arc<Mutex<Vec<String>>>);

impl WhoTool {
    fn seen(&self) -> Vec<String> {
        self.0.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl ToolDefinition for WhoTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(WHO)
            .description("Say who this run works for.")
            .input_schema(json!({"type": "object", "properties": {}}))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for WhoTool {
    async fn execute_mut(&self, ctx: ToolContext<'_>, _input: serde_json::Value) -> ToolResult {
        let caller = ctx.run_local::<Caller>().map_or("nobody", |c| c.0);
        let tenant = ctx.run_local::<Tenant>().map_or("none", |t| t.0);
        let answer = format!("{caller}@{tenant}");
        self.0.lock().expect("not poisoned").push(answer.clone());
        Ok(answer)
    }
}

/// Replays assistant rounds in order, for the parent and any child alike.
struct ScriptedProvider {
    model: ModelInfo,
    rounds: Mutex<VecDeque<Vec<ContentBlock>>>,
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
            .rounds
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| vec![ContentBlock::text("done")]);
        Ok(provider_event_stream_from_response(Response {
            id: "scripted".to_string(),
            model: self.model.id.clone(),
            role: Role::Assistant,
            usage: None,
            content,
            stop_reason: None,
        }))
    }
}

fn calling(id: &str, name: &str, input: serde_json::Value) -> Vec<ContentBlock> {
    vec![ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input,
    }]
}

/// One prepared run over a runtime holding `tool` and `spawn`, scripted with
/// `rounds`.
fn prepared(
    workspace: &Path,
    tool: &WhoTool,
    rounds: Vec<Vec<ContentBlock>>,
) -> basis::PreparedRun {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let runtime = Runtime::builder()
        .with_provider_instance(ScriptedProvider {
            model: model.clone(),
            rounds: Mutex::new(rounds.into()),
        })
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(ApprovalGate::new())
        .with_tool(SpawnTool::new())
        .with_tool(tool.clone())
        .build()
        .expect("runtime builds");
    let config = AgentConfig {
        tool_profile: ToolProfile::hide(["shell", "background_run", "task"]),
        workspace: WorkspaceConfig {
            base_dir: workspace.to_path_buf(),
            ..Default::default()
        },
        ..Default::default()
    };
    let session = runtime
        .create_session_with_config("test", model, config)
        .expect("session");
    let context = basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    };
    prepare_with_session(
        session,
        workspace,
        "who is this for?",
        &context,
        "openai",
        "scripted-model",
    )
    .expect("prepared")
}

#[tokio::test]
async fn each_turn_s_tools_read_that_turn_s_locals_over_the_run_s() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tool = WhoTool::default();
    let mut run = prepared(
        workspace.path(),
        &tool,
        vec![
            calling("c1", WHO, json!({})),
            vec![ContentBlock::text("first done")],
            calling("c2", WHO, json!({})),
            vec![ContentBlock::text("second done")],
            calling("c3", WHO, json!({})),
            vec![ContentBlock::text("third done")],
        ],
    )
    .with_bounds(
        TurnOptions::default().with_locals(
            RunLocals::new()
                .with(Caller("run-default"))
                .with(Tenant("acme")),
        ),
    );

    for locals in [
        RunLocals::new().with(Caller("alice")),
        RunLocals::new().with(Caller("bob")),
        RunLocals::new(),
    ] {
        run.send_with_options(
            "who?",
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default().with_locals(locals),
        )
        .await
        .expect("turn completes");
    }

    assert_eq!(
        tool.seen(),
        vec!["alice@acme", "bob@acme", "run-default@acme"],
        "the turn's value wins, the run's fills in what the turn does not say"
    );
}

#[tokio::test]
async fn a_delegated_child_s_tools_read_the_parent_s_locals() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tool = WhoTool::default();
    let mut run = prepared(
        workspace.path(),
        &tool,
        vec![
            // The parent delegates; the child calls the tool, then answers;
            // the parent then answers.
            calling("p1", SPAWN, json!({"input": "find out who this is for"})),
            calling("k1", WHO, json!({})),
            vec![ContentBlock::text("child done")],
            vec![ContentBlock::text("parent done")],
        ],
    );

    run.send_with_options(
        "who?",
        CollectingSink::new(),
        AllowAll,
        TurnOptions::default().with_locals(RunLocals::new().with(Caller("carol"))),
    )
    .await
    .expect("turn completes");

    assert_eq!(tool.seen(), vec!["carol@none"]);
}
