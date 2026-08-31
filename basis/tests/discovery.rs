//! The coherent discovery-off posture, through Basis's public embedding API.
//!
//! Every supported workspace/global input is present and hostile. The run must
//! still be made only from the host's provider, resolved model, prompt, native
//! tool, roster, and interceptor. A hook and an MCP server both carry a process
//! marker, so an empty report alone cannot disguise attempted execution.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use basis::{
    CollectingSink, Config, ContextConfig, ContextScope, HookOutcome, HookRequest, HooksConfig,
    Interceptor, InterceptorError, MemoryConfig, ModelInfo, Provider, RunError, RunOutcome,
    Runtime, RuntimeBuilder, Setting, SystemPrompt, ToolRoster, Workspace, WorkspaceBuilder,
    WorkspaceMemoryRoot, async_trait,
    event::ContextFile,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::{
        ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
        declared::ToolsConfig,
    },
};
#[cfg(feature = "mcp")]
use basis::{McpConfig, McpServer};
use serde_json::{Value, json};

const PROVIDER: &str = "host-provider";
const MODEL: &str = "host-model";
const HOST_PROMPT: &str = "HOST_PROMPT_ONLY";
const HOST_TOOL: &str = "host_fact";
const HOST_TOOL_RESULT: &str = "HOST_TOOL_RESULT_ONLY";
const FINAL_MESSAGE: &str = "HOST_PROVIDER_COMPLETE";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    model: String,
    system: Option<String>,
    tools: Vec<String>,
    tool_results: Vec<String>,
}

#[derive(Clone, Default)]
struct HostProvider {
    listings: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl HostProvider {
    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().expect("provider requests").clone()
    }
}

#[async_trait]
impl Provider for HostProvider {
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
        self.listings.fetch_add(1, Ordering::SeqCst);
        Ok(vec![resolved_model()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let turn = self.streams.fetch_add(1, Ordering::SeqCst);
        self.asked.lock().expect("provider requests").push(Asked {
            model: request.model.to_string(),
            system: request.system.as_deref().map(str::to_string),
            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
            tool_results: request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { content, .. } => Some(content.to_display_string()),
                    _ => None,
                })
                .collect(),
        });

        let (content, stop_reason) = if turn == 0 {
            (
                vec![ContentBlock::ToolUse {
                    id: "host-call-1".to_string(),
                    name: HOST_TOOL.to_string(),
                    input: json!({}),
                }],
                Some("tool_use".to_string()),
            )
        } else {
            (vec![ContentBlock::text(FINAL_MESSAGE)], None)
        };

        Ok(provider_event_stream_from_response(Response {
            id: format!("host-response-{turn}"),
            model: request.model.to_string(),
            role: Role::Assistant,
            content,
            stop_reason,
            usage: None,
        }))
    }
}

struct HostTool(Arc<AtomicUsize>);

impl ToolDefinition for HostTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(HOST_TOOL)
            .description("returns one host-owned fact")
            .input_schema(json!({"type": "object", "properties": {}}))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for HostTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HOST_TOOL_RESULT.to_string())
    }
}

struct HostInterceptor(Arc<AtomicUsize>);

#[async_trait]
impl Interceptor for HostInterceptor {
    fn name(&self) -> &str {
        "host-interceptor"
    }

    async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        assert_eq!(call.tool_name, HOST_TOOL);
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HookOutcome::Allow)
    }
}

fn resolved_model() -> ModelInfo {
    ModelInfo::new(MODEL, PROVIDER).with_context_window(262_144)
}

fn runtime_builder(
    provider: HostProvider,
    tool_calls: Arc<AtomicUsize>,
    interceptions: Arc<AtomicUsize>,
) -> RuntimeBuilder {
    Runtime::builder()
        .with_provider_instance(provider)
        .with_tool(HostTool(tool_calls))
        .with_interceptor(HostInterceptor(interceptions))
        .with_ephemeral_history()
}

fn write(path: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create fixture directory");
    std::fs::write(path, body).expect("write hostile fixture");
}

fn process_command(marker: &Path, source: &str) -> Value {
    json!([
        "/bin/sh",
        "-c",
        format!("printf {source} > '{}'", marker.display())
    ])
}

