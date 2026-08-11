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
//! the prompt — for the case where one prompt is the whole job. Everything in
//! this module is a wrapper around [`Workspace`]: it opens
//! one, mints a single run from it, and drops it when the run ends. A host
//! sending more than one prompt at a repository should open the workspace
//! itself and keep it, which is what the split of ADR-0010 is for.
//!
//! A host that already owns a mentra runtime skips to [`prepare_with_session`]
//! and keeps its own.

mod output;
mod prepared;
mod sink;
mod turn;
mod usage;

use std::{path::PathBuf, time::Duration};

use mentra::{BuiltinProvider, ModelSelector, Session};
use thiserror::Error;

#[cfg(feature = "mcp")]
use crate::mcp::{McpConfig, McpError};
use crate::{
    approval::Approver,
    context::{ContextConfig, ContextError, WorkspaceContext},
    event::RunOutcome,
    hooks::HooksConfig,
    provider::ProviderError,
    shell::ShellAccess,
    skills::SkillsConfig,
    templates::TemplatesConfig,
    workspace::{
        DEFAULT_SESSION_NAME, RunSpec, Workspace, WorkspaceBuilder, load_templates,
        resolved_workspace,
    },
};

pub use output::{OutputReport, OutputSpec};
pub use prepared::{LoadedSkill, PreparedRun, RunContext};
pub use sink::{
    CollectingSink, EventFanIn, EventSink, FnSink, MergedEvents, NullSink, TaggedEvent, TaggedSink,
};
pub use turn::TurnOptions;
pub use usage::RunUsage;

/// The signal a caller trips to stop a turn.
///
/// Re-exported rather than restated, and the reason is the one thing lan cannot
/// wrap: a token is an *identity*, not a value. The turn holds one half and the
/// caller the other, and a lan-owned copy would have to forward the trip to
/// mentra's — a second object that can disagree with the first about whether
/// the stop button was pressed. So this is a deliberate leak, like
/// [`ModelSelector`] and [`BuiltinProvider`] on [`RunConfig`].
///
/// Re-exporting it is what makes the leak cheap. A host embedding `lan-core`
/// should not have to add mentra to its own manifest — and pin the same
/// version — to name a type lan's own API asks it for; a skew there fails to
/// compile with no hint that two crates disagree about one struct. Hence the
/// rule: every mentra type lan's surface makes a caller *name*, lan re-exports.
///
/// Two of them go on a turn and they mean different things —
/// [`TurnOptions::cancel`] abandons it, [`TurnOptions::stop`] ends it
/// gracefully.
pub use mentra::runtime::CancellationToken;

/// How hard the model should think before answering.
///
/// lan's own enum rather than a re-export, for the reason [`Event`](crate::Event) and
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

impl From<Effort> for mentra::provider::ReasoningEffort {
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
///
/// This conflates two lifetimes, and [`split`](Self::split) is where the seam
/// is: most of it describes a *workspace* — where to discover context, which
/// provider answers, which MCP servers to connect — and only a handful of
/// fields describe one run. A caller sending many prompts at one repository
/// wants [`Workspace`] and a [`RunSpec`] each; a caller
/// sending one wants this, and pays for the discovery once either way.
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
    /// usage is only known once a round has streamed in full. The run ends at
    /// that boundary keeping everything it committed, and says so —
    /// [`Bound::TokenBudget`] on the report, exit `3` from the CLI — whether or
    /// not the work it kept amounts to an answer.
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
        self.spec().turn_options()
    }

    /// The two halves this config conflates: what belongs to the workspace, and
    /// what belongs to one run of it.
    ///
    /// Every function in this module is `config.split()` followed by an
    /// `open().await` and a mint, so this is not a second description of the
    /// mapping — it *is* the mapping, and it is public because it is also the
    /// migration path. A caller that outgrows one-prompt-per-config keeps the
    /// builder, opens it once, and mints a [`RunSpec`] per run.
    pub fn split(&self) -> (WorkspaceBuilder, RunSpec) {
        let mut builder = Workspace::builder(&self.workspace)
            .with_model(self.model.clone())
            .with_context(self.context.clone())
            .with_skills(self.skills.clone())
            .with_templates(self.templates.clone())
            .with_hooks(self.hooks.clone())
            .with_shell(self.shell);

        #[cfg(feature = "mcp")]
        {
            builder = builder.with_mcp(self.mcp.clone());
        }
        if let Some(provider) = self.provider {
            builder = builder.with_provider(provider);
        }
        if let Some(base_url) = &self.base_url {
            builder = builder.with_base_url(base_url.clone());
        }

        (builder, self.spec())
    }

    /// The per-run half alone, for the callers that need no runtime.
    fn spec(&self) -> RunSpec {
        RunSpec {
            prompt: self.prompt.clone(),
            session_name: self.session_name.clone(),
            effort: self.effort,
            deadline: self.deadline,
            tool_budget: self.tool_budget,
            token_budget: self.token_budget,
            // A one-prompt run has no siblings to share an allowance with, so
            // there is nothing for a pool to do here. A caller who wants one
            // wants the `Workspace` shape (ADR-0010), where a pool is attached
            // per `RunSpec`.
            budget: None,
        }
    }
}

