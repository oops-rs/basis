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
    provider::{ReasoningEffort, ReasoningOptions},
    provider_core::{StaticCredentialSource, responses, responses::ResponsesProvider},
};
use thiserror::Error;

use crate::{
    approval::{ApprovalPolicy, Approver, PolicyAuthorizer},
    context::{ContextConfig, ContextError, WorkspaceContext},
    event::RunOutcome,
    hooks::{self, HookRunner, HooksConfig},
    mcp::{self, McpConfig, McpError},
    provider::{self, ProviderError},
    shell::ShellAccess,
    skills::{self, SkillsConfig},
    templates::{self, Template, TemplatesConfig},
};

pub use prepared::{LoadedSkill, PreparedRun, RunContext, TurnOptions};
pub use sink::{CollectingSink, EventSink, FnSink, NullSink};

/// Default name for the session a run creates. Sessions are named so a client
/// can tell them apart; the name carries no behavior.
const DEFAULT_SESSION_NAME: &str = "lan run";

/// How hard the model should think before answering.
///
/// lan's own enum rather than a re-export, for the reason [`Event`] and
/// [`TurnOptions`] are: the surface lan promises should not move when mentra's
/// does. Only providers with a reasoning control honor it; the rest ignore it,
/// which is why there is no "unsupported" error to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl From<Effort> for ReasoningEffort {
    fn from(effort: Effort) -> Self {
        match effort {
            Effort::Low => Self::Low,
            Effort::Medium => Self::Medium,
            Effort::High => Self::High,
        }
    }
}

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
    /// Which MCP servers this run connects, and where to look for more.
    pub mcp: McpConfig,
    /// Where to look for prompt templates. Discovered, never executed here —
    /// a template becomes a prompt only when something renders it.
    pub templates: TemplatesConfig,
    /// Where to look for subprocess hooks — external commands with a say over
    /// each tool call.
    pub hooks: HooksConfig,
    /// Whether the agent may run commands. Denied unless granted; see
    /// ADR-0006.
    pub shell: ShellAccess,
    /// When the agent must ask before doing something consequential.
    pub approval: ApprovalPolicy,
    /// How hard the model should think. `None` leaves the provider's default.
    pub effort: Option<Effort>,
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
            mcp: McpConfig::default(),
            templates: TemplatesConfig::default(),
            hooks: HooksConfig::default(),
            // Not read from the environment here: a library default must not
            // depend on ambient state. The binary reads LAN_ALLOW_SHELL and
            // calls `with_shell` explicitly.
            shell: ShellAccess::Denied,
            approval: ApprovalPolicy::default(),
            effort: None,
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

    /// Sets which MCP servers the run connects.
    ///
    /// Servers arrive from three places — the caller's own list, the
    /// workspace's `.mcp.json`, and the global one — and this is where the
    /// first of those goes. See [`crate::mcp`] for the precedence.
    pub fn with_mcp(self, mcp: McpConfig) -> Self {
        Self { mcp, ..self }
    }

    pub fn with_templates(self, templates: TemplatesConfig) -> Self {
        Self { templates, ..self }
    }

    /// Sets where subprocess hooks are discovered.
    ///
    /// A hook is an external command that gets a say over each tool call; see
    /// [`crate::hooks`] for the wire contract and for what happens when one
    /// breaks.
    pub fn with_hooks(self, hooks: HooksConfig) -> Self {
        Self { hooks, ..self }
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

    /// Asks the model to think harder, where the provider supports it.
    pub fn with_effort(self, effort: Effort) -> Self {
        Self {
            effort: Some(effort),
            ..self
        }
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

    #[error("no session to resume")]
    NoSuchSession,

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

    #[error(transparent)]
    Mcp(#[from] McpError),

    #[error("failed to load prompt templates: {0}")]
    Templates(#[from] crate::templates::TemplateError),

    #[error("failed to load hooks: {0}")]
    Hooks(#[from] crate::hooks::HookConfigError),
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

    prepare_without_prompt(config).await
}

/// Builds a session with no prompt in hand yet.
///
/// What a protocol server needs: ACP's `session/new` opens a conversation
/// before the user has typed anything, so the empty-prompt check that guards
/// [`prepare`] would reject exactly the case that matters. Prompts arrive later
/// through [`PreparedRun::send`], which does its own checking.
pub async fn prepare_without_prompt(config: RunConfig) -> Result<PreparedRun, RunError> {
    let resolved = resolve(&config).await?;

    let mut session = resolved.runtime.create_session_with_config(
        config.session_name.clone(),
        resolved.model.clone(),
        agent_config(&config, &resolved.context),
    )?;
    apply_effort(&mut session, config.effort)?;

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

    let mut session = resolved.runtime.resume_session(agent_id)?;
    apply_effort(&mut session, config.effort)?;

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
    templates_dirs: Vec<PathBuf>,
    templates: Vec<Template>,
    mcp_files: Vec<crate::event::ContextFile>,
    mcp_servers: Vec<String>,
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
            templates_dirs: self.templates_dirs,
            templates: self.templates,
            mcp_files: self.mcp_files,
            mcp_servers: self.mcp_servers,
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
            git_protected(
                RuntimePolicy::workspace_bounded(&config.workspace),
                &config.workspace,
            )
            .allow_shell_commands(config.shell.is_granted())
            .allow_background_commands(config.shell.is_granted()),
        )
        // Without an authorizer mentra allows every call unconditionally, and
        // no permission request can ever be raised.
        .with_tool_authorizer(PolicyAuthorizer::new(config.approval));

    // Loaded before the build so a hooks file that does not parse fails the run
    // loudly, rather than at the first tool call — or worse, never.
    //
    // One runner for every hook rather than one registration each: `with_pre_hook`
    // appends, so both work, but lan wants the ordering and the short-circuit to
    // be its own (see `crate::hooks`). A run with no hooks registers nothing, so
    // the mechanism costs nothing until someone writes the file.
    let hooks = hooks::load(&config.workspace, &config.hooks)?;
    let builder = if hooks.is_empty() {
        builder
    } else {
        builder.with_pre_hook(HookRunner::new(&config.workspace, hooks))
    };

    // MCP servers are registered on the builder and connected by `build_async`,
    // so this must happen before the build. mentra's `McpRegistration` is
    // private, which is why the fold matches here rather than in `crate::mcp`.
    //
    // Discovery is run for its own sake as well: the header names which files
    // took effect, and an `.mcp.json` is the last thing that should apply
    // invisibly — it says which programs to spawn.
    let mcp_files: Vec<crate::event::ContextFile> = mcp::discover(&config.workspace, &config.mcp)?
        .iter()
        .map(|source| crate::event::ContextFile {
            path: source.path.clone(),
            scope: source.scope.label(),
        })
        .collect();

    let servers = mcp::servers(&config.workspace, &config.mcp)?;
    let mcp_servers: Vec<String> = servers
        .iter()
        .map(|server| server.name().to_string())
        .collect();

    let builder = servers
        .into_iter()
        .fold(builder, |builder, server| match server {
            mcp::McpServer::Stdio(server) => builder.with_mcp_server(server),
            mcp::McpServer::Sse(server) => builder.with_mcp_sse_server(server),
        });

    let runtime = match &choice.base_url {
        Some(base_url) => {
            builder.with_registered_provider(compatible_provider(base_url, &choice.api_key))
        }
        None => builder.with_provider(choice.provider, choice.api_key.clone()),
    }
    // `build` ignores MCP configuration outright; only `build_async` opens the
    // connections. Always the async one, so a server can never be dropped by
    // the choice of constructor.
    .build_async()
    .await?;

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

    // Templates need no runtime registration — they are lan-side convention
    // data, rendered into a prompt by whatever surface offers them.
    let (templates_dirs, templates) = load_templates(config)?;

    Ok(Resolved {
        runtime,
        model,
        provider: ProviderId::from(choice.provider).to_string(),
        context,
        skills_dirs,
        skills,
        templates_dirs,
        templates,
        mcp_files,
        mcp_servers,
    })
}

/// Prepares a run against a session the caller already built, so a host with
/// its own runtime — custom tools, its own store, a provider lan does not
/// know — still gets lan's context discovery and event stream.
///
/// The prompt in `config` may be empty. Once a session outlives a turn, a
/// conversation with nothing said yet is a real state — it is what ACP's
/// `session/new` opens — so the check belongs where a prompt is actually sent,
/// which is [`PreparedRun::execute`] and [`PreparedRun::send`].
pub fn prepare_with_session(
    session: Session,
    config: &RunConfig,
    provider: impl Into<String>,
    model: impl Into<String>,
) -> Result<PreparedRun, RunError> {
    let context = WorkspaceContext::discover_with(&config.workspace, &config.context)?;
    // Unlike skills, templates are registered on nothing — so lan can discover
    // them here without touching a runtime it does not own.
    let (templates_dirs, templates) = load_templates(config)?;

    Ok(PreparedRun::new(
        session,
        RunContext {
            workspace: resolved_workspace(&config.workspace, &context),
            prompt: config.prompt.clone(),
            provider: provider.into(),
            model: model.into(),
            context,
            // The caller owns the runtime, so it owns skill and MCP
            // registration too.
            skills_dirs: Vec::new(),
            skills: Vec::new(),
            templates_dirs,
            templates,
            mcp_files: Vec::new(),
            mcp_servers: Vec::new(),
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

/// Loads every template the workspace defines, with the roots they came from.
///
/// A root that exists but holds a file lan cannot read is an error rather than
/// an empty command list: a template that failed to load and a template nobody
/// wrote look the same from a client, and only one of them is worth knowing
/// about.
fn load_templates(config: &RunConfig) -> Result<(Vec<PathBuf>, Vec<Template>), RunError> {
    let sources = templates::discover(&config.workspace, &config.templates);
    let dirs: Vec<PathBuf> = sources.iter().map(|source| source.path.clone()).collect();

    Ok((dirs, templates::load_sources(&sources)?))
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

/// Asks the model for a reasoning effort, when one was requested.
///
/// A provider without a reasoning control ignores the request rather than
/// failing, so this is safe to call unconditionally — which is why `None`
/// leaves the session untouched instead of sending a default nobody asked for.
fn apply_effort(session: &mut Session, effort: Option<Effort>) -> Result<(), RunError> {
    let Some(effort) = effort else {
        return Ok(());
    };

    session.set_reasoning(Some(ReasoningOptions {
        effort: Some(effort.into()),
        summary: None,
    }))?;

    Ok(())
}

/// Keeps the parts of `.git` that decide what *runs* out of reach.
///
/// `.git/hooks` holds programs git executes on ordinary operations, and
/// `.git/config` can name more of them (`core.hooksPath`, and the `filter`/
/// `diff` drivers that run on checkout). Writing either turns a file edit into
/// code execution outside anything lan's policy or approval covers, which is
/// why they are singled out rather than denying `.git` wholesale — an agent
/// legitimately reads `.git`, and `git` itself must keep writing objects and
/// refs underneath it.
///
/// **This binds the builtin file tools, not the shell.** A command like
/// `sh -c 'echo … > .git/hooks/pre-commit'` still reaches the path, because
/// nothing here parses shell. It closes the route a model actually takes and
/// remains hygiene; per ADR-0004 the boundary is the container's.
fn git_protected(policy: RuntimePolicy, workspace: &Path) -> RuntimePolicy {
    let git = workspace.join(".git");
    policy
        .with_denied_write_root(git.join("hooks"))
        .with_denied_write_root(git.join("config"))
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

    #[test]
    fn asking_for_no_effort_leaves_the_provider_default() {
        let config = RunConfig::new("/repo", "prompt");

        assert_eq!(config.effort, None);
        assert_eq!(
            config.clone().with_effort(Effort::High).effort,
            Some(Effort::High)
        );
        assert_eq!(config.effort, None, "the original is untouched");
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
