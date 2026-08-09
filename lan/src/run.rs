//! One prompt against one workspace: the smallest complete thing lan does.
//!
//! This is the P1 acceptance surface from `docs/ARCHITECTURE.md` §6 —
//! arbitrary prompts on arbitrary repos, in-process and as a subprocess. The
//! binary is a thin shell over [`run`]; a Rust host calls it directly.
//!
//! Nothing here knows what the prompt is for. The mission arrives as data: the
//! prompt itself, the workspace's own context files, and configuration.
//!
//! [`run`] answers the whole question — build a runtime, resolve a model, send
//! the prompt. A host that already owns a mentra runtime skips to
//! [`prepare_with_session`] and keeps its own.

mod prepared;
mod sink;

use std::path::{Path, PathBuf};

use mentra::{
    BuiltinProvider, ModelSelector, ProviderId, Runtime, RuntimePolicy, Session,
    agent::{AgentConfig, WorkspaceConfig},
    provider_core::{StaticCredentialSource, responses, responses::ResponsesProvider},
};
use thiserror::Error;

use crate::{
    context::{ContextConfig, ContextError, WorkspaceContext},
    event::RunOutcome,
    provider::{self, ProviderError},
    skills::{self, SkillsConfig},
};

pub use prepared::{PreparedRun, RunContext};
pub use sink::{CollectingSink, EventSink, FnSink, NullSink};

/// Default name for the session a run creates. Sessions are named so a client
/// can tell them apart; the name carries no behavior.
const DEFAULT_SESSION_NAME: &str = "lan run";

/// Everything a run needs. Task-specific behavior lives in `prompt` and in the
/// workspace, never in this struct.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub workspace: PathBuf,
    pub prompt: String,
    /// `None` auto-detects from the environment.
    pub provider: Option<BuiltinProvider>,
    /// An OpenAI-compatible endpoint to use instead of the provider's own
    /// service. `None` falls back to `LAN_BASE_URL` / `OPENAI_BASE_URL`.
    pub base_url: Option<String>,
    pub model: ModelSelector,
    pub context: ContextConfig,
    pub skills: SkillsConfig,
    pub session_name: String,
}

impl RunConfig {
    pub fn new(workspace: impl Into<PathBuf>, prompt: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            prompt: prompt.into(),
            provider: None,
            base_url: None,
            model: ModelSelector::NewestAvailable,
            context: ContextConfig::default(),
            skills: SkillsConfig::default(),
            session_name: DEFAULT_SESSION_NAME.to_string(),
        }
    }

    pub fn with_provider(self, provider: BuiltinProvider) -> Self {
        Self {
            provider: Some(provider),
            ..self
        }
    }

    /// Points the run at an OpenAI-compatible endpoint. A trailing `/v1` is
    /// stripped during resolution — paste the URL a gateway publishes.
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            ..self
        }
    }

    pub fn with_model(self, model: ModelSelector) -> Self {
        Self { model, ..self }
    }

    pub fn with_context(self, context: ContextConfig) -> Self {
        Self { context, ..self }
    }

    pub fn with_skills(self, skills: SkillsConfig) -> Self {
        Self { skills, ..self }
    }

    pub fn with_session_name(self, session_name: impl Into<String>) -> Self {
        Self {
            session_name: session_name.into(),
            ..self
        }
    }
}

/// What a completed run produced, alongside the sink it wrote to.
#[derive(Debug)]
pub struct RunReport<S> {
    pub session_id: String,
    pub model: String,
    pub provider: String,
    /// The assistant's final message, absent when the run failed.
    pub final_message: Option<String>,
    pub outcome: RunOutcome,
    pub sink: S,
}

impl<S> RunReport<S> {
    pub fn succeeded(&self) -> bool {
        matches!(self.outcome, RunOutcome::Ok)
    }
}

#[derive(Debug, Error)]
pub enum RunError {
    #[error("prompt is empty")]
    EmptyPrompt,