/// A bound that ended a run before its work did.
///
/// Separate from [`RunOutcome`] because the two answer different questions.
/// "The model ran out of the time you gave it" and "the provider refused the
/// request" call for different reactions, and a caller — the CLI's exit code,
/// or a script driving many runs — should not have to read an error message to
/// tell them apart (ADR-0015). The outcome says whether an answer arrived; this
/// says whether an allowance is what ended the run, and the two are
/// independent. [`Deadline`](Self::Deadline) and [`ToolBudget`](Self::ToolBudget)
/// always arrive alongside [`RunOutcome::Error`], because no final message
/// does; [`TokenBudget`](Self::TokenBudget) can arrive on a run that answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Bound {
    /// [`RunConfig::with_deadline`] — the run took longer than it was given.
    Deadline,
    /// [`RunConfig::with_tool_budget`] — the run made all the calls it had.
    ToolBudget,
    /// [`RunConfig::with_token_budget`], or a [`BudgetPool`](crate::BudgetPool)
    /// the run drew dry — it spent its allowance and was refused another round.
    ///
    /// The odd one out, and worth knowing why before branching on it. This
    /// bound is *graceful*: mentra ends the run at a round boundary keeping
    /// everything committed so far, exactly as if the model had finished. So a
    /// run can report [`RunOutcome::Ok`] with an ordinary answer *and* this
    /// bound, which is the honest description of "you got an answer, and the
    /// allowance is why there is not more of one". Whether an answer arrives
    /// comes down to what the last committed message was: prose, and the turn
    /// succeeds; a tool result, and it fails owing a final message it never
    /// got.
    ///
    /// Reportable at all only because mentra records the decision at the
    /// boundary it makes it
    /// ([`RunOptions::ended_early`](mentra::runtime::RunOptions::ended_early)).
    /// Comparing usage against the budget afterwards would answer a different
    /// question — what is true now, rather than what the runner decided on —
    /// and a pooled run can cross the line without being the run that was
    /// stopped by it.
    TokenBudget,
}

/// What a completed run produced, alongside the sink it wrote to.
#[derive(Debug)]
pub struct RunReport<S> {
    pub session_id: String,
    pub model: String,
    pub provider: String,
    /// The assistant's final message, absent when the run failed — and absent
    /// on a typed turn, where the answer is
    /// [`OutputReport::value`] rather than prose.
    pub final_message: Option<String>,
    pub outcome: RunOutcome,
    /// Which bound ended the run, when one did rather than the work.
    ///
    /// Neither field implies the other: a bounded run usually failed for want
    /// of a final message, but see [`Bound::TokenBudget`], which a run that
    /// answered can carry.
    pub stopped_by: Option<Bound>,
    /// What the run reported spending. Present whether it succeeded or not: a
    /// turn that failed on its fourth round still spent the first three.
    pub usage: RunUsage,
    pub sink: S,
}

impl<S> RunReport<S> {
    pub fn succeeded(&self) -> bool {
        matches!(self.outcome, RunOutcome::Ok)
    }
}

/// Anything that can go wrong opening a workspace, preparing a run, or driving
/// one.
///
/// One error type across all three, rather than a `WorkspaceError` beside it:
/// opening a workspace exists to prepare runs, and every failure listed here is
/// a failure a caller of [`run`] has always been able to receive.
#[derive(Debug, Error)]
pub enum RunError {
    #[error("prompt is empty")]
    EmptyPrompt,

