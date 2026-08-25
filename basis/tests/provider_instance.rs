//! A prepared run end to end on a provider the host constructed (decision
//! D5a): the seam mentra always had, reached through basis's own surface.
//!
//! Written against `basis` alone, deliberately — the trait from the crate
//! root, everything the implementation touches from `basis::runtime`'s
//! provider-authoring re-exports — so this file is the compile-time check
//! that the promised set is complete enough to write a real provider
//! against, the same enforcement `Nowhere` gives the executor re-exports in
//! the builder's own tests.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use basis::{
    CollectingSink, ContextConfig, ModelSelector, Provider, RunOutcome, Runtime, Workspace,
    WorkspaceBuilder, async_trait,
    hooks::HooksConfig,
    runtime::{
        ContentBlock, ModelInfo, ProviderCapabilities, ProviderDescriptor, ProviderError,
        ProviderEventStream, Request, Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::declared::ToolsConfig,
};

/// The shape a host's provider actually takes: answers every turn with one
/// scripted message, counts how often it was asked, and never opens a socket.
struct Scripted {
    asked: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Scripted {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new("scripted")
    }

    /// Listing and streaming are claimed because the run exercises both: the
    /// pinned model resolves through the listing before the first turn, and
    /// the turn itself arrives as a stream.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo::new("scripted-model", "scripted")])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let asked = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(provider_event_stream_from_response(Response {
            id: format!("scripted-{asked}"),
            model: "scripted-model".to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text("the scripted answer")],
            stop_reason: None,
            usage: None,
        }))
    }
}

/// A workspace builder that looks nowhere except where the test put
/// something; `tests/workspace.rs` explains the choices.
fn pinned(workspace: &Path, runtime: Arc<Runtime>) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_runtime(runtime)
        .with_model(ModelSelector::Id("scripted-model".to_string()))
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
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
        })
}

#[tokio::test]
async fn a_prepared_run_streams_through_a_host_supplied_provider() {
    let asked = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::builder()
            .with_provider_instance(Scripted {
                asked: Arc::clone(&asked),
            })
            .with_ephemeral_history()
            .build()
            .expect("an instance needs no credential, no environment, no network"),
    );
    assert_eq!(
        runtime.provider(),
        "scripted",
        "the runtime answers to the descriptor's own id"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules").expect("write");
    let workspace = pinned(dir.path(), runtime).open().await.expect("opens");

    let report = workspace
        .prepare("say the line")
        .expect("mints")
        .execute(CollectingSink::default())
        .await
        .expect("completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(
        report.provider, "scripted",
        "every run reports the instance's id, not a builtin's"
    );
    assert_eq!(report.final_message.as_deref(), Some("the scripted answer"));
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "one prepared run is one turn through the instance"
    );
}
