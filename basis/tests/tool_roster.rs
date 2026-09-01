//! Exact per-run rosters deny hallucinated registered tools before execution.
//!
//! The provider deliberately calls every registered name omitted from one
//! run's allowlist. The runtime must return one typed unavailable result per
//! call without reaching schemas, hooks, subprocesses, skills, or executors.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(feature = "mcp")]
use basis::McpConfig;
use basis::{
    CollectingSink, Config, ContentBlock, ContextConfig, HookOutcome, HookRequest, HooksConfig,
    Interceptor, InterceptorError, MemoryConfig, ModelInfo, Provider, RunOutcome, RunProfile,
    RunSpec, Runtime, ToolResultPagingConfig, ToolRoster, Workspace, async_trait,
    runtime::{
        ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream, Request,
        Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::{
        ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
        declared::ToolsConfig,
    },
};
use serde_json::json;

const PROVIDER: &str = "roster-provider";
const MODEL: &str = "roster-model";
const ALLOWED: &str = "allowed_probe";
const OMITTED_HOST: &str = "omitted_host_probe";
const OMITTED_MCP_SHAPED: &str = "mcp__foreign__probe";
const DECLARED: &str = "declared_probe";
const SKILL: &str = "blocked_skill";
const PAGER: &str = "read_tool_result";

#[derive(Clone, Default)]
struct Shared {
    omitted: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<Request<'static>>>>,
    allowed_calls: Arc<AtomicUsize>,
    omitted_host_calls: Arc<AtomicUsize>,
    omitted_mcp_calls: Arc<AtomicUsize>,
    interceptions: Arc<AtomicUsize>,
}

impl Shared {
    fn set_omitted(&self, omitted: Vec<String>) {
        *self.omitted.lock().expect("omitted roster") = omitted;
    }

    fn omitted(&self) -> Vec<String> {
        self.omitted.lock().expect("omitted roster").clone()
    }

    fn requests(&self) -> Vec<Request<'static>> {
        self.requests.lock().expect("request recorder").clone()
    }
}

struct ScriptedProvider(Shared);

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
        Err(ProviderError::InvalidRequest(
            "the resolved-model roster test must not list models".to_string(),
        ))
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let model = request.model.to_string();
        let call = {
            let mut requests = self.0.requests.lock().expect("request recorder");
            requests.push(request.into_owned());
            requests.len()
        };

        let (content, stop_reason) = match call {
            1 => (
                vec![ContentBlock::ToolUse {
                    id: "allowed-call".to_string(),
                    name: ALLOWED.to_string(),
                    input: json!({}),
                }],
                Some("tool_use".to_string()),
            ),
            2 => (
                self.0
                    .omitted()
                    .into_iter()
                    .enumerate()
                    .map(|(index, name)| ContentBlock::ToolUse {
                        id: format!("denied-{index}"),
                        name,
                        input: json!({}),
                    })
                    .collect(),
                Some("tool_use".to_string()),
            ),
            _ => (vec![ContentBlock::text("roster complete")], None),
        };

        Ok(provider_event_stream_from_response(Response {
            id: format!("roster-response-{call}"),
            model,
            role: Role::Assistant,
            content,
            stop_reason,
            usage: None,
        }))
    }
}

#[derive(Clone, Default)]
struct Capture {
    requests: Arc<Mutex<Vec<Request<'static>>>>,
}

struct CaptureProvider(Capture);

#[async_trait]
impl Provider for CaptureProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(PROVIDER)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Err(ProviderError::InvalidRequest(
            "the pager probe must not list models".to_string(),
        ))
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let model = request.model.to_string();
        self.0
            .requests
            .lock()
            .expect("pager request recorder")
            .push(request.into_owned());
        Ok(provider_event_stream_from_response(Response {
            id: "pager-probe-response".to_string(),
            model,
            role: Role::Assistant,
            content: vec![ContentBlock::text("pager registered")],
            stop_reason: None,
            usage: None,
        }))
    }
}

struct CountingTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

impl ToolDefinition for CountingTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(self.name)
            .description("counts whether roster enforcement reached an executor")
            .input_schema(json!({"type": "object", "properties": {}}))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for CountingTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: serde_json::Value) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("{} executed", self.name))
    }
}

struct CountingInterceptor(Arc<AtomicUsize>);