fn hostile_workspace(root: &Path, process_marker: &Path) {
    write(root.join("AGENTS.md").as_path(), "WORKSPACE_CONTEXT_MARKER");
    write(
        root.join(".basis/config.json").as_path(),
        "{WORKSPACE_CONFIG_MARKER",
    );
    write(
        root.join(".basis/hooks.json").as_path(),
        serde_json::to_vec(&json!({
            "schema": 1,
            "hooks": [{
                "name": "WORKSPACE_HOOK_MARKER",
                "command": process_command(process_marker, "workspace-hook")
            }]
        }))
        .expect("serialize hook"),
    );
    write(
        root.join(".basis/tools.json").as_path(),
        serde_json::to_vec(&json!({
            "schema": 1,
            "tools": {
                "WORKSPACE_DECLARED_TOOL_MARKER": {
                    "description": "hostile declared tool",
                    "input_schema": {"type": "object"},
                    "command": process_command(process_marker, "workspace-tool")
                }
            }
        }))
        .expect("serialize declared tool"),
    );
    write(
        root.join(".basis/memory/hostile.md").as_path(),
        "---\nname: WORKSPACE_MEMORY_MARKER\ninvalid-frontmatter\n---\nbody",
    );
    write(
        root.join(".basis/skills/hostile/SKILL.md").as_path(),
        "---\nname: WORKSPACE_SKILL_MARKER\ninvalid-frontmatter\n---\nbody",
    );
    write(
        root.join(".agents/skills/shared/SKILL.md").as_path(),
        "WORKSPACE_SHARED_SKILL_MARKER",
    );
    write(
        root.join(".basis/templates/hostile.md").as_path(),
        "---\nWORKSPACE_TEMPLATE_MARKER: [\n---\nbody",
    );
    write(
        root.join(".mcp.json").as_path(),
        serde_json::to_vec(&json!({
            "mcpServers": {
                "WORKSPACE_MCP_MARKER": {
                    "command": "/bin/sh",
                    "args": ["-c", format!("printf workspace-mcp > '{}'", process_marker.display())]
                }
            }
        }))
        .expect("serialize MCP config"),
    );
}

fn hostile_home(root: &Path) {
    write(root.join("AGENTS.md").as_path(), "HOME_CONTEXT_MARKER");
    write(root.join("config.json").as_path(), "{HOME_CONFIG_MARKER");
    write(root.join("hooks.json").as_path(), "{HOME_HOOK_MARKER");
    write(root.join("tools.json").as_path(), "{HOME_TOOL_MARKER");
    write(root.join("mcp.json").as_path(), "{HOME_MCP_MARKER");
    write(
        root.join("memory/hostile.md").as_path(),
        "---\nHOME_MEMORY_MARKER: [\n---\nbody",
    );
    write(
        root.join("skills/hostile/SKILL.md").as_path(),
        "---\nHOME_SKILL_MARKER: [\n---\nbody",
    );
    write(
        root.join("templates/hostile.md").as_path(),
        "---\nHOME_TEMPLATE_MARKER: [\n---\nbody",
    );
    write(
        root.join(".agents/skills/shared/SKILL.md").as_path(),
        "HOME_SHARED_SKILL_MARKER",
    );
}

fn configured_builder(
    workspace: &Path,
    home: &Path,
    runtime: RuntimeBuilder,
    supplied_config: Config,
) -> WorkspaceBuilder {
    let builder = Workspace::builder(workspace)
        .with_runtime_builder(runtime)
        .with_resolved_model(resolved_model())
        .with_system_prompt(SystemPrompt::Replace(HOST_PROMPT.to_string()))
        .with_tool_roster(ToolRoster::only([HOST_TOOL]))
        // Every discovery-specific setter deliberately comes later. The off
        // switch must be sticky rather than a bundle of defaults a setter can
        // accidentally undo.
        .without_discovery()
        .with_config(supplied_config)
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: Some(home.to_path_buf()),
            walk_parents: true,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: Some(home.to_path_buf()),
            shared_home_dir: true,
        })
        .with_memory(MemoryConfig {
            global_root: Some(home.join("memory")),
            workspace_root: WorkspaceMemoryRoot::Dir(workspace.join(".basis/memory")),
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: Some(home.to_path_buf()),
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: Some(home.to_path_buf()),
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: Some(home.to_path_buf()),
        });

    #[cfg(feature = "mcp")]
    let builder = builder.with_mcp(McpConfig {
        workspace_file: PathBuf::from(".mcp.json"),
        global_dir: Some(home.to_path_buf()),
        supplied: Vec::new(),
    });

    builder
}

