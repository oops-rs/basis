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

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use mentra::{
    BuiltinProvider, ModelSelector, ProviderId, Runtime, RuntimePolicy, Session,
    agent::{AgentConfig, WorkspaceConfig},
    provider::{ReasoningEffort, ReasoningOptions},
    provider_core::{StaticCredentialSource, responses, responses::ResponsesProvider},
};
use thiserror::Error;

#[cfg(feature = "mcp")]
use crate::mcp::{self, McpConfig, McpError};
use crate::{
    approval::{ApprovalGate, Approver},
    context::{ContextConfig, ContextError, WorkspaceContext},
    event::{ContextFile, RunOutcome},
    hooks::{self, HookRunner, HooksConfig},
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
/// does. Provider adapters translate this semantic level to their own wire
/// format. A provider or model that does not offer the requested level returns
/// an error rather than silently lowering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl From<Effort> for ReasoningEffort {
    fn from(effort: Effort) -> Self {
        match effort {
            Effort::Low => Self::Low,
            Effort::Medium => Self::Medium,
            Effort::High => Self::High,
            Effort::XHigh => Self::XHigh,
            Effort::Max => Self::Max,
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
    /// service. These endpoints use complete local replay instead of automatic
    /// `previous_response_id` chaining. `None` falls back to `LAN_BASE_URL` /
    /// `OPENAI_BASE_URL`.
    pub base_url: Option<String>,
    pub model: ModelSelector,
    pub context: ContextConfig,
    pub skills: SkillsConfig,
    /// Which MCP servers this run connects, and where to look for more.
    #[cfg(feature = "mcp")]
    pub mcp: McpConfig,
    /// Where to look for prompt templates. Discovered, never executed here —
    /// a template becomes a prompt only when something renders it.
    pub templates: TemplatesConfig,
    /// Where to look for subprocess hooks — external commands with a say over
    /// each tool call.
    pub hooks: HooksConfig,
    /// Whether the agent may run commands. Granted by default; see ADR-0013.
    pub shell: ShellAccess,
    /// How hard the model should think. `None` leaves the provider's default;
    /// unsupported provider/model levels fail instead of being downgraded.
    pub effort: Option<Effort>,
    /// Gives up on the run after this long.
    ///
    /// Unset by default, and unset for an unattended caller too. An attended
    /// `lan run` has a person watching, who can tell "thinking hard" from
    /// "stuck" in a way no timer can; a caller nobody is watching has to write
    /// the bound down in advance, and with no scheduler shipped there is no
    /// period for lan to guess one from (ADR-0014).
    pub deadline: Option<Duration>,
    /// Caps how many tool calls the run may make.
    pub tool_budget: Option<usize>,
    /// Caps the tokens the run may report using, input plus output.
    ///
    /// Soft by construction: usage is only known once a round has streamed in
    /// full, so the round that crosses the line always finishes. This is the
    /// bound that maps to money.
    pub token_budget: Option<u64>,
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
            #[cfg(feature = "mcp")]
            mcp: McpConfig::default(),
            templates: TemplatesConfig::default(),
            hooks: HooksConfig::default(),
            // Granted, per ADR-0013, and from the enum's own default rather
            // than from anything ambient: what a run may do is stated here, in
            // the config, not read out of the environment behind the caller.
            shell: ShellAccess::default(),
            effort: None,
            deadline: None,
            tool_budget: None,
            token_budget: None,
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
    /// Compatible endpoints use complete local replay rather than automatic
    /// `previous_response_id` chaining.
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
    #[cfg(feature = "mcp")]
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
    /// Granted by default (ADR-0013). Denying is the read-only posture: it
    /// shuts the command tools and nothing else, so it is a narrowing of what
    /// this run does, never a claim about what the process could do.
    pub fn with_shell(self, shell: ShellAccess) -> Self {
        Self { shell, ..self }
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

    /// Gives up on the run after `deadline`.
    ///
    /// Every bound here is a *graceful* end rather than a discarded run: the
    /// event stream closes the way it always does, and whatever the model
    /// committed before the bound tripped is kept. That is what makes bounding
    /// an unattended run safe to do — the alternative, throwing the work away
    /// for being one round too long, would make callers reluctant to set one.
    pub fn with_deadline(self, deadline: Duration) -> Self {
        Self {
            deadline: Some(deadline),
            ..self
        }
    }

    /// Caps how many tool calls the run may make.
    pub fn with_tool_budget(self, tool_budget: usize) -> Self {
        Self {
            tool_budget: Some(tool_budget),
            ..self
        }
    }

    /// Caps the tokens the run may report using, input plus output.
    ///
    /// Soft: the round that crosses the line is allowed to finish, because
    /// usage is only known once a round has streamed in full.
    pub fn with_token_budget(self, token_budget: u64) -> Self {
        Self {
            token_budget: Some(token_budget),
            ..self
        }
    }

    /// The bounds this config puts on every turn the run performs.
    ///
    /// Limits only. Cancellation and the graceful stop signal are per-call
    /// things a caller holds a token for, not configuration, so they stay at
    /// their defaults here and arrive through
    /// [`send_with_options`](PreparedRun::send_with_options).
    pub fn turn_options(&self) -> TurnOptions {
        TurnOptions {
            deadline: self.deadline,
            tool_budget: self.tool_budget,
            token_budget: self.token_budget,
            ..TurnOptions::default()
        }
    }
}

/// A bound that ended a run before its work did.
///
/// Separate from [`RunOutcome`] because the two answer different questions. A
/// bounded run *failed* in the sense that no final message arrived, and it is
/// reported that way on the stream; but "the model ran out of the time you gave
/// it" and "the provider refused the request" call for different reactions, and
/// a caller — the CLI's exit code, or a script driving many runs — should not
/// have to read an error message to tell them apart (ADR-0015).
///
/// A token budget is deliberately absent: crossing it ends the turn *gracefully*
/// at the next round boundary, so the run finishes with [`RunOutcome::Ok`] and
/// keeps what it committed. There is nothing to distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Bound {
    /// [`RunConfig::with_deadline`] — the run took longer than it was given.
    Deadline,
    /// [`RunConfig::with_tool_budget`] — the run made all the calls it had.
    ToolBudget,
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
    /// Which bound ended the run, when one did rather than the work.
    pub stopped_by: Option<Bound>,
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
    #[cfg(feature = "mcp")]
    Mcp(#[from] McpError),

    #[error("failed to load prompt templates: {0}")]
    Templates(#[from] crate::templates::TemplateError),

    #[error("failed to load hooks: {0}")]
    Hooks(#[from] crate::hooks::HookConfigError),
}

/// Runs one prompt to completion, streaming events into `sink`.
///
/// Consequential calls are approved by [`AllowAll`](crate::AllowAll), which is
/// what a headless run needs: there is nobody to ask, and a question nothing
/// answers is a hang. It asserts nothing about the run being confined — with
/// commands on by default (ADR-0013) an unattended run carries its user's full
/// authority, so an *attended* one is usually better served by
/// [`run_with_approver`], and anything that needs a real boundary gets it from
/// the OS.
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

/// Runs one prompt, putting every consequential call to `approver`.
///
/// The approver is the whole of lan's approval story (ADR-0010):
/// [`DenyAll`](crate::approval::DenyAll) for a run that may change nothing,
/// the binary's terminal prompter for a person at a TTY, or a host's own — one
/// that allows edits and denies the network, or asks a team over Slack. Note
/// the contract it inherits: an approver that cannot answer must deny.
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
    let bounds = config.turn_options();

    let mut session = resolved.runtime.create_session_with_config(
        config.session_name.clone(),
        resolved.model.clone(),
        agent_config(&config, &resolved.context),
    )?;
    apply_effort(&mut session, config.effort)?;

    Ok(PreparedRun::new(
        session,
        resolved.into_context(config.prompt, &config.workspace),
    )
    .with_bounds(bounds))
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
    let bounds = config.turn_options();

    let mut session = resolved.runtime.resume_session(agent_id)?;
    apply_effort(&mut session, config.effort)?;

    Ok(PreparedRun::new(
        session,
        resolved.into_context(config.prompt.clone(), &config.workspace),
    )
    .with_bounds(bounds))
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
    mcp_files: Vec<ContextFile>,
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
        // kernel's job, and per ADR-0013 lan ships no instance of one. What
        // the config says about commands is passed through as written.
        .with_policy(
            git_protected(
                RuntimePolicy::workspace_bounded(&config.workspace),
                &config.workspace,
            )
            .allow_shell_commands(config.shell.is_granted())
            .allow_background_commands(config.shell.is_granted()),
        )
        // Without an authorizer mentra allows every call unconditionally, and
        // no permission request can ever be raised — so the gate goes on even
        // for a run that approves everything (see `crate::approval`).
        .with_tool_authorizer(ApprovalGate::new());

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

    // Both lists reach the header whether or not this build has MCP in it: what
    // a run reports is a schema clients parse, and a field that vanished with a
    // cargo feature would make the stream's shape depend on how lan was built.
    #[cfg(feature = "mcp")]
    let (builder, mcp_files, mcp_servers) = {
        let (files, servers) = discovered_mcp(config)?;
        let names: Vec<String> = servers
            .iter()
            .map(|server| server.name().to_string())
            .collect();
        let builder = servers
            .into_iter()
            .fold(builder, |builder, server| match server {
                mcp::McpServer::Stdio(server) => builder.with_mcp_server(server),
                mcp::McpServer::Sse(server) => builder.with_mcp_sse_server(server),
            });

        (builder, files, names)
    };
    #[cfg(not(feature = "mcp"))]
    let (mcp_files, mcp_servers): (Vec<ContextFile>, Vec<String>) = (Vec::new(), Vec::new());

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

/// Registers the MCP servers this run connects, and reports what took effect.
///
/// Servers are registered on the builder and connected by `build_async`, so
/// this must happen before the build. mentra's `McpRegistration` is private,
/// which is why the fold matches here rather than in [`crate::mcp`].
///
/// Discovery runs for its own sake as well: the header names which files took
/// effect, and an `.mcp.json` is the last thing that should apply invisibly —
/// it says which programs to spawn.
#[cfg(feature = "mcp")]
fn discovered_mcp(config: &RunConfig) -> Result<(Vec<ContextFile>, Vec<mcp::McpServer>), RunError> {
    let files: Vec<ContextFile> = mcp::discover(&config.workspace, &config.mcp)?
        .iter()
        .map(|source| ContextFile {
            path: source.path.clone(),
            scope: source.scope.label(),
        })
        .collect();

    Ok((files, mcp::servers(&config.workspace, &config.mcp)?))
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
    )
    .with_bounds(config.turn_options()))
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
/// bearer auth — so lan takes that definition, swaps the base URL, and disables
/// automatic Hybrid HTTP state chaining. Building on the preset avoids
/// describing a provider from scratch and drifting from whatever mentra learns
/// next.
fn compatible_provider(base_url: &str, api_key: &str) -> ResponsesProvider<StaticCredentialSource> {
    let mut definition = responses::openai_definition();
    definition.base_url = Some(base_url.to_string());
    definition.descriptor.display_name = Some(format!("OpenAI-compatible ({base_url})"));

    // A compatible endpoint promises the Responses wire shape, not every
    // optional OpenAI extension. LAN already replays the complete local
    // transcript, so do not probe `previous_response_id` support with a
    // request that may fail; native provider presets retain Hybrid chaining.
    ResponsesProvider::new(definition, StaticCredentialSource::new(api_key))
        .without_hybrid_http_previous_response_id()
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
/// `None` leaves the session untouched instead of sending a default nobody
/// asked for. Mentra's provider adapter validates the requested level and maps
/// it to that API's wire format.
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
/// remains hygiene; per ADR-0004 and ADR-0013 the boundary is the OS's, and
/// lan does not ship one.
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
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use super::*;

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut header_end = None;
        let mut content_length = 0_usize;

        loop {
            let read = stream.read(&mut buffer).expect("read request");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if header_end.is_none()
                && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let end = index + 4;
                header_end = Some(end);
                let headers = String::from_utf8_lossy(&bytes[..end]);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                    .unwrap_or_default();
            }
            if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
                break;
            }
        }

        String::from_utf8(bytes).expect("request should be utf8")
    }

    fn spawn_two_response_server() -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("read server address");
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 1..=2 {
                let (mut stream, _) = listener.accept().expect("accept request");
                requests.push(read_http_request(&mut stream));
                let response_id = format!("resp_{index}");
                let body = format!(
                    concat!(
                        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{}\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}}}\n\n",
                        "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{}\",\"model\":\"gpt-5\",\"status\":\"completed\"}}}}\n\n"
                    ),
                    response_id, response_id
                );
                let response = format!(
                    concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "connection: close\r\n",
                        "content-type: text/event-stream\r\n",
                        "content-length: {}\r\n\r\n",
                        "{}"
                    ),
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
            requests
        });

        (format!("http://{address}/"), handle)
    }

    #[test]
    fn a_config_carries_no_task_specific_defaults() {
        let config = RunConfig::new("/repo", "do the thing");

        assert_eq!(config.provider, None);
        assert!(matches!(config.model, ModelSelector::NewestAvailable));
        assert_eq!(config.session_name, DEFAULT_SESSION_NAME);
    }

    #[tokio::test]
    async fn compatible_provider_skips_automatic_previous_response_id_chaining() {
        let (base_url, handle) = spawn_two_response_server();
        let provider = compatible_provider(&base_url, "test-key");

        for (index, message) in ["first", "second"].into_iter().enumerate() {
            let request = mentra::provider_core::Request {
                model: Cow::Borrowed("gpt-5"),
                system: None,
                messages: Cow::Owned(vec![mentra::Message::user(mentra::ContentBlock::text(
                    message,
                ))]),
                tools: Cow::Owned(Vec::new()),
                tool_choice: None,
                temperature: None,
                max_output_tokens: None,
                metadata: Cow::Owned(BTreeMap::new()),
                provider_request_options: Default::default(),
            };
            let mut stream = provider
                .session()
                .stream_response(request)
                .await
                .expect("compatible provider should stream");
            while let Some(event) = stream.recv().await {
                event.expect("response event should decode");
            }
            if index == 0 {
                assert_eq!(
                    provider.session().latest_response_id().as_deref(),
                    Some("resp_1"),
                    "the second request must have provider state available to suppress"
                );
            }
        }

        let requests = handle.join().expect("server should capture requests");
        for request in requests {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            let payload: serde_json::Value =
                serde_json::from_str(body).expect("request body should be json");
            assert!(payload.get("previous_response_id").is_none());
        }
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
    fn commands_are_available_unless_the_caller_says_otherwise() {
        let config = RunConfig::new("/repo", "prompt");

        assert_eq!(config.shell, ShellAccess::Granted);
        assert!(config.shell.is_granted());
    }

    #[test]
    fn denying_shell_returns_a_new_config() {
        let base = RunConfig::new("/repo", "prompt");
        let denied = base.clone().with_shell(ShellAccess::Denied);

        assert_eq!(
            base.shell,
            ShellAccess::Granted,
            "the original is untouched"
        );
        assert_eq!(denied.shell, ShellAccess::Denied);
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

    #[test]
    fn every_lan_effort_maps_to_the_same_provider_level() {
        for (effort, expected) in [
            (Effort::Low, ReasoningEffort::Low),
            (Effort::Medium, ReasoningEffort::Medium),
            (Effort::High, ReasoningEffort::High),
            (Effort::XHigh, ReasoningEffort::XHigh),
            (Effort::Max, ReasoningEffort::Max),
        ] {
            assert_eq!(ReasoningEffort::from(effort), expected);
        }
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

    #[test]
    fn a_run_is_unbounded_unless_the_caller_asks_for_a_bound() {
        let options = RunConfig::new("/repo", "prompt").turn_options();

        // ADR-0014: with no scheduler shipped there is no period to default a
        // deadline from, so bounding is explicit everywhere. An attended run
        // has a person, and a timer that interrupted someone mid-thought would
        // be a worse harness rather than a safer one.
        assert_eq!(options.deadline, None);
        assert_eq!(options.tool_budget, None);
        assert_eq!(options.token_budget, None);
    }

    #[test]
    fn every_bound_reaches_the_turn_as_configured() {
        let options = RunConfig::new("/repo", "prompt")
            .with_deadline(Duration::from_secs(3_600))
            .with_tool_budget(12)
            .with_token_budget(50_000)
            .turn_options();

        assert_eq!(options.deadline, Some(Duration::from_secs(3_600)));
        assert_eq!(options.tool_budget, Some(12));
        assert_eq!(options.token_budget, Some(50_000));
    }

    #[test]
    fn bounding_a_config_returns_a_new_value() {
        let base = RunConfig::new("/repo", "prompt");
        let bounded = base.clone().with_deadline(Duration::from_secs(600));

        assert_eq!(base.deadline, None, "the original must be untouched");
        assert_eq!(bounded.deadline, Some(Duration::from_secs(600)));
    }

    #[test]
    fn a_config_carries_no_stop_signal_of_its_own() {
        // Cancellation belongs to whoever holds the token for one call, so a
        // config that could carry one would be handing every turn built from
        // it the same stop button.
        let options = RunConfig::new("/repo", "prompt")
            .with_deadline(Duration::from_secs(60))
            .turn_options();

        assert!(options.cancel.is_none());
        assert!(options.stop.is_none());
    }
}
