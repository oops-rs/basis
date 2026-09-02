//! Per-run host contracts at the provider boundary.
//!
//! These tests use only `basis` exports. Besides checking the values Mentra's
//! provider actually receives, that makes this a public-API probe for every
//! upstream type a [`RunProfile`] makes an embedding host name.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[cfg(feature = "mcp")]
use basis::McpConfig;
use basis::{
    AllowAll, CollectingSink, Compaction, ContextConfig, Effort, Event, MemoryConfig, ModelInfo,
    Provider, ProviderRequestOptions, ReasoningOptions, RunError, RunProfile, RunSpec, Runtime,
    RuntimeBuilder, SystemPrompt, ToolResultPagingConfig, ToolRoster, Workspace, WorkspaceBuilder,
    async_trait,
    hooks::HooksConfig,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::declared::ToolsConfig,
    tools::{ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult},
};
use serde_json::json;

const PROVIDER: &str = "profile-provider";
const WORKSPACE_MODEL: &str = "workspace-model";
const PROFILE_MODEL: &str = "profile-model";
const WORKSPACE_SYSTEM: &str = "workspace system";
const PROFILE_SYSTEM: &str = "profile system";
const VISIBLE_TOOL: &str = "profile_visible";
const FOREIGN_TOOL: &str = "mcp__foreign__secret";
const SECRET_HEADER: &str = "Bearer must-not-reach-debug";
const COUNTING_TOOL: &str = "counting_tool";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    model: String,
    system: Option<String>,
    tools: Vec<String>,
    max_output_tokens: Option<u32>,
    options: ProviderRequestOptions,
}

#[derive(Clone, Default)]
struct CapturingProvider {
    listings: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl CapturingProvider {
    fn requests(&self) -> Vec<Asked> {
        self.asked.lock().expect("provider recorder").clone()
    }

    fn listings(&self) -> usize {
        self.listings.load(Ordering::SeqCst)
    }

    fn streams(&self) -> usize {
        self.streams.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for CapturingProvider {
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
        Ok(vec![workspace_model()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let call = self.streams.fetch_add(1, Ordering::SeqCst) + 1;
        self.asked.lock().expect("provider recorder").push(Asked {
            model: request.model.to_string(),
            system: request.system.map(|system| system.into_owned()),
            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
            max_output_tokens: request.max_output_tokens,
            options: request.provider_request_options,
        });

        Ok(provider_event_stream_from_response(Response {
            id: format!("profile-response-{call}"),
            model: PROFILE_MODEL.to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text("done")],
            stop_reason: None,
            usage: None,
        }))
    }
}

struct NamedTool(&'static str);

impl ToolDefinition for NamedTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(self.0)
            .description("a test-only host tool")
            .input_schema(json!({"type": "object", "properties": {}}))
            .build()
    }
}

struct CountingTool {
    calls: Arc<AtomicUsize>,
}

impl ToolDefinition for CountingTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(COUNTING_TOOL)
            .description("records accidental execution")
            .input_schema(json!({"type": "object", "properties": {}}))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for CountingTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: serde_json::Value) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("unused".to_string())
    }
}

#[async_trait]
impl ToolExecutor for NamedTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: serde_json::Value) -> ToolResult {
        Ok("unused".to_string())
    }
}

fn workspace_model() -> ModelInfo {
    ModelInfo::new(WORKSPACE_MODEL, PROVIDER).with_context_window(64_000)
}

fn profile_model() -> ModelInfo {
    let mut model = ModelInfo::new(PROFILE_MODEL, PROVIDER).with_context_window(262_144);
    model.display_name = Some("Host-resolved profile model".to_string());
    model.description = Some("metadata Basis must preserve".to_string());
    model
}

fn runtime(provider: CapturingProvider) -> RuntimeBuilder {
    Runtime::builder()
        .with_provider_instance(provider)
        .with_tool(NamedTool(VISIBLE_TOOL))
        .with_tool(NamedTool(FOREIGN_TOOL))
        .with_ephemeral_history()
}

fn builder(path: &std::path::Path, provider: CapturingProvider) -> WorkspaceBuilder {
    Workspace::builder(path)
        .without_discovery()
        .with_runtime_builder(runtime(provider))
        .with_resolved_model(workspace_model())
        .with_system_prompt(SystemPrompt::Replace(WORKSPACE_SYSTEM.to_string()))
}