    #[error(transparent)]
    Context(#[from] ContextError),

    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error("runtime error: {0}")]
    Runtime(#[from] mentra::error::RuntimeError),

    #[error("failed to write an event: {0}")]
    Sink(#[from] std::io::Error),

    #[error("event forwarding task failed: {0}")]
    Forwarder(#[from] tokio::task::JoinError),

    /// Carried as text because `SkillLoadError` is `pub` inside a private
    /// module, so `register_skills_dir`'s error type cannot be named by a
    /// caller (oops-rs/mentra#8). Becomes a typed `#[from]` variant when that
    /// lands.
    #[error("failed to load skills from {path}: {message}")]
    Skills { path: PathBuf, message: String },
}

/// Runs one prompt to completion, streaming events into `sink`.
///
/// A setup failure — no credential, unreachable model, unreadable workspace —
/// is an `Err`. A failure *during* the turn is reported as
/// [`RunOutcome::Error`] on an otherwise complete stream, because by then the
/// events already emitted are worth keeping.
pub async fn run<S: EventSink>(config: RunConfig, sink: S) -> Result<RunReport<S>, RunError> {
    prepare(config).await?.execute(sink).await
}

/// Resolves everything a run needs — context, credential, runtime, model,
/// session — without sending the prompt.
pub async fn prepare(config: RunConfig) -> Result<PreparedRun, RunError> {
    if config.prompt.trim().is_empty() {
        return Err(RunError::EmptyPrompt);
    }

    let context = WorkspaceContext::discover_with(&config.workspace, &config.context)?;
    let choice = provider::resolve(config.provider, config.base_url.as_deref())?;

    let builder = Runtime::builder()
        // The in-process boundary. Per ADR-0004 this is hygiene, not the
        // security boundary — that is the kernel's job (Docker in P4).
        .with_policy(RuntimePolicy::workspace_bounded(&config.workspace));

    let runtime = match &choice.base_url {
        Some(base_url) => {
            builder.with_registered_provider(compatible_provider(base_url, &choice.api_key))
        }
        None => builder.with_provider(choice.provider, choice.api_key.clone()),
    }
    .build()?;

    let model = runtime
        .resolve_model(choice.provider, config.model.clone())
        .await?;

    // Skills must be registered on the runtime before the session spawns, so
    // the agent's tool roster includes `load_skill`.
    let skills_dir = register_skills(&runtime, &config)?;

    let session = runtime.create_session_with_config(
        config.session_name.clone(),
        model.clone(),
        agent_config(&config, &context),
    )?;

    Ok(PreparedRun::new(
        session,
        RunContext {
            workspace: resolved_workspace(&config.workspace, &context),
            prompt: config.prompt,
            provider: ProviderId::from(choice.provider).to_string(),
            model: model.id,
            context,
            skills_dir,
        },
    ))
}

/// Registers the most specific skills directory that exists.
///
/// Only one, because `register_skills_dir` replaces rather than merges
/// (oops-rs/mentra#8). When several exist, the extras are reported rather than
/// silently ignored — a user who put skills in two places should learn that
/// only one is live.
fn register_skills(runtime: &Runtime, config: &RunConfig) -> Result<Option<PathBuf>, RunError> {
    let sources = skills::discover(&config.workspace, &config.skills);
    let Some(active) = sources.first() else {
        return Ok(None);
    };

    runtime
        .register_skills_dir(&active.path)
        .map_err(|error| RunError::Skills {
            path: active.path.clone(),
            message: error.to_string(),
        })?;

    for ignored in sources.iter().skip(1) {
        eprintln!(
            "lan: using skills from {} ({}); ignoring {} — one directory at a \
             time until oops-rs/mentra#8 lands",
            active.path.display(),
            active.scope.label(),
            ignored.path.display(),
        );
    }

    Ok(Some(active.path.clone()))
}

/// Prepares a run against a session the caller already built, so a host with
/// its own runtime — custom tools, its own store, a provider lan does not
/// know — still gets lan's context discovery and event stream.
pub fn prepare_with_session(
    session: Session,
    config: &RunConfig,
    provider: impl Into<String>,
    model: impl Into<String>,
) -> Result<PreparedRun, RunError> {
    if config.prompt.trim().is_empty() {
        return Err(RunError::EmptyPrompt);
    }

    let context = WorkspaceContext::discover_with(&config.workspace, &config.context)?;

    Ok(PreparedRun::new(
        session,
        RunContext {
            workspace: resolved_workspace(&config.workspace, &context),
            prompt: config.prompt.clone(),
            provider: provider.into(),
            model: model.into(),
            context,
            // The caller owns the runtime, so it owns skill registration too.
            skills_dir: None,
        },
    ))
}

/// Builds a provider aimed at an OpenAI-compatible endpoint.
///
/// mentra's OpenAI preset is the right shape — the Responses wire format and
/// bearer auth — so lan takes that definition and swaps only the base URL,
/// rather than describing a provider from scratch and drifting from whatever
/// mentra's preset learns next.
fn compatible_provider(base_url: &str, api_key: &str) -> ResponsesProvider<StaticCredentialSource> {
    let mut definition = responses::openai_definition();
    definition.base_url = Some(base_url.to_string());
    definition.descriptor.display_name = Some(format!("OpenAI-compatible ({base_url})"));

    ResponsesProvider::new(definition, StaticCredentialSource::new(api_key))
}

/// The workspace as discovery resolved it, falling back to what was asked for.
///
/// Discovery follows symlinks so the parent walk is meaningful, which means a
/// document's path can sit under a different spelling of the same directory
/// than the caller typed. Reporting the resolved root keeps the header
/// internally consistent — `workspace` and `context_files` name one place.
fn resolved_workspace(requested: &Path, context: &WorkspaceContext) -> PathBuf {
    context
        .root()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| requested.to_path_buf())
}

/// Turns discovered context into the agent's system prompt, and scopes the
/// agent to the workspace. Everything else stays at mentra's defaults —
/// opinions belong in the prompt and the workspace, not here.
fn agent_config(config: &RunConfig, context: &WorkspaceContext) -> AgentConfig {
    AgentConfig {
        system: context.render(),
        workspace: WorkspaceConfig {
            base_dir: config.workspace.clone(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_carries_no_task_specific_defaults() {
        let config = RunConfig::new("/repo", "do the thing");

        assert_eq!(config.provider, None);
        assert!(matches!(config.model, ModelSelector::NewestAvailable));
        assert_eq!(config.session_name, DEFAULT_SESSION_NAME);
    }

    #[test]
    fn builders_return_new_values() {
        let base = RunConfig::new("/repo", "prompt");
        let derived = base
            .clone()
            .with_provider(BuiltinProvider::Anthropic)
            .with_session_name("named");

        assert_eq!(base.provider, None, "the original must be untouched");
        assert_eq!(derived.provider, Some(BuiltinProvider::Anthropic));
        assert_eq!(derived.session_name, "named");
    }

    #[test]
    fn context_becomes_the_system_prompt_and_the_workspace_is_scoped() {
        let context = WorkspaceContext::from_documents(vec![crate::context::ContextDocument {
            path: PathBuf::from("/repo/AGENTS.md"),
            scope: crate::context::ContextScope::Workspace,
            content: "house rules".to_string(),
        }]);
        let config = RunConfig::new("/repo", "prompt");

        let agent = agent_config(&config, &context);

        assert!(
            agent
                .system
                .expect("a system prompt")
                .contains("house rules")
        );
        assert_eq!(agent.workspace.base_dir, PathBuf::from("/repo"));
    }

    #[test]
    fn an_empty_workspace_context_leaves_the_system_prompt_unset() {
        let config = RunConfig::new("/repo", "prompt");

        let agent = agent_config(&config, &WorkspaceContext::default());

        assert_eq!(agent.system, None);
    }

    #[tokio::test]
    async fn an_empty_prompt_is_rejected_before_any_provider_work() {
        let config = RunConfig::new("/definitely/not/a/real/path", "   \n ");

        let error = prepare(config).await.expect_err("rejected");

        // Reaching provider resolution or workspace validation would prove the
        // check ran too late.
        assert!(matches!(error, RunError::EmptyPrompt));
    }

    #[tokio::test]
    async fn a_missing_workspace_fails_before_a_provider_is_needed() {
        let config = RunConfig::new("/definitely/not/a/real/path", "hello");

        let error = prepare(config).await.expect_err("rejected");

        assert!(matches!(
            error,
            RunError::Context(ContextError::WorkspaceMissing { .. })
        ));
    }
}