#[tokio::test]
async fn hostile_workspace_and_home_are_inert_but_explicit_host_inputs_run() {
    let fixture = tempfile::tempdir().expect("fixture root");
    let workspace_dir = fixture.path().join("workspace");
    let home_dir = fixture.path().join("home");
    std::fs::create_dir_all(&workspace_dir).expect("workspace");
    std::fs::create_dir_all(&home_dir).expect("home");
    let process_marker = fixture.path().join("DISCOVERY_PROCESS_MARKER");
    hostile_workspace(&workspace_dir, &process_marker);
    hostile_home(&home_dir);

    let provider = HostProvider::default();
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let interceptions = Arc::new(AtomicUsize::new(0));
    let supplied_config = Config {
        model: Some(Setting {
            value: "TRUSTED_CONFIG_MODEL_MARKER".to_string(),
            path: PathBuf::from("/host/trusted-config"),
            scope: ContextScope::Global,
        }),
        files: vec![ContextFile {
            path: PathBuf::from("/host/trusted-config"),
            scope: "global".to_string(),
        }],
        ..Config::default()
    };

    let workspace = configured_builder(
        &workspace_dir,
        &home_dir,
        runtime_builder(
            provider.clone(),
            Arc::clone(&tool_calls),
            Arc::clone(&interceptions),
        ),
        supplied_config.clone(),
    )
    .open()
    .await
    .expect("hostile discovery files are never opened");

    assert_eq!(workspace.config(), &supplied_config);
    assert_eq!(workspace.provider(), PROVIDER);
    assert_eq!(
        workspace.model(),
        MODEL,
        "the resolved model outranks config"
    );
    assert!(workspace.context().documents().is_empty());
    assert!(workspace.memories().is_empty());
    assert!(workspace.skills().is_empty());
    assert!(workspace.templates().is_empty());
    assert!(workspace.declared_tools().is_empty());
    assert!(workspace.declared_tool_files().is_empty());
    assert!(workspace.mcp_servers().is_empty());

    let report = workspace
        .prepare("run the explicit host tool")
        .expect("the workspace guard supports minting")
        .execute(CollectingSink::default())
        .await
        .expect("the explicit host run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(report.provider, PROVIDER);
    assert_eq!(report.model, MODEL);
    assert_eq!(report.final_message.as_deref(), Some(FINAL_MESSAGE));
    assert_eq!(provider.listings.load(Ordering::SeqCst), 0);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 2);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        interceptions.load(Ordering::SeqCst),
        1,
        "host interceptors still run through the registered workspace guard"
    );

    let asked = provider.asked();
    assert_eq!(asked.len(), 2);
    for request in &asked {
        assert_eq!(request.model, MODEL);
        assert_eq!(request.system.as_deref(), Some(HOST_PROMPT));
        assert_eq!(request.tools, [HOST_TOOL]);
    }
    assert!(asked[0].tool_results.is_empty());
    assert_eq!(asked[1].tool_results, [HOST_TOOL_RESULT]);
    assert!(
        !process_marker.exists(),
        "neither a discovered hook nor MCP server may start a process"
    );
}

