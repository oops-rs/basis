//! One prompt against one workspace: the smallest complete thing basis does.
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

mod bounds;
mod output;
mod prepared;
mod sink;
mod turn;
mod usage;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mentra::Session;

use crate::{
    approval::{AllowAll, Approver},
    context::{ContextConfig, WorkspaceContext},
    event::RunOutcome,
    templates::TemplatesConfig,
    workspace::{DEFAULT_SESSION_NAME, RunSpec, Workspace, WorkspaceBuilder, load_templates},
};

pub use bounds::Bounds;
pub use output::{
    OutputAttempt, OutputAttemptReport, OutputDecision, OutputFailure, OutputReport,
    OutputReservation, OutputSpec,
};
pub use prepared::{
    AgentEventTapGuard, Compacted, LoadedSkill, PreparedRun, PromptPart, RunContext,
};
pub use sink::{
    CollectingSink, EventFanIn, EventSink, FnSink, MergedEvents, NullSink, TaggedEvent, TaggedSink,
};
pub use turn::TurnOptions;
pub use usage::RunUsage;

/// Recoverability classification for a retained [`RunFailure`].
pub use mentra::error::ErrorCategory as RunFailureCategory;
/// Mentra's original typed terminal failure retained by [`RunReport`].
pub use mentra::error::RuntimeError as RunFailure;

/// The signal a caller trips to stop a turn.
///
/// Re-exported rather than restated, and the reason is the one thing basis cannot
/// wrap: a token is an *identity*, not a value. The turn holds one half and the
/// caller the other, and a basis-owned copy would have to forward the trip to
/// mentra's — a second object that can disagree with the first about whether
/// the stop button was pressed. So this is a deliberate leak, like
/// [`ModelSelector`](mentra::ModelSelector) and
/// [`BuiltinProvider`](mentra::BuiltinProvider) on
/// [`RuntimeBuilder`](crate::RuntimeBuilder).
///
/// Re-exporting it is what makes the leak cheap. A host embedding `basis`
/// should not have to add mentra to its own manifest — and pin the same
/// version — to name a type basis's own API asks it for; a skew there fails to
/// compile with no hint that two crates disagree about one struct. Hence the
/// rule: every mentra type basis's surface makes a caller *name*, basis re-exports.
///
/// Two of them go on a turn and they mean different things —
/// [`TurnOptions::cancel`] abandons it, [`TurnOptions::stop`] ends it
/// gracefully.
pub use mentra::runtime::CancellationToken;

/// How much provider-generated reasoning summary to request.
///
/// Re-exported because [`ReasoningOptions::summary`] makes a caller name it;
/// like the other complete reasoning types below, a Basis caller should not
/// need a separately version-pinned Mentra dependency to construct it.
pub use mentra::provider_core::ReasoningSummary;

/// The per-turn steering seam, whole: the trait a host implements and every
/// type its signatures make that host *name*.
///
/// Re-exported rather than wrapped, under the rule stated on
/// [`CancellationToken`] — a mentra type basis's surface asks a caller to
/// write is a name basis re-exports, or the caller pins mentra in their own
/// manifest to spell it. And like the token, a strategy is an *identity*
/// rather than a value: [`TurnOptions::with_round_strategy`] hands mentra the
/// caller's own `Arc`, so a basis-owned mirror of [`RoundDecision`] would be a
/// translation layer with opinions of its own — the opposite of a seam.
///
/// The set is exactly what implementing costs. [`RoundStrategy`] is the trait
/// (spelled with [`async_trait`](crate::async_trait), which basis already
/// re-exports); [`RoundContext`] and [`RoundBoundary`] are its question;
/// [`RoundDecision`], [`RoundAdjustment`], [`ReasoningChange`] and
/// [`RoundToolResult`] are its vocabulary for answering. The last three are
/// here because the vocabulary's own signatures demand them:
/// [`RoundDecision::inject`] takes [`ContentBlock`]s,
/// [`RoundAdjustment::with_model`] a [`ModelInfo`], and
/// [`ReasoningChange::Set`] a [`ReasoningOptions`] — whose `effort` field an
/// [`Effort`] converts into, so switching effort mid-run never names mentra's
/// own level enum.
pub use mentra::{
    ContentBlock, ModelInfo, ReasoningChange, ReasoningOptions, RoundAdjustment, RoundBoundary,
    RoundContext, RoundDecision, RoundStrategy, RoundToolResult,
};

/// Mentra's complete provider-neutral agent event, forwarded unchanged by
/// [`PreparedRun::register_agent_event_tap`].
///
/// Re-exported under the same rule as [`CancellationToken`]: this public
/// callback asks a Basis host to name the type, so the host should not need a
/// separately version-pinned Mentra dependency just to spell its signature.
pub use mentra::agent::AgentEvent;