#[async_trait]
impl Interceptor for CountingInterceptor {
    fn name(&self) -> &str {
        "roster-counting-interceptor"
    }

    async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        assert_eq!(call.tool_name, ALLOWED, "omitted calls must bypass hooks");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HookOutcome::Allow)
    }
}

fn write(path: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().expect("fixture parent"))
        .expect("create fixture directory");
    std::fs::write(path, body).expect("write fixture");
}

fn write_discovered_inputs(root: &Path) {
    write(
        &root.join(".basis/skills/blocked/SKILL.md"),
        format!(
            "---\nname: {SKILL}\ndescription: must remain unavailable\n---\nSKILL_BODY_MUST_NOT_REACH_PROVIDER"
        ),
    );
    write(
        &root.join(".basis/tools.json"),
        serde_json::to_vec(&json!({
            "schema": 1,
            "tools": {
                DECLARED: {
                    "description": "must be refused before subprocess execution",
                    "input_schema": {"type": "object", "properties": {}},
                    "command": ["roster-command-must-not-execute"]
                }
            }
        }))
        .expect("serialize declared manifest"),
    );
}

fn controlled_builder(root: &Path, shared: Shared) -> basis::WorkspaceBuilder {
    let builder = Workspace::builder(root)
        .with_runtime_builder(
            Runtime::builder()
                .with_provider_instance(ScriptedProvider(shared.clone()))
                .with_tool(CountingTool {
                    name: ALLOWED,
                    calls: Arc::clone(&shared.allowed_calls),
                })
                .with_tool(CountingTool {
                    name: OMITTED_HOST,
                    calls: Arc::clone(&shared.omitted_host_calls),
                })
                .with_tool(CountingTool {
                    name: OMITTED_MCP_SHAPED,
                    calls: Arc::clone(&shared.omitted_mcp_calls),
                })
                .with_interceptor(CountingInterceptor(Arc::clone(&shared.interceptions)))
                .with_ephemeral_history(),
        )
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER).with_context_window(64_000))
        .with_config(Config::default())
        .with_context(ContextConfig {
            file_name: "NO_CONTEXT.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: false,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_memory(MemoryConfig::disabled());

    #[cfg(feature = "mcp")]
    let builder = builder.with_mcp(McpConfig {
        workspace_file: PathBuf::from(".basis/mcp.json"),
        global_dir: None,
        supplied: Vec::new(),
    });

    builder
}

fn assert_registry_categories(registered: &BTreeSet<String>) {
    for name in [
        "spawn",
        "shell",
        "background_run",
        "check_background",
        "read",
        "ls",
        "grep",
        "glob",
        "write",
        "edit",
        "compact",
        "memory_search",
        "memory_pin",
        "memory_forget",
        "task",
        "task_create",
        "task_list",
        "team_spawn",
        "team_send",
        ALLOWED,
        OMITTED_HOST,
        OMITTED_MCP_SHAPED,
        DECLARED,
        "load_skill",
    ] {
        assert!(
            registered.contains(name),
            "registered roster omitted {name}"
        );
    }
    assert!(
        !registered.contains("files"),
        "Basis defaults to split file tools"
    );
}

#[tokio::test]
async fn paging_registers_its_scoped_reader_before_the_first_request() {
    let fixture = tempfile::tempdir().expect("pager workspace");
    let capture = Capture::default();
    let workspace = Workspace::builder(fixture.path())
        .without_discovery()
        .with_runtime_builder(
            Runtime::builder()
                .with_provider_instance(CaptureProvider(capture.clone()))
                .with_ephemeral_history(),
        )
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER).with_context_window(64_000))
        .open()
        .await
        .expect("pager workspace opens");

    workspace
        .prepare(
            RunSpec::new("prove pager registration").with_profile(
                RunProfile::new()
                    .with_tool_roster(ToolRoster::only([PAGER]))
                    .with_tool_result_paging(Some(ToolResultPagingConfig {
                        threshold_bytes: 64 * 1024,
                        page_bytes: 32 * 1024,
                    })),
            ),
        )
        .expect("pager run mints")
        .execute(CollectingSink::default())
        .await
        .expect("pager probe completes");

    let requests = capture.requests.lock().expect("pager request recorder");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        [PAGER]
    );
}