fn discovery_builder(path: &Path, provider: CapturingProvider) -> WorkspaceBuilder {
    let builder = Workspace::builder(path)
        .with_runtime_builder(runtime(provider))
        .with_resolved_model(workspace_model())
        .with_context(ContextConfig {
            file_name: "NO_CONTEXT.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
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

fn persisted_text(path: &Path) -> String {
    if path.is_file() {
        return std::fs::read_to_string(path).expect("read persisted store file as UTF-8");
    }

    std::fs::read_dir(path)
        .expect("read persisted store directory")
        .map(|entry| {
            let entry = entry.expect("read persisted store directory entry");
            persisted_text(&entry.path())
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        summary: None,
    }
}

fn complete_request_options() -> ProviderRequestOptions {
    let mut options = ProviderRequestOptions {
        reasoning: Some(low_reasoning()),
        ..Default::default()
    };
    options.responses.parallel_tool_calls = Some(false);
    options.responses.previous_response_id = Some("profile-previous-response".to_string());
    options.responses.store = Some(false);
    options.responses.stream = Some(true);
    options.responses.include = vec!["reasoning.encrypted_content".to_string()];
    options.responses.service_tier = Some("priority".to_string());
    options.responses.prompt_cache_key = Some("profile-cache".to_string());
    options.anthropic.disable_parallel_tool_use = Some(true);
    options.gemini.thoughts = Some(true);
    options.session.sticky_turn_state = Some("profile-turn".to_string());
    options.session.turn_metadata = Some("profile-metadata".to_string());
    options.session.subagent = Some("profile-subagent".to_string());
    options.session.prefer_connection_reuse = Some(true);
    options.session.session_affinity = Some("profile-affinity".to_string());
    options
        .session
        .extra_headers
        .insert("authorization".to_string(), SECRET_HEADER.to_string());
    options
}

#[tokio::test]
async fn a_profile_reaches_the_provider_field_for_field() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("the discovery-free workspace opens");
    let expected_options = ProviderRequestOptions {
        reasoning: Some(high_reasoning()),
        ..complete_request_options()
    };
    let profile = RunProfile::new()
        .with_resolved_model(profile_model())
        .with_tool_roster(ToolRoster::only([VISIBLE_TOOL]))
        .with_provider_request_options(expected_options.clone())
        .with_max_output_tokens(Some(1_234))
        .with_compaction(
            Compaction::default()
                .with_keep_recent_tool_results(Some(2))
                .with_auto_threshold_tokens(Some(12_345)),
        )
        .with_tool_result_paging(Some(ToolResultPagingConfig {
            threshold_bytes: 128 * 1024,
            page_bytes: 16 * 1024,
        }))
        .with_system_prompt(SystemPrompt::Replace(PROFILE_SYSTEM.to_string()));

    let debug = format!("{profile:?}");
    assert!(
        !debug.contains(SECRET_HEADER),
        "headers must be redacted: {debug}"
    );
    assert!(
        !debug.contains(PROFILE_SYSTEM),
        "prompt text must be redacted: {debug}"
    );

    let mut run = workspace
        .prepare(RunSpec::new("go").with_profile(profile))
        .expect("the run mints without provider activity");
    assert_eq!(run.context_window(), Some(262_144));
    assert!(matches!(
        run.header(),
        Event::RunStarted { ref model, .. } if model == PROFILE_MODEL
    ));
    let report = run
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the scripted provider completes");

    assert_eq!(report.model, PROFILE_MODEL);
    assert_eq!(provider.listings(), 0, "resolved metadata bypasses listing");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.model, PROFILE_MODEL);
    assert_eq!(request.system.as_deref(), Some(PROFILE_SYSTEM));
    assert_eq!(request.tools, [VISIBLE_TOOL]);
    assert_eq!(request.max_output_tokens, Some(1_234));
    assert_eq!(request.options.reasoning, Some(high_reasoning()));
    assert_eq!(request.options.responses, expected_options.responses);
    assert_eq!(request.options.anthropic, expected_options.anthropic);
    assert_eq!(request.options.gemini, expected_options.gemini);
    assert_eq!(request.options.session, expected_options.session);
    assert_eq!(request.options, expected_options);
}

#[tokio::test]
async fn durable_history_refuses_extra_headers_before_persisting_or_running() {
    let provider = CapturingProvider::default();
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let workspace_dir = tempfile::tempdir().expect("workspace");
    let store = tempfile::tempdir().expect("durable store");
    let workspace = Workspace::builder(workspace_dir.path())
        .without_discovery()
        .with_runtime_builder(
            Runtime::builder()
                .with_provider_instance(provider.clone())
                .with_tool(CountingTool {
                    calls: Arc::clone(&tool_calls),
                })
                .with_store_dir(store.path()),
        )
        .with_resolved_model(workspace_model())
        .open()
        .await
        .expect("durable workspace opens");

    let error = workspace
        .prepare(RunSpec::new("must not mint").with_profile(
            RunProfile::new().with_provider_request_options(complete_request_options()),
        ))
        .expect_err("a persisted AgentConfig must not receive request credentials");

    assert!(matches!(
        error,
        RunError::RunProfileHeadersRequireEphemeralHistory
    ));
    assert_eq!(provider.listings(), 0);
    assert_eq!(provider.streams(), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    assert!(!persisted_text(store.path()).contains(SECRET_HEADER));
}

#[tokio::test]
async fn durable_history_accepts_complete_options_when_extra_headers_are_empty() {
    let provider = CapturingProvider::default();
    let workspace_dir = tempfile::tempdir().expect("workspace");
    let store = tempfile::tempdir().expect("durable store");
    let workspace = Workspace::builder(workspace_dir.path())
        .without_discovery()
        .with_runtime_builder(
            Runtime::builder()
                .with_provider_instance(provider.clone())
                .with_store_dir(store.path()),
        )
        .with_resolved_model(workspace_model())
        .open()
        .await
        .expect("durable workspace opens");
    let mut safe_options = complete_request_options();
    safe_options.session.extra_headers.clear();
    let safe = workspace.prepare(
        RunSpec::new("safe to persist")
            .with_profile(RunProfile::new().with_provider_request_options(safe_options)),
    );
    assert!(safe.is_ok(), "empty extra headers remain durable-safe");
    drop(safe);
    assert!(!persisted_text(store.path()).contains(SECRET_HEADER));
    assert_eq!(provider.streams(), 0);
}

#[tokio::test]
async fn explicit_clears_win_and_omitted_fields_inherit() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens");

    workspace
        .prepare(
            RunSpec::new("clear")
                .with_effort(Effort::High)
                .with_profile(
                    RunProfile::new()
                        .with_max_output_tokens(None)
                        .with_provider_request_options(ProviderRequestOptions::default()),
                ),
        )
        .expect("clear profile mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("clear profile runs");

    workspace
        .prepare(
            RunSpec::new("clear even when effort is called later")
                .with_profile(
                    RunProfile::new()
                        .with_provider_request_options(ProviderRequestOptions::default()),
                )
                .with_effort(Effort::High),
        )
        .expect("late legacy effort cannot override the profile")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("reverse builder order runs");

    workspace
        .prepare(
            RunSpec::new("inherit")
                .with_effort(Effort::High)
                .with_profile(RunProfile::new()),
        )
        .expect("empty profile mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("empty profile runs");

    let requests = provider.requests();
    assert_eq!(requests[0].system.as_deref(), Some(WORKSPACE_SYSTEM));
    assert_eq!(requests[0].max_output_tokens, None);
    assert_eq!(requests[0].options.reasoning, None);
    assert_eq!(requests[1].options.reasoning, None);
    assert_eq!(requests[2].system.as_deref(), Some(WORKSPACE_SYSTEM));
    assert_eq!(requests[2].max_output_tokens, Some(8_192));
    assert_eq!(requests[2].options.reasoning, Some(high_reasoning()));
}

#[tokio::test]
async fn stated_request_options_outrank_a_later_effort() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens");

    workspace
        .prepare(
            RunSpec::new("complete options, then effort")
                .with_profile(
                    RunProfile::new().with_provider_request_options(complete_request_options()),
                )
                .with_effort(Effort::High),
        )
        .expect("the run mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");

    assert_eq!(
        provider.requests()[0].options.reasoning,
        Some(low_reasoning()),
        "the profile is the complete contract; effort is only its fallback"
    );
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn an_exact_profile_roster_is_still_narrowed_by_foreign_tools() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens");

    workspace
        .prepare(RunSpec::new("go").with_profile(
            RunProfile::new().with_tool_roster(ToolRoster::only([VISIBLE_TOOL, FOREIGN_TOOL])),
        ))
        .expect("run mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("run completes");

    assert_eq!(provider.requests()[0].tools, [VISIBLE_TOOL]);
}

#[tokio::test]
async fn a_profile_model_for_another_provider_is_typed_before_activity() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens");
    let error = workspace
        .prepare(
            RunSpec::new("go").with_resolved_model(ModelInfo::new("foreign", "other-provider")),
        )
        .expect_err("provider identity cannot be inferred or repaired");

    assert!(matches!(
        error,
        RunError::ResolvedModelProviderMismatch {
            ref model,
            ref model_provider,
            ref runtime_provider,
        } if model == "foreign"
            && model_provider == "other-provider"
            && runtime_provider == PROVIDER
    ));
    assert_eq!(provider.listings(), 0);
    assert_eq!(provider.streams(), 0);
}

#[tokio::test]
async fn resume_applies_exact_model_metadata_without_guessing_the_system_snapshot() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace_default = "workspace-only-system".repeat(40);
    let seed_system = "profile-seed-system".repeat(50);
    let workspace = builder(dir.path(), provider.clone())
        .with_system_prompt(SystemPrompt::Replace(workspace_default))
        .open()
        .await
        .expect("workspace opens");
    let agent_id = {
        let run = workspace
            .prepare(RunSpec::new("seed").with_profile(
                RunProfile::new().with_system_prompt(SystemPrompt::Replace(seed_system.clone())),
            ))
            .expect("seed mints with a per-run prompt");
        assert!(run.estimated_context_tokens() > 100);
        run.agent_id().to_string()
    };

    let mut resumed = workspace
        .resume(
            &agent_id,
            RunSpec::new("again").with_resolved_model(profile_model()),
        )
        .expect("model-only is one supported persisted mutation");
    assert_eq!(resumed.context_window(), Some(262_144));
    assert!(
        resumed.estimated_context_tokens() < 10,
        "resume must report an unknown prompt floor, not either known prompt"
    );
    assert!(matches!(
        resumed.header(),
        Event::RunStarted { ref model, .. } if model == PROFILE_MODEL
    ));
    let report = resumed
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("resumed run completes");

    assert_eq!(report.model, PROFILE_MODEL);
    let request = provider.requests().pop().expect("one resumed request");
    assert_eq!(request.model, PROFILE_MODEL);
    assert_eq!(request.system.as_deref(), Some(seed_system.as_str()));
}

#[tokio::test]
async fn reasoning_only_resume_performs_one_mutation_and_leaves_the_window_unknown() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens");
    let agent_id = {
        let run = workspace.prepare("seed").expect("seed mints");
        run.agent_id().to_string()
    };

    let mut resumed = workspace
        .resume(&agent_id, RunSpec::new("again").with_effort(Effort::High))
        .expect("reasoning-only is one supported persisted mutation");
    assert_eq!(
        resumed.context_window(),
        None,
        "restoring the workspace model too would be a second persisted write"
    );
    resumed
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("resumed turn completes");

    let request = provider.requests().pop().expect("one resumed request");
    assert_eq!(request.model, WORKSPACE_MODEL);
    assert_eq!(request.options.reasoning, Some(high_reasoning()));
}

#[tokio::test]
async fn resume_refuses_model_plus_any_reasoning_mutation_before_lookup() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens");

    let legacy_effort = workspace
        .resume(
            "missing-agent",
            RunSpec::new("again")
                .with_resolved_model(profile_model())
                .with_effort(Effort::High),
        )
        .expect_err("legacy effort is still a second persisted mutation");
    assert!(matches!(legacy_effort, RunError::NonAtomicResumeProfile));
    assert_eq!(provider.listings(), 0);
    assert_eq!(provider.streams(), 0);
}