/// How hard the model should think before answering.
///
/// basis's own enum rather than a re-export, for the reason [`Event`](crate::Event) and
/// [`TurnOptions`] are: the surface basis promises should not move when mentra's
/// does. Provider adapters translate this semantic level to their own wire
/// format. A provider or model that does not offer the requested level returns
/// an error rather than silently lowering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// The level a session is set to, read back as basis names it.
///
/// The direction [`PreparedRun::effort`](crate::PreparedRun::effort) needs, and
/// it is a `TryFrom` because mentra's enum is `#[non_exhaustive]` too: a level
/// added upstream that basis has no name for is not a level basis can report,
/// and answering `Low` — or `None`, which means *no level requested* — would
/// both be claims about a session that are simply untrue. The error carries the
/// mentra value so a caller that wants to render something can.
impl TryFrom<mentra::provider::ReasoningEffort> for Effort {
    type Error = mentra::provider::ReasoningEffort;

    fn try_from(effort: mentra::provider::ReasoningEffort) -> Result<Self, Self::Error> {
        use mentra::provider::ReasoningEffort as Upstream;

        match effort {
            Upstream::Low => Ok(Self::Low),
            Upstream::Medium => Ok(Self::Medium),
            Upstream::High => Ok(Self::High),
            Upstream::XHigh => Ok(Self::XHigh),
            Upstream::Max => Ok(Self::Max),
            unknown => Err(unknown),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Bound {
    /// [`RunSpec::with_deadline`] — the run took longer than it was given.
    Deadline,
    /// [`RunSpec::with_tool_budget`] — the run made all the calls it had.
    ToolBudget,
    /// [`RunSpec::with_token_budget`], or a [`BudgetPool`](crate::BudgetPool)
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
    /// Original typed runtime failure, before [`RunOutcome`] display/wire
    /// projection. `None` on success and on Basis-owned output-shape mismatch.
    ///
    /// Also `None` on a failed typed turn ([`PreparedRun::output`]), where the
    /// same error is the [`OutputFailure::error`] the report arrives beside: a
    /// `RunFailure` is not `Clone` and cannot be in both places, and the error
    /// is the half a caller reaching for `?` gets. The validated path
    /// ([`PreparedRun::output_parts_validated_with_options`]) returns `Ok`, so
    /// there this field is the only home and keeps it (ADR-0024 §4).
    pub failure: Option<RunFailure>,
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

pub use crate::error::RunError;

/// Runs one prompt to completion, streaming events into `sink`.
///
/// Consequential calls are approved by [`AllowAll`], which is
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
/// hold it. For a conversation, open the [`Workspace`] yourself and keep the
/// run it mints — [`send`](PreparedRun::send) is what makes it one; for many
/// conversations, keep the workspace. Anything beyond a path and a prompt —
/// a model, an endpoint, a bound — is the same shape one call earlier:
/// `Workspace::builder(path)` and a [`RunSpec`].
pub async fn run<S: EventSink>(
    workspace: impl Into<PathBuf>,
    prompt: impl Into<String>,
    sink: S,
) -> Result<RunReport<S>, RunError> {
    run_with_approver(workspace, prompt, sink, AllowAll).await
}

/// Runs one prompt, putting every consequential call to `approver`.
///
/// The approver is the whole of basis's approval story (ADR-0010):
/// [`DenyAll`](crate::approval::DenyAll) for a run that may change nothing,
/// the binary's terminal prompter for a person at a TTY, or a host's own — one
/// that allows edits and denies the network, or asks a team over Slack. Note
/// the contract it inherits: an approver that cannot answer must deny.
pub async fn run_with_approver<S: EventSink, A: Approver>(
    workspace: impl Into<PathBuf>,
    prompt: impl Into<String>,
    sink: S,
    approver: A,
) -> Result<RunReport<S>, RunError> {
    let prompt = prompt.into();
    if prompt.trim().is_empty() {
        return Err(RunError::EmptyPrompt);
    }

    let mut run = mint_carrying_workspace(Workspace::builder(workspace.into()), |workspace| {
        workspace.prepare(prompt)
    })
    .await?;

    run.execute_with_approver(sink, approver).await
}

/// Opens the builder and mints one run that carries the workspace.
///
/// The one resolution path for every free function above, and the load-bearing
/// half is the carry: these functions hand back a [`PreparedRun`] and nothing
/// else, so the run must be what keeps the workspace alive until the run ends
/// — the module's own promise. A workspace dropped when this returns would
/// take its hook registration and MCP connections with it *before the first
/// turn is driven*: the dispatcher fails open for a directory no live
/// workspace claims, so every `.basis/hooks.json` hook would be silently
/// bypassed, and the minted roster would offer `mcp__*` tools whose servers
/// were already torn down. See [`PreparedRun::with_workspace`].
async fn mint_carrying_workspace(
    builder: WorkspaceBuilder,
    mint: impl FnOnce(&Workspace) -> Result<PreparedRun, RunError>,
) -> Result<PreparedRun, RunError> {
    let workspace = Arc::new(builder.open().await?);
    let prepared = mint(&workspace)?;

    Ok(prepared.with_workspace(workspace))
}

/// Prepares a run against a session the caller already built, so a host with
/// its own runtime — custom tools, its own store, a provider basis does not
/// know — still gets basis's context discovery and event stream.
///
/// The inputs are exactly what this path can honor, and every field of `spec`
/// is honored. The prompt may be empty, because once a session outlives a
/// turn a conversation with nothing said yet is a real state — it is what
/// ACP's `session/new` opens — so the check belongs where a prompt is
/// actually sent, which is [`PreparedRun::execute`] and [`PreparedRun::send`].
/// The bounds become the run's configured limits; an `effort` is applied to
/// the session in hand; a `session_name` renames it — unless it is the
/// default, which is left alone, because the caller named the session at mint
/// and asking for the default and saying nothing are the same request.
/// `context` says where discovery looks; templates are discovered with the
/// default configuration, there being no caller that has ever wanted
/// otherwise on this path.
///
/// This is the one path that does not go through
/// [`Workspace`], because there is no runtime for basis to
/// build: the caller brought one. It still discovers what it can without
/// touching that runtime — skills and MCP registration stay the caller's, who
/// owns the runtime they would register on.
///
/// **`#[doc(hidden)]`, not private.** This existed because `Workspace` could
/// not yet be handed a runtime, a provider, a tool roster, or a round
/// strategy — every knob a bring-your-own-runtime caller actually wanted was
/// missing from the assembler, so bypassing it was the only way to get one.
/// The assembler is open now: [`WorkspaceBuilder::with_runtime`] and
/// [`with_runtime_builder`](WorkspaceBuilder::with_runtime_builder) accept a
/// caller's own [`Runtime`](crate::Runtime) or recipe (ADR-0018, including a
/// [`RuntimeBuilder::with_provider_instance`](crate::RuntimeBuilder::with_provider_instance)
/// the host constructed itself), [`WorkspaceBuilder::with_tool_roster`] states
/// the model's roster (D3), and [`TurnOptions::with_round_strategy`] reaches a
/// live turn. Every caller with a real host to write against should reach for
/// those instead — the discovery this function does by hand
/// (`WorkspaceContext::discover_with`, default-only templates, no skills, no
/// MCP) is exactly what `WorkspaceBuilder::open` already does more completely,
/// once a workspace exists to open. What remains behind this name is the test
/// harness this crate's own suite drives sessions through — `basis-cli`'s
/// bridge tests among them — where a scripted [`Session`] needs basis's event
/// stream and context discovery without a real runtime underneath it, and the
/// signature this wave found already fits every caller that still needs it.
#[doc(hidden)]
pub fn prepare_with_session(
    session: Session,
    workspace: &Path,
    spec: impl Into<RunSpec>,
    context: &ContextConfig,
    provider: impl Into<String>,
    model: impl Into<String>,
) -> Result<PreparedRun, RunError> {
    let spec = spec.into();
    let mut session = session;
    if spec.session_name != DEFAULT_SESSION_NAME {
        session.set_name(spec.session_name.clone())?;
    }

    // The one resolution, for the same reason
    // [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open) has one: this
    // is the other path that reports a workspace, and everything below names
    // it through this value. Resolving inside discovery and leaving the
    // caller's spelling to the rest would put a header's `workspace` and its
    // `templates_dirs` on two different directories for any relative or
    // symlinked path — the same split, in the one place it survived.
    let workspace = crate::context::resolve_workspace(workspace)?;

    let discovered = WorkspaceContext::discover_with(&workspace, context)?;
    // Unlike skills, templates are registered on nothing — so basis can discover
    // them here without touching a runtime it does not own.
    let (templates_dirs, templates) = load_templates(&workspace, &TemplatesConfig::default())?;

    let mut prepared = PreparedRun::new(
        session,
        RunContext {
            workspace,
            prompt: spec.prompt.clone(),
            provider: provider.into(),
            model: model.into(),
            context: discovered,
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
    .with_bounds(spec.turn_options());

    if let Some(effort) = spec.effort {
        prepared.set_effort(Some(effort))?;
    }

    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextError;

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
        let error = run("/definitely/not/a/real/path", "   \n ", NullSink)
            .await
            .expect_err("rejected");

        // Reaching provider resolution or workspace validation would prove the
        // check ran too late.
        assert!(matches!(error, RunError::EmptyPrompt));
    }

    #[tokio::test]
    async fn a_missing_workspace_fails_before_a_provider_is_needed() {
        let error = run("/definitely/not/a/real/path", "hello", NullSink)
            .await
            .expect_err("rejected");

        assert!(matches!(
            error,
            RunError::Context(ContextError::WorkspaceMissing { .. })
        ));
    }
}