#[tokio::test]
async fn exact_profile_roster_refuses_every_hallucinated_registered_omission() {
    let fixture = tempfile::tempdir().expect("workspace fixture");
    write_discovered_inputs(fixture.path());
    let shared = Shared::default();
    let workspace = controlled_builder(fixture.path(), shared.clone())
        .open()
        .await
        .expect("controlled workspace opens");

    let descriptors = workspace.mentra_runtime().tools();
    let allowed_descriptor = descriptors
        .iter()
        .find(|descriptor| descriptor.provider.name == ALLOWED)
        .expect("allowed tool is registered")
        .provider
        .clone();
    let registered = descriptors
        .into_iter()
        .map(|descriptor| descriptor.provider.name)
        .collect::<BTreeSet<_>>();
    assert_registry_categories(&registered);

    let mut omitted = registered
        .into_iter()
        .filter(|name| name != ALLOWED)
        .collect::<BTreeSet<_>>();
    // The pager is session-scoped, so it is absent from the base runtime
    // registry above. `paging_registers_its_scoped_reader_before_the_first_request`
    // proves it is genuinely registered when enabled; this run deliberately
    // omits that registered name from its exact roster.
    omitted.insert(PAGER.to_string());
    let omitted = omitted.into_iter().collect::<Vec<_>>();
    assert!(!omitted.is_empty());
    shared.set_omitted(omitted.clone());

    let run = workspace.prepare(
        RunSpec::new("exercise the exact roster").with_profile(
            RunProfile::new()
                .with_tool_roster(ToolRoster::only([ALLOWED]))
                .with_tool_result_paging(Some(ToolResultPagingConfig {
                    threshold_bytes: 64 * 1024,
                    page_bytes: 32 * 1024,
                })),
        ),
    );
    let mut run = run.expect("profile mints without provider activity");
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        run.execute(CollectingSink::default()),
    )
    .await
    .expect("roster run must not hang")
    .expect("roster run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(report.final_message.as_deref(), Some("roster complete"));
    assert_eq!(shared.allowed_calls.load(Ordering::SeqCst), 1);
    assert_eq!(shared.omitted_host_calls.load(Ordering::SeqCst), 0);
    assert_eq!(shared.omitted_mcp_calls.load(Ordering::SeqCst), 0);
    assert_eq!(shared.interceptions.load(Ordering::SeqCst), 1);

    let requests = shared.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(
            request.tools.as_ref(),
            std::slice::from_ref(&allowed_descriptor)
        );
    }

    let expected = omitted
        .iter()
        .enumerate()
        .map(|(index, name)| (format!("denied-{index}"), name.clone()))
        .collect::<BTreeMap<_, _>>();
    let denied_results = requests[2]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id.starts_with("denied-") => {
                Some((tool_use_id.clone(), content.to_display_string(), *is_error))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(denied_results.len(), expected.len());
    let denied_result_map = denied_results
        .iter()
        .map(|(call_id, content, is_error)| (call_id.clone(), (content.clone(), *is_error)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        denied_result_map.len(),
        denied_results.len(),
        "denied result call ids must be unique"
    );
    assert_eq!(
        denied_result_map.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    for (call_id, name) in &expected {
        let (content, is_error) = denied_result_map
            .get(call_id)
            .expect("every denied call has one result");
        assert!(is_error, "{name} must be returned as an error result");
        assert_eq!(
            content,
            &format!("Tool '{name}' is not available for this agent")
        );
    }

    let completed = report
        .sink
        .events()
        .iter()
        .filter_map(|event| match event {
            basis::Event::ToolCompleted {
                tool_call_id,
                tool_name,
                is_error,
                ..
            } if tool_call_id.starts_with("denied-") => {
                Some((tool_call_id.clone(), tool_name.clone(), *is_error))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), expected.len());
    let completed_map = completed
        .iter()
        .map(|(call_id, name, is_error)| (call_id.clone(), (name.clone(), *is_error)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        completed_map.len(),
        completed.len(),
        "denied completion call ids must be unique"
    );
    assert_eq!(
        completed_map.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    for (call_id, expected_name) in &expected {
        let (name, is_error) = completed_map
            .get(call_id)
            .expect("every denied call has one completion");
        assert_eq!(name, expected_name);
        assert!(is_error, "denied {name} completion must be an error");
    }
}