#[tokio::test]
async fn resume_model_also_refuses_an_effective_workspace_effort_before_lookup() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let config_dir = dir.path().join(".basis");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    std::fs::write(
        config_dir.join("config.json"),
        r#"{"schema": 1, "effort": "high"}"#,
    )
    .expect("write workspace effort");
    let workspace = discovery_builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens with discovered effort");

    let error = workspace
        .resume(
            "missing-agent",
            RunSpec::new("again").with_resolved_model(profile_model()),
        )
        .expect_err("workspace effort is also a reasoning mutation");

    assert!(matches!(error, RunError::NonAtomicResumeProfile));
    assert_eq!(provider.listings(), 0);
    assert_eq!(provider.streams(), 0);
}

#[tokio::test]
async fn unsupported_resume_fields_fail_before_session_or_provider_activity() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens");
    let error = workspace
        .resume(
            "missing-agent",
            RunSpec::new("again").with_profile(
                RunProfile::new()
                    .with_system_prompt(SystemPrompt::Replace("new system".to_string())),
            ),
        )
        .expect_err("unsupported fields are rejected before even looking up the session");

    assert!(matches!(
        error,
        RunError::UnsupportedResumeProfile {
            field: "system_prompt"
        }
    ));
    assert_eq!(provider.listings(), 0);
    assert_eq!(provider.streams(), 0);
}

#[tokio::test]
async fn resume_validates_profile_provider_before_session_lookup() {
    let provider = CapturingProvider::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = builder(dir.path(), provider.clone())
        .open()
        .await
        .expect("workspace opens");
    let error = workspace
        .resume(
            "missing-agent",
            RunSpec::new("again").with_resolved_model(ModelInfo::new("foreign", "other-provider")),
        )
        .expect_err("provider mismatch wins before session lookup");

    assert!(matches!(
        error,
        RunError::ResolvedModelProviderMismatch { .. }
    ));
    assert_eq!(provider.listings(), 0);
    assert_eq!(provider.streams(), 0);
}
