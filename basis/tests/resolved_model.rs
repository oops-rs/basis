//! Host-resolved models through the workspace boundary.
//!
//! A host that already has complete model metadata must be able to inject it
//! without asking the provider's catalogue to rediscover the same answer.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[cfg(feature = "mcp")]
use basis::McpConfig;
use basis::{
    Config, ContextConfig, MemoryConfig, ModelInfo, ModelSelector, Provider, RunError, Runtime,
    Workspace, WorkspaceBuilder, async_trait,
    hooks::HooksConfig,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::declared::ToolsConfig,
};

#[derive(Clone, Default)]
struct CatalogProbe {
    listings: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
}

impl CatalogProbe {
    fn listings(&self) -> usize {
        self.listings.load(Ordering::SeqCst)
    }

    fn streams(&self) -> usize {
        self.streams.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for CatalogProbe {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new("probe")
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
        Ok(vec![
            ModelInfo::new("catalog-model", "probe").with_context_window(41_000),
        ])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.streams.fetch_add(1, Ordering::SeqCst);
        Ok(provider_event_stream_from_response(Response {
            id: "probe-response".to_string(),
            model: "catalog-model".to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text("done")],
            stop_reason: None,
            usage: None,
        }))
    }
}

fn runtime(probe: CatalogProbe) -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_provider_instance(probe)
            .with_ephemeral_history()
            .build()
            .expect("the probe runtime builds offline"),
    )
}

fn pinned(workspace: &Path, runtime: Arc<Runtime>) -> WorkspaceBuilder {
    let builder = Workspace::builder(workspace)
        .with_runtime(runtime)
        .with_config(Config::default())
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

fn injected_model() -> ModelInfo {
    let mut model = ModelInfo::new("host-model", "probe").with_context_window(262_144);
    model.display_name = Some("Host model".to_string());
    model.description = Some("Resolved before Basis opens the workspace".to_string());
    model
}

#[tokio::test]
async fn a_resolved_model_bypasses_the_catalog_and_preserves_its_context_window() {
    let probe = CatalogProbe::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = pinned(dir.path(), runtime(probe.clone()))
        .with_resolved_model(injected_model())
        .open()
        .await
        .expect("the resolved model opens without provider activity");

    assert_eq!(workspace.model(), "host-model");
    assert_eq!(probe.listings(), 0, "the catalog must be bypassed");
    assert_eq!(probe.streams(), 0, "opening must not start a turn");

    let prepared = workspace.prepare("inspect the model").expect("mints");
    assert_eq!(prepared.context_window(), Some(262_144));
    assert_eq!(probe.listings(), 0, "minting must not resolve again");
    assert_eq!(probe.streams(), 0, "minting must not start a turn");
}

#[tokio::test]
async fn a_resolved_model_for_another_provider_is_a_typed_pre_activity_error() {
    let probe = CatalogProbe::default();
    let dir = tempfile::tempdir().expect("workspace");
    let error = pinned(dir.path(), runtime(probe.clone()))
        .with_resolved_model(ModelInfo::new("foreign-model", "other-provider"))
        .open()
        .await
        .expect_err("provider identity is part of the resolved contract");

    assert!(
        matches!(
            error,
            RunError::ResolvedModelProviderMismatch {
                ref model,
                ref model_provider,
                ref runtime_provider,
            } if model == "foreign-model"
                && model_provider == "other-provider"
                && runtime_provider == "probe"
        ),
        "{error}"
    );
    assert_eq!(probe.listings(), 0, "mismatch must not consult the catalog");
    assert_eq!(probe.streams(), 0, "mismatch must not start a turn");
}

#[tokio::test]
async fn a_later_selector_replaces_a_resolved_model() {
    let probe = CatalogProbe::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = pinned(dir.path(), runtime(probe.clone()))
        .with_resolved_model(injected_model())
        .with_model(ModelSelector::Id("catalog-model".to_string()))
        .open()
        .await
        .expect("the later selector resolves normally");

    assert_eq!(workspace.model(), "catalog-model");
    assert_eq!(probe.listings(), 1, "selector behavior stays unchanged");
    assert_eq!(
        workspace
            .prepare("inspect the model")
            .expect("mints")
            .context_window(),
        Some(41_000)
    );
}

#[tokio::test]
async fn a_later_resolved_model_replaces_a_selector() {
    let probe = CatalogProbe::default();
    let dir = tempfile::tempdir().expect("workspace");
    let workspace = pinned(dir.path(), runtime(probe.clone()))
        .with_model(ModelSelector::Id("catalog-model".to_string()))
        .with_resolved_model(injected_model())
        .open()
        .await
        .expect("the later resolved model wins without catalog activity");

    assert_eq!(workspace.model(), "host-model");
    assert_eq!(
        probe.listings(),
        0,
        "the replaced selector must not resolve"
    );
    assert_eq!(
        workspace
            .prepare("inspect the model")
            .expect("mints")
            .context_window(),
        Some(262_144)
    );
}