    /// The shared allowance this turn draws on has nothing left.
    ///
    /// A decision rather than a failure of the work, which is why it is its own
    /// variant: a caller fanning out over a [`BudgetPool`](crate::BudgetPool)
    /// stops minting on this, where it would retry on a provider error. Raised
    /// before the prompt is sent and before the stream opens, so the
    /// conversation is left exactly as it was.
    #[error("the shared token budget is spent: {spent} of {limit} tokens reported")]
    BudgetExhausted { limit: u64, spent: u64 },

    #[error("no session to resume")]
    NoSuchSession,

    #[error(transparent)]
    Context(#[from] ContextError),

    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error("runtime error: {0}")]
    Runtime(#[from] mentra::error::RuntimeError),

    /// A typed turn answered, but not in the shape that was asked for.
    ///
    /// Separate from [`Runtime`](Self::Runtime) because the two call for
    /// different reactions and lan can tell them apart honestly: this one is
    /// lan's own verdict. The typed path asks mentra for the raw payload and
    /// deserializes it here, so a value that does not fit `T` is a schema or
    /// prompt problem — retry with a clearer schema — while a provider failure
    /// is not. The exchange stays in the session's transcript either way; see
    /// [`PreparedRun::output`].
    #[error("the run's output did not match the requested type: {0}")]
    OutputMismatch(#[source] serde_json::Error),

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
/// The session is dropped when this returns, and so is the workspace opened to
/// hold it. For a conversation, keep the [`PreparedRun`] from [`prepare`] and
/// call [`send`](PreparedRun::send) on it; for many conversations, keep a
/// [`Workspace`].
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
///
/// One prompt, one workspace, opened and dropped around it.
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
    let (workspace, spec) = config.split();

    workspace.open().await?.prepare(spec)
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
    let (workspace, spec) = config.split();

    workspace.open().await?.resume(agent_id, spec)
}

/// Prepares a run against a session the caller already built, so a host with
/// its own runtime — custom tools, its own store, a provider lan does not
/// know — still gets lan's context discovery and event stream.
///
/// The prompt in `config` may be empty. Once a session outlives a turn, a
/// conversation with nothing said yet is a real state — it is what ACP's
/// `session/new` opens — so the check belongs where a prompt is actually sent,
/// which is [`PreparedRun::execute`] and [`PreparedRun::send`].
///
/// This is the one path that does not go through
/// [`Workspace`], because there is no runtime for lan to
/// build: the caller brought one. It still discovers what it can without
/// touching that runtime.
pub fn prepare_with_session(
    session: Session,
    config: &RunConfig,
    provider: impl Into<String>,
    model: impl Into<String>,
) -> Result<PreparedRun, RunError> {
    let context = WorkspaceContext::discover_with(&config.workspace, &config.context)?;
    // Unlike skills, templates are registered on nothing — so lan can discover
    // them here without touching a runtime it does not own.
    let (templates_dirs, templates) = load_templates(&config.workspace, &config.templates)?;

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
        use mentra::provider::ReasoningEffort;

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

    #[test]
    fn splitting_a_config_keeps_every_per_run_field() {
        let (_, spec) = RunConfig::new("/repo", "prompt")
            .with_session_name("named")
            .with_effort(Effort::Max)
            .with_deadline(Duration::from_secs(90))
            .with_tool_budget(7)
            .with_token_budget(1_000)
            .split();

        assert_eq!(spec.prompt, "prompt");
        assert_eq!(spec.session_name, "named");
        assert_eq!(spec.effort, Some(Effort::Max));
        assert_eq!(spec.deadline, Some(Duration::from_secs(90)));
        assert_eq!(spec.tool_budget, Some(7));
        assert_eq!(spec.token_budget, Some(1_000));
    }

    #[tokio::test]
    async fn splitting_a_config_keeps_every_workspace_field() {
        // Checked by opening the builder rather than by reading fields, because
        // the fields are private and because what matters is that the opened
        // workspace behaves as the config asked. A missing workspace fails the
        // same way through both paths — which it can only do if `split` carried
        // the path and the context config across.
        let config = RunConfig::new("/definitely/not/a/real/path", "hello")
            .with_provider(BuiltinProvider::Anthropic)
            .with_base_url("http://127.0.0.1:1/v1");
        let (builder, _) = config.split();

        assert!(matches!(
            builder.open().await.expect_err("rejected"),
            RunError::Context(ContextError::WorkspaceMissing { .. })
        ));
    }
}
