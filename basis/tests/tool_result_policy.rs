//! Host-defined tool-result limits through Basis's runtime boundary.
//!
//! These tests inspect the second provider request, after a host tool ran. The
//! request is the load-bearing boundary: an untruncated event or transcript is
//! insufficient if the model itself received a shortened result.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use basis::{
    AllowAll, CollectingSink, ContextConfig, MemoryConfig, ModelSelector, Provider, RunOutcome,
    Runtime, ToolResultPolicy, Workspace, WorkspaceBuilder, async_trait,
    hooks::HooksConfig,
    runtime::{
        ContentBlock, ModelInfo, ProviderCapabilities, ProviderDescriptor, ProviderError,
        ProviderEventStream, Request, Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::{
        ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
        declared::ToolsConfig,
    },
};
use serde_json::json;

const TOOL_NAME: &str = "large_output";

#[derive(Clone)]
struct CapturingProvider {
    calls: Arc<AtomicUsize>,
    tool_results: Arc<Mutex<Vec<String>>>,
}

impl CapturingProvider {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            tool_results: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn last_tool_result(&self) -> String {
        self.tool_results
            .lock()
            .expect("tool-result recorder")
            .last()
            .expect("the provider saw a tool result")
            .clone()
    }
}

#[async_trait]
impl Provider for CapturingProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new("capturing")
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
        Ok(vec![ModelInfo::new("test-model", "capturing")])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (content, stop_reason) = if call == 0 {
            (
                vec![ContentBlock::ToolUse {
                    id: "large-output-1".to_string(),
                    name: TOOL_NAME.to_string(),
                    input: json!({}),
                }],
                Some("tool_use".to_string()),
            )
        } else {
            let visible = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { content, .. } => Some(content.to_display_string()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.tool_results
                .lock()
                .expect("tool-result recorder")
                .extend(visible);
            (vec![ContentBlock::text("done")], None)
        };

        Ok(provider_event_stream_from_response(Response {
            id: format!("capture-{call}"),
            model: "test-model".to_string(),
            role: Role::Assistant,
            content,
            stop_reason,
            usage: None,
        }))
    }
}

struct FixedOutput(String);

impl ToolDefinition for FixedOutput {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(TOOL_NAME)
            .description("returns a fixed test payload")
            .input_schema(json!({"type": "object", "properties": {}}))
            .build()
    }
}

#[async_trait]
impl ToolExecutor for FixedOutput {
    async fn execute(&self, _ctx: ParallelToolContext, _input: serde_json::Value) -> ToolResult {
        Ok(self.0.clone())
    }
}

fn pinned(workspace: &Path, runtime: Arc<Runtime>) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_runtime(runtime)
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
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
        .with_memory(MemoryConfig::disabled())
}

async fn provider_visible_result(output: String, policy: Option<ToolResultPolicy>) -> String {
    let provider = CapturingProvider::new();
    let mut builder = Runtime::builder()
        .with_provider_instance(provider.clone())
        .with_tool(FixedOutput(output))
        .with_ephemeral_history();
    if let Some(policy) = policy {
        builder = builder.with_tool_result_policy(policy);
    }
    let runtime = Arc::new(builder.build().expect("the Basis runtime builds"));

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("AGENTS.md"), "house rules").expect("write context");
    let workspace = pinned(workspace.path(), runtime)
        .open()
        .await
        .expect("the Basis workspace opens");
    let report = workspace
        .prepare("call the large-output tool")
        .expect("the Basis run prepares")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the Basis run completes");
    assert!(matches!(report.outcome, RunOutcome::Ok));

    provider.last_tool_result()
}

#[tokio::test]
async fn unlimited_policy_preserves_compact_output_larger_than_fifty_kibibytes() {
    let output = format!("BEGIN:{}:END", "x".repeat(60 * 1024));

    let visible =
        provider_visible_result(output.clone(), Some(ToolResultPolicy::unlimited())).await;

    assert_eq!(visible, output);
}

#[tokio::test]
async fn unlimited_policy_preserves_more_than_two_thousand_physical_lines() {
    let output = (0..2_101)
        .map(|line| format!("physical-line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let visible =
        provider_visible_result(output.clone(), Some(ToolResultPolicy::unlimited())).await;

    assert_eq!(visible, output);
}

#[tokio::test]
async fn omitted_policy_preserves_mentras_existing_default_limits() {
    let compact = format!("BEGIN:{}:END", "x".repeat(60 * 1024));
    let compact_visible = provider_visible_result(compact.clone(), None).await;

    assert_ne!(compact_visible, compact);
    assert!(compact_visible.contains("[truncated:"));

    let many_lines = (0..2_101)
        .map(|line| format!("physical-line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let lines_visible = provider_visible_result(many_lines.clone(), None).await;

    assert_ne!(lines_visible, many_lines);
    assert!(lines_visible.contains("showing 2000 of 2101 lines"));
}

#[tokio::test]
async fn bounded_policy_maps_byte_line_and_no_spill_posture() {
    let output = "line-one\nline-two\nline-three".to_string();
    let policy = ToolResultPolicy::new(usize::MAX, 2, false);

    let visible = provider_visible_result(output, Some(policy)).await;

    assert!(visible.contains("showing 2 of 3 lines"), "{visible}");
    assert!(
        visible.contains("spill-to-file is disabled by runtime policy"),
        "{visible}"
    );
}
