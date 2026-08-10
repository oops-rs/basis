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
    approval::{ApprovalPolicy, Approver, PolicyAuthorizer},
    context::{ContextConfig, ContextError, WorkspaceContext},
    event::RunOutcome,
    provider::{self, ProviderError},
    shell::ShellAccess,
    skills::{self, SkillsConfig},
};

pub use prepared::{LoadedSkill, PreparedRun, RunContext, TurnOptions};
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
    /// Whether the agent may run commands. Denied unless granted; see
    /// ADR-0006.
    pub shell: ShellAccess,
    /// When the agent must ask before doing something consequential.
    pub approval: ApprovalPolicy,
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
            // Not read from the environment here: a library default must not
            // depend on ambient state. The binary reads LAN_ALLOW_SHELL and
            // calls `with_shell` explicitly.
            shell: ShellAccess::Denied,
            approval: ApprovalPolicy::default(),
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

    /// Grants or denies command execution.
    ///
    /// Granting asserts that something outside this process confines the
    /// workspace — a container, or a per-command sandbox. lan takes the
    /// caller's word for it and never infers it (ADR-0006).
    pub fn with_shell(self, shell: ShellAccess) -> Self {
        Self { shell, ..self }
    }

    /// Sets when the agent must ask before acting.
    ///
    /// [`ApprovalPolicy::Prompt`] only means anything if the run is given an
    /// approver — see [`PreparedRun::execute_with_approver`].
    pub fn with_approval(self, approval: ApprovalPolicy) -> Self {
        Self { approval, ..self }
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

    #[error("failed to load skills: {0}")]
    Skills(#[from] mentra::SkillLoadError),
}

/// Runs one prompt to completion, streaming events into `sink`.
///
/// A setup failure — no credential, unreachable model, unreadable workspace —
/// is an `Err`. A failure *during* the turn is reported as
/// [`RunOutcome::Error`] on an otherwise complete stream, because by then the
/// events already emitted are worth keeping.
///
/// The session is dropped when this returns. For a conversation, keep the
/// [`PreparedRun`] from [`prepare`] and call
/// [`send`](PreparedRun::send) on it.
pub async fn run<S: EventSink>(config: RunConfig, sink: S) -> Result<RunReport<S>, RunError> {
    prepare(config).await?.execute(sink).await
}

/// Runs one prompt, routing approval requests to `approver`.
///
/// Only meaningful with [`ApprovalPolicy::Prompt`]; under the other policies
/// nothing is ever asked.
pub async fn run_with_approver<S: EventSink, A: Approver>(
    config: RunConfig,
    sink: S,
    approver: A,
) -> Result<RunReport<S>, RunError> {
    prepare(config)
        .await?
        .execute_with_approver(sink, approver)
        .await
}

/// Resolves everything a run needs — context, credential, runtime, model,
/// session — without sending the prompt.
pub async fn prepare(config: RunConfig) -> Result<PreparedRun, RunError> {
    if config.prompt.trim().is_empty() {
        return Err(RunError::EmptyPrompt);
    }

    let resolved = resolve(&config).await?;

    let session = resolved.runtime.create_session_with_config(
        config.session_name.clone(),
        resolved.model.clone(),
        agent_config(&config, &resolved.context),
    )?;

    Ok(PreparedRun::new(
        session,
        resolved.into_context(config.prompt, &config.workspace),
    ))
}

/// Picks up a conversation a previous process left behind.
///
/// `agent_id` is [`PreparedRun::agent_id`], not the session id: mentra persists
/// agents, and a session is one process's view of one. Resuming replays the
/// transcript from the store, so the first turn after this already knows
/// everything the last one did.
///
/// `config.prompt` may be empty here — a caller that resumes to inspect the
/// history, or to send a prompt chosen later, has nothing to say yet.
pub async fn resume(agent_id: &str, config: RunConfig) -> Result<PreparedRun, RunError> {
    let resolved = resolve(&config).await?;

    let session = resolved.runtime.resume_session(agent_id)?;

    Ok(PreparedRun::new(
        session,
        resolved.into_context(config.prompt.clone(), &config.workspace),
    ))
}

/// A runtime and everything resolved alongside it, before a session exists.
///
/// Shared by [`prepare`] and [`resume`] so the two cannot disagree about how a
/// runtime is built — the policy, the authorizer, the provider, and the skills
/// are the same questions whether the conversation is new or continuing.
struct Resolved {
    runtime: Runtime,
    model: mentra::ModelInfo,
    provider: String,
    context: WorkspaceContext,
    skills_dirs: Vec<PathBuf>,
    skills: Vec<LoadedSkill>,
}

impl Resolved {
    fn into_context(self, prompt: String, requested_workspace: &Path) -> RunContext {
        RunContext {
            workspace: resolved_workspace(requested_workspace, &self.context),
            prompt,
            provider: self.provider,
            model: self.model.id,
            context: self.context,
            skills_dirs: self.skills_dirs,
            skills: self.skills,
        }
    }
}

async fn resolve(config: &RunConfig) -> Result<Resolved, RunError> {
    let context = WorkspaceContext::discover_with(&config.workspace, &config.context)?;
    let choice = provider::resolve(config.provider, config.base_url.as_deref())?;

    let builder = Runtime::builder()
        // Path roots are hygiene, not a boundary: per ADR-0004 that is the
        // kernel's job. Commands are the case where the difference bites, so
        // they stay off unless the caller has granted them (ADR-0006).
        .with_policy(
            RuntimePolicy::workspace_bounded(&config.workspace)
                .allow_shell_commands(config.shell.is_granted())
                .allow_background_commands(config.shell.is_granted()),
        )
        // Without an authorizer mentra allows every call unconditionally, and
        // no permission request can ever be raised.
        .with_tool_authorizer(PolicyAuthorizer::new(config.approval));

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
    let skills_dirs = register_skills(&runtime, config)?;
    let skills = runtime
        .skills()
        .into_iter()
        .map(|skill| LoadedSkill {
            name: skill.name,
            description: skill.description,
            path: skill.path,
        })
        .collect();

    Ok(Resolved {
        runtime,
        model,
        provider: ProviderId::from(choice.provider).to_string(),
        context,
        skills_dirs,
        skills,
    })
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
            skills_dirs: Vec::new(),
            skills: Vec::new(),
        },
    ))
}

/// Registers every skills directory that exists, most specific first.
///
/// Roots layer rather than replace, so a workspace skill shadows a personal
/// one of the same name and everything else from the global root still loads.
fn register_skills(runtime: &Runtime, config: &RunConfig) -> Result<Vec<PathBuf>, RunError> {
    let sources = skills::discover(&config.workspace, &config.skills);
    let paths: Vec<PathBuf> = sources.iter().map(|source| source.path.clone()).collect();

    runtime.register_skills_dirs(&paths)?;

    Ok(paths)
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
    fn commands_are_denied_unless_granted() {
        let config = RunConfig::new("/repo", "prompt");

        assert_eq!(config.shell, ShellAccess::Denied);
        assert!(!config.shell.is_granted());
    }

    #[test]
    fn granting_shell_returns_a_new_config() {
        let base = RunConfig::new("/repo", "prompt");
        let granted = base.clone().with_shell(ShellAccess::Granted);

        assert_eq!(base.shell, ShellAccess::Denied, "the original is untouched");
        assert_eq!(granted.shell, ShellAccess::Granted);
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