#[tokio::test]
async fn absent_explicit_config_means_no_config_discovery() {
    let workspace_dir = tempfile::tempdir().expect("workspace");
    let home_dir = tempfile::tempdir().expect("global home");
    write(
        workspace_dir.path().join(".basis/config.json").as_path(),
        "{MALFORMED_CONFIG_MUST_NOT_BE_READ",
    );
    write(
        home_dir.path().join("config.json").as_path(),
        "{MALFORMED_GLOBAL_CONFIG_MUST_NOT_BE_READ",
    );
    let provider = HostProvider::default();
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let interceptions = Arc::new(AtomicUsize::new(0));

    let workspace = Workspace::builder(workspace_dir.path())
        .with_runtime_builder(runtime_builder(
            provider.clone(),
            Arc::clone(&tool_calls),
            Arc::clone(&interceptions),
        ))
        .with_resolved_model(resolved_model())
        .without_discovery()
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: Some(home_dir.path().to_path_buf()),
            walk_parents: true,
        })
        .open()
        .await
        .expect("an absent explicit config resolves to Config::default without a file probe");

    assert_eq!(workspace.config(), &Config::default());
    assert_eq!(provider.listings.load(Ordering::SeqCst), 0);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(interceptions.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn discovery_off_still_applies_supplied_mcp_servers() {
    let workspace = tempfile::tempdir().expect("workspace");
    let marker = workspace.path().join("supplied-mcp-started");
    write(
        &workspace.path().join(".mcp.json"),
        "{malformed discovery must stay unread",
    );
    let process = process_command(&marker, "supplied-mcp");
    let process = process
        .as_array()
        .expect("the existing process fixture is argv");
    let provider = HostProvider::default();
    let _workspace = Workspace::builder(workspace.path())
        .with_runtime_builder(runtime_builder(
            provider,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ))
        .with_resolved_model(resolved_model())
        .without_discovery()
        .with_mcp(McpConfig {
            workspace_file: PathBuf::from(".mcp.json"),
            global_dir: None,
            supplied: vec![McpServer::Stdio(mentra::mcp::McpServerConfig {
                name: "supplied-marker".to_string(),
                command: process[0].as_str().expect("program").to_string(),
                args: process[1..]
                    .iter()
                    .map(|argument| argument.as_str().expect("argument").to_string())
                    .collect(),
                env: Default::default(),
                cwd: None,
            })],
        })
        .open()
        .await
        .expect("the malformed discovered file stays inert");

    assert!(
        marker.is_file(),
        "the valid supplied config must be applied even though file discovery is off"
    );
}

#[tokio::test]
async fn discovery_off_refuses_every_shared_runtime_before_activity() {
    for contaminated in [false, true] {
        let workspace_dir = tempfile::tempdir().expect("workspace");
        let skill_root = tempfile::tempdir().expect("skill root");
        write(
            skill_root.path().join("hostile/SKILL.md").as_path(),
            "---\nname: hostile\ndescription: RUNTIME_SKILL_DESCRIPTION_MARKER\n---\nRUNTIME_SKILL_BODY_MARKER",
        );
        let provider = HostProvider::default();
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let interceptions = Arc::new(AtomicUsize::new(0));
        let runtime = Arc::new(
            runtime_builder(
                provider.clone(),
                Arc::clone(&tool_calls),
                Arc::clone(&interceptions),
            )
            .build()
            .expect("build the shared runtime before the workspace exists"),
        );
        if contaminated {
            runtime
                .mentra_runtime()
                .register_skills_dir(skill_root.path())
                .expect("pre-register the hostile shared-runtime skill");
        }

        let error = Workspace::builder(workspace_dir.path())
            .with_runtime(runtime)
            .with_resolved_model(resolved_model())
            .without_discovery()
            .open()
            .await
            .expect_err("an Arc runtime cannot provide a race-free discovery-off posture");

        assert!(
            matches!(error, RunError::DiscoveryDisabledSharedRuntime),
            "contaminated={contaminated}: {error}"
        );
        assert_eq!(provider.listings.load(Ordering::SeqCst), 0);
        assert_eq!(provider.streams.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        assert_eq!(interceptions.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn discovery_off_still_validates_the_workspace_path_before_activity() {
    let fixture = tempfile::tempdir().expect("fixture root");
    let missing = fixture.path().join("missing-workspace");
    let provider = HostProvider::default();
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let interceptions = Arc::new(AtomicUsize::new(0));

    let error = Workspace::builder(&missing)
        .with_runtime_builder(runtime_builder(
            provider.clone(),
            Arc::clone(&tool_calls),
            Arc::clone(&interceptions),
        ))
        .with_resolved_model(resolved_model())
        .without_discovery()
        .open()
        .await
        .expect_err("a missing workspace remains invalid");

    assert!(error.to_string().contains("workspace"), "{error}");
    assert_eq!(provider.listings.load(Ordering::SeqCst), 0);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(interceptions.load(Ordering::SeqCst), 0);
}
