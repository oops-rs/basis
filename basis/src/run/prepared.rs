//! Driving a session that is already built.
//!
//! Splitting this from [`run`](super::run) gives two things. A host that
//! already owns a mentra [`Runtime`](mentra::Runtime) — with its own provider,
//! store, or custom tools — can still use basis's context discovery and event
//! stream instead of reimplementing them. And basis's own tests can drive the
//! whole pipeline against a scripted runtime, so the event contract is checked
//! without a network call.

use std::{path::PathBuf, sync::Arc};

use mentra::{
    ContentBlock, Session,
    runtime::{EarlyEnd, ProviderRetry, RunOptions},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::oneshot;

use self::forward::forward_events;
use super::{
    Bound, Effort, EventSink, OutputReport, OutputSpec, RunError, RunReport, RunUsage, TurnOptions,
    turn::{bounded, drawable},
};
use crate::{
    approval::{AllowAll, Approver, SideEffectLevels},
    context::WorkspaceContext,
    event::{ContextFile, EVENT_SCHEMA_VERSION, Event, RunOutcome, SkillSummary, TemplateSummary},
    lifecycle::{LifecycleError, Supervisor, TaskHandle},
    templates::Template,
    workspace::Workspace,
};

mod forward;

/// What a run is about, once the runtime questions are settled.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub workspace: PathBuf,
    pub prompt: String,
    pub provider: String,
    pub model: String,
    pub context: WorkspaceContext,
    /// Skills directories registered on the runtime, most specific first.
    pub skills_dirs: Vec<PathBuf>,
    /// The skills those directories actually produced, after layering.
    pub skills: Vec<LoadedSkill>,
    /// Template directories that exist, most specific first.
    pub templates_dirs: Vec<PathBuf>,
    /// The templates those directories produced, after layering, name-ordered.
    /// Over ACP these become the client's commands, mapped by `basis-acp`.
    pub templates: Vec<Template>,
    /// MCP configuration files in effect, weakest precedence first.
    pub mcp_files: Vec<ContextFile>,
    /// The servers those files produced, after layering. Names only: the
    /// header must not echo a command or a credential.
    pub mcp_servers: Vec<String>,
}

/// A skill available to the run, without its body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// A session and the prompt to send it. Nothing has been sent yet.
pub struct PreparedRun {
    session: Session,
    run: RunContext,
    bounds: TurnOptions,
    /// The workspace that minted this run, when the run is what keeps it
    /// alive ([`with_workspace`](Self::with_workspace)); `None` for a caller
    /// that holds the workspace itself.
    ///
    /// Held for its `Drop`, not read: a [`Workspace`] carries its hook
    /// registration and MCP connections, and both end the moment the last
    /// handle to it goes.
    workspace: Option<Arc<Workspace>>,
    /// The reading end of the runtime's side channel; see
    /// [`with_side_effect_levels`](Self::with_side_effect_levels). Empty for a
    /// run whose caller never wired one, which costs the run nothing but the
    /// level on each [`ApprovalRequest`](crate::ApprovalRequest).
    levels: SideEffectLevels,
    /// How patiently this run's turns wait out a failing provider, copied from
    /// the [`Runtime`](crate::Runtime) that minted it.
    ///
    /// A per-run field for a runtime-scoped knob because that is the shape
    /// mentra offers: the schedule rides on `RunOptions`, so a runtime's
    /// answer has to be carried to each run rather than set once upstream.
    /// [`Workspace`](crate::Workspace) puts it here at mint; a caller on the
    /// [`prepare_with_session`](super::prepare_with_session) path built the
    /// mentra runtime itself and gets mentra's default, which is the only
    /// honest answer when basis was never told about a provider connection.
    provider_retry: ProviderRetry,
    /// How many attempts that schedule gets, carried from the same runtime and
    /// for the same reason. mentra splits the count from the waits; basis
    /// keeps them together, because a host set them as one policy.
    retry_budget: usize,
    /// What [`set_effort`](PreparedRun::set_effort) last asked for, and only
    /// that — see [`effort`](PreparedRun::effort) for why it is not the same
    /// question as "what level is the session at".
    effort: Option<Effort>,
}

/// Hand-written because mentra's `Session` is not `Debug`, and because the
/// context documents hold whole files — a derived impl would dump them.
impl std::fmt::Debug for PreparedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRun")
            .field("session_id", &self.session.id().to_string())
            .field("workspace", &self.run.workspace)
            .field("provider", &self.run.provider)
            .field("model", &self.run.model)
            .field("context_files", &self.run.context.documents().len())
            .field("skills", &self.run.skills.len())
            .field("templates", &self.run.templates.len())
            .field("mcp_servers", &self.run.mcp_servers.len())
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

impl PreparedRun {
    pub fn new(session: Session, run: RunContext) -> Self {
        Self {
            session,
            run,
            bounds: TurnOptions::default(),
            workspace: None,
            levels: SideEffectLevels::new(),
            provider_retry: ProviderRetry::default(),
            retry_budget: RunOptions::default().retry_budget,
            effort: None,
        }
    }

    /// Carries the minting runtime's provider retry schedule onto this run.
    ///
    /// Set by [`Workspace`](crate::Workspace) at mint, at the one place both
    /// `prepare` and `resume` go through, so two runs from one runtime cannot
    /// disagree about how patient they are.
    pub(crate) fn with_provider_retry(
        self,
        (provider_retry, retry_budget): (ProviderRetry, usize),
    ) -> Self {
        Self {
            provider_retry,
            retry_budget,
            ..self
        }
    }

    /// The mentra options one turn runs on: what the caller asked for, filled
    /// in from this run's bounds by the caller, plus the runtime's retry
    /// schedule.
    ///
    /// The one place basis composes a [`RunOptions`], so the untyped and typed
    /// turns below cannot drift about what a turn carries — and so a test can
    /// read the same value a turn is about to be driven on.
    pub(crate) fn run_options(&self, options: TurnOptions) -> RunOptions {
        options.into_run_options(self.provider_retry, self.retry_budget)
    }

    /// Sets what every turn on this run may spend.
    ///
    /// [`prepare`](super::prepare) installs [`RunConfig`](super::RunConfig)'s
    /// bounds here; a host that built its own session says so itself. Only the
    /// limits are read — a cancellation token belongs to one call, not to the
    /// run, and arrives through [`send_with_options`](Self::send_with_options).
    pub fn with_bounds(self, bounds: TurnOptions) -> Self {
        Self { bounds, ..self }
    }

    /// What every turn on this run may spend.
    pub const fn bounds(&self) -> &TurnOptions {
        &self.bounds
    }

    /// Makes this run the keeper of the workspace that minted it.
    ///
    /// A `PreparedRun` owns its session but only *describes* its workspace,
    /// and two things live exactly as long as the workspace does: its hook
    /// registration on the runtime's dispatcher, and its MCP connections. A
    /// caller that drops the workspace at mint and drives the run afterwards
    /// runs every turn with the workspace's hooks silently unenforced — the
    /// dispatcher fails open for a directory no live workspace claims, which
    /// is correct for a retired workspace and catastrophic for one that was
    /// merely dropped early — and with its MCP servers torn down while the
    /// minted roster still offers their tools. The free functions in
    /// [`run`](mod@crate::run) attach the workspace here for exactly that
    /// reason; a host that keeps the workspace itself needs nothing from this.
    pub fn with_workspace(self, workspace: Arc<Workspace>) -> Self {
        Self {
            workspace: Some(workspace),
            ..self
        }
    }

    /// The workspace this run keeps alive, when it is the one keeping it.
    ///
    /// `None` does not mean there is no workspace — only that someone else
    /// holds it, which is the [`Workspace::prepare`] shape.
    pub fn workspace(&self) -> Option<&Arc<Workspace>> {
        self.workspace.as_ref()
    }

    /// Connects this run to the side channel its runtime's
    /// [`ApprovalGate`](crate::ApprovalGate) writes to, so every
    /// [`ApprovalRequest`](crate::ApprovalRequest) carries what the call
    /// actually reaches.
    ///
    /// Needed only on the [`prepare_with_session`](super::prepare_with_session)
    /// path, where the caller built the mentra runtime and installed the gate
    /// itself. Take the handle off that gate before handing it over — mentra
    /// takes an authorizer by value and never gives it back:
    ///
    /// ```no_run
    /// # fn example(session: mentra::Session, config: &basis::RunConfig)
    /// # -> Result<basis::PreparedRun, basis::RunError> {
    /// let gate = basis::ApprovalGate::new();
    /// let levels = gate.levels();
    /// let runtime = mentra::Runtime::builder().with_tool_authorizer(gate);
    /// # let _ = runtime;
    ///
    /// Ok(basis::run::prepare_with_session(session, config, "openai", "a-model")?
    ///     .with_side_effect_levels(levels))
    /// # }
    /// ```
    ///
    /// [`Workspace::prepare`] and the free functions in
    /// [`run`](mod@crate::run) do this themselves, so no caller of those has
    /// anything to wire. Unwired, the run works exactly as before and every
    /// request reports `None` — unknown, which an approver should read as the
    /// worst the call could be rather than the least.
    ///
    /// **Interim.** This method exists only until mentra's permission event
    /// carries the classification itself
    /// ([mentra#21](https://github.com/oops-rs/mentra/issues/21)); see
    /// [`SideEffectLevels`](crate::approval::SideEffectLevels).
    pub fn with_side_effect_levels(self, levels: SideEffectLevels) -> Self {
        Self { levels, ..self }
    }

    /// The header line this run will open with, before anything is sent.
    pub fn header(&self) -> Event {
        header_for(&self.session.id().to_string(), &self.run)
    }

    /// The session this run drives, for a host that wants mentra's own surface
    /// — branching, the transcript tree, subagents — alongside basis's.
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Gives the session back, ending basis's involvement.
    ///
    /// A workspace this run was keeping alive
    /// ([`with_workspace`](Self::with_workspace)) is dropped here with the
    /// rest of the run, and its hooks and MCP connections end with it — the
    /// session that comes back is mentra's alone.
    pub fn into_session(self) -> Session {
        self.session
    }

    /// The session's id, which changes every time a session is created —
    /// including on resume.
    pub fn session_id(&self) -> String {
        self.session.id().to_string()
    }

    /// The persisted agent id: the handle [`resume`](super::resume) takes.
    ///
    /// Unlike the session id this survives the process, because it names the
    /// row in mentra's store rather than this run of it.
    pub fn agent_id(&self) -> &str {
        self.session.agent_id()
    }

    /// The committed conversation so far, oldest first.
    pub fn history(&self) -> &[mentra::Message] {
        self.session.history()
    }

    /// What this run is about, minus the session.
    pub fn context(&self) -> &RunContext {
        &self.run
    }

    /// Switches the model this conversation's later turns run on, keeping the
    /// provider it was opened with.
    ///
    /// Takes effect from the next turn: mentra threads the model into each
    /// model request as it builds it, so a turn already in flight finishes on
    /// the model it started with. It also persists — mentra rewrites the agent
    /// record — so a session resumed in another process comes back on the model
    /// it was last set to.
    ///
    /// `model` is not checked against the provider's catalogue, and
    /// deliberately: mentra does not check either, listing models is a network
    /// round trip, and a caller naming a model basis has never heard of is the
    /// ordinary case for a self-hosted endpoint. An id the provider rejects
    /// fails on the next turn, where the provider can say why.
    ///
    /// The provider is *not* switchable here. mentra resolves a provider from
    /// the runtime's registry, and a run built on one provider's credential and
    /// endpoint (ADR-0018) has no second connection to move to.
    pub fn set_model(&mut self, model: impl Into<String>) -> Result<(), RunError> {
        let model = model.into();

        self.session
            .set_model(mentra::ModelInfo::new(model.clone(), &self.run.provider))?;
        // The context is what `header()` and every report read the model from.
        // Leaving it stale would have the stream describe a run that is no
        // longer happening.
        self.run.model = model;

        Ok(())
    }

    /// Asks the model to think harder, or less hard, from the next turn on.
    ///
    /// `None` clears the request and restores the provider's own default.
    /// Persisted and deferred exactly as [`set_model`](Self::set_model) is, and
    /// for the same reason: mentra reads the level live when it builds each
    /// model request.
    ///
    /// A provider or model that does not offer the requested level fails the
    /// turn rather than quietly running at a lower one — see [`Effort`].
    pub fn set_effort(&mut self, effort: Option<Effort>) -> Result<(), RunError> {
        self.session
            .set_reasoning(effort.map(|effort| mentra::provider::ReasoningOptions {
                effort: Some(effort.into()),
                summary: None,
            }))?;
        self.effort = effort;

        Ok(())
    }

    /// The effort [`set_effort`](Self::set_effort) last asked this run for.
    ///
    /// `None` means nothing has asked, *not* that the session is at the
    /// provider's default: a run whose [`RunSpec`](crate::RunSpec) named an
    /// effort at mint had it applied to the session before this run existed,
    /// and mentra offers no way to read the level back off a session. So this
    /// answers "what has this run been set to", which is what a picker showing
    /// the user their own last choice needs, and not "what will the next
    /// request carry".
    pub const fn effort(&self) -> Option<Effort> {
        self.effort
    }

    /// Sends the configured prompt and streams the turn into `sink`.
    ///
    /// Consequential calls are approved by [`AllowAll`], the default for a run
    /// that was given no approver of its own;
    /// [`execute_with_approver`](Self::execute_with_approver) is where anything
    /// stricter goes.
    ///
    /// The stream always opens with [`Event::RunStarted`] and always closes
    /// with [`Event::RunFinished`], including when the turn fails: by then the
    /// stream has content a client needs to be able to finish reading.
    ///
    /// The session survives, so this can be called again — see
    /// [`send`](Self::send) for a turn with a different prompt.
    pub async fn execute<S: EventSink>(&mut self, sink: S) -> Result<RunReport<S>, RunError> {
        self.execute_with_approver(sink, AllowAll).await
    }

    /// Sends the configured prompt, streaming into `sink` and putting every
    /// consequential call to `approver`.
    ///
    /// The approver runs on the forwarding task while the turn is blocked
    /// waiting on it, which is what makes an interactive answer possible at
    /// all — and what means an approver must answer rather than defer. One that
    /// cannot answer denies; see [`Approver`].
    pub async fn execute_with_approver<S: EventSink, A: Approver>(
        &mut self,
        sink: S,
        approver: A,
    ) -> Result<RunReport<S>, RunError> {
        self.execute_with_approver_and_options(sink, approver, TurnOptions::default())
            .await
    }

    /// Sends the configured prompt with explicit run options — a cancellation
    /// token, a deadline, a tool budget.
    ///
    /// The one-shot path is bounded by its config but had no way to be
    /// *stopped*: a token belongs to one call, so it cannot travel in a config
    /// that mints many. This is where it arrives, and it is what a host driving
    /// a one-prompt run behind a UI needs, exactly as
    /// [`send_with_options`](Self::send_with_options) serves a conversation.
    pub async fn execute_with_options<S: EventSink>(
        &mut self,
        sink: S,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        self.execute_with_approver_and_options(sink, AllowAll, options)
            .await
    }

    /// Sends the configured prompt with both an approver and explicit options.
    pub async fn execute_with_approver_and_options<S: EventSink, A: Approver>(
        &mut self,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        let prompt = self.run.prompt.clone();
        self.turn(prompt, sink, approver, options).await
    }

    /// Starts this one-shot run under a lifecycle [`Supervisor`].
    ///
    /// The handle is returned as soon as the supervisor accepts the work.
    /// Waiting happens through [`TaskHandle::wait`], independently of the
    /// event sink. A successful task's bytes are the assistant's UTF-8 final
    /// message; a failed run becomes [`crate::TaskState::Failed`].
    ///
    /// Cancellation is cooperative: the supervisor trips the turn's own
    /// cancellation token, then waits for the run to close its event stream
    /// before publishing [`crate::TaskState::Cancelled`].
    pub async fn spawn<S: EventSink, A: Approver>(
        mut self,
        supervisor: &Supervisor,
        parent: Option<&TaskHandle>,
        detached: bool,
        sink: S,
        approver: A,
    ) -> Result<TaskHandle, LifecycleError> {
        supervisor
            .spawn_cooperative(parent, detached, move |context| async move {
                let (options, cancel) = TurnOptions::cancellable();
                let cancellation = context.cancellation();
                let execution = self.execute_with_approver_and_options(sink, approver, options);
                tokio::pin!(execution);

                let report = tokio::select! {
                    report = &mut execution => report,
                    () = cancellation.cancelled() => {
                        cancel.cancel();
                        execution.await
                    }
                }
                .map_err(|error| error.to_string())?;

                match (report.outcome, report.final_message) {
                    (RunOutcome::Ok, Some(message)) => Ok(message.into_bytes()),
                    (RunOutcome::Ok, None) => {
                        Err("run finished successfully without a final message".to_string())
                    }
                    (RunOutcome::Error { message }, _) => Err(message),
                }
            })
            .await
    }

    /// Sends a further prompt on the same conversation.
    ///
    /// This is what separates a session from a one-shot: the model sees every
    /// earlier turn, because the session was never thrown away.
    pub async fn send<S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        sink: S,
        approver: A,
    ) -> Result<RunReport<S>, RunError> {
        self.turn(prompt.into(), sink, approver, TurnOptions::default())
            .await
    }

    /// Sends a prompt with explicit run options — a cancellation token, a
    /// deadline, a tool budget.
    ///
    /// This is what a protocol server's stop button needs: ACP's
    /// `session/cancel` trips the token, and the turn ends rather than running
    /// to completion unheard.
    ///
    /// A bound left unset here falls back to the run's own
    /// ([`bounds`](Self::bounds)). Attaching a token is a statement about
    /// stopping, not about limits, and reading it as "no deadline after all"
    /// would quietly unbound a run its caller had configured.
    pub async fn send_with_options<S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        self.turn(prompt.into(), sink, approver, options).await
    }

    /// Sends a prompt whose answer must be a value of type `T` rather than
    /// prose.
    ///
    /// ADR-0010's structured output, and the primitive a workflow is built on:
    /// the model is handed one terminal tool whose input *is* the answer, and
    /// `T` is deserialized from what it sent. The caller writes the schema —
    /// see [`OutputSpec`] for why basis derives nothing.
    ///
    /// **By default a typed turn is a shaping turn, not a working one.** That
    /// terminal tool is the *only* tool the turn holds — no files, no shell, no
    /// MCP — and the model is required to call it, so the turn can answer only
    /// from the conversation it already has. Asking it to review code in the
    /// same call returns a structurally valid answer from a model that read
    /// nothing, reported as a success. Two ways past that, and they are
    /// different trades. [`OutputSpec::with_tools`] keeps the ordinary toolset
    /// on this turn, so one call reads and answers — and gives up the forcing
    /// that guaranteed an answer. Or do the work on an ordinary turn
    /// ([`send`](Self::send) or [`execute`](Self::execute)) and ask for the
    /// shape on the next, which keeps the forcing and keeps each run's reading
    /// in a context of its own; `examples/review_workflow.rs` is that written
    /// out.
    ///
    /// The stream is unchanged. Header, forwarded events, permissions put to
    /// the approver, `RunFinished`: a client reading events cannot tell a typed
    /// turn from any other, which is the point — only the return value differs.
    /// The answer travels as the terminal tool's
    /// [`ToolQueued`](Event::ToolQueued) input and
    /// [`ToolCompleted`](Event::ToolCompleted) summary, and
    /// [`RunReport::final_message`] stays `None`, because a typed turn's
    /// committed final message is that tool result — putting a JSON payload in
    /// a field named for the assistant's prose would have every client render
    /// it as speech. Prose the model wrote alongside the call, usually none,
    /// arrives as [`Event::AssistantMessage`].
    ///
    /// Where a plain turn reports its failure on the stream and still returns
    /// `Ok`, this returns `Err`: a typed turn without a value has nothing to
    /// hand back.
    ///
    /// - [`RunError::OutputMismatch`] — an answer arrived that `T` did not
    ///   accept. mentra commits the exchange before basis reads it, so the
    ///   transcript keeps the attempt and a follow-up turn can say what was
    ///   wrong with it.
    /// - [`RunError::Runtime`] — the turn failed, *or* it finished without ever
    ///   calling the terminal tool. mentra reports both as
    ///   `MalformedProviderEvent` and basis will not read error prose to tell
    ///   them apart. A working turn ([`OutputSpec::with_tools`]) reaches the
    ///   second of those the most ways, since nothing forces its ending: it can
    ///   answer in prose, or be refused another round by a bound while it is
    ///   still gathering. Which bound that was is on the stream, as
    ///   [`Event::RunFinished`]'s `stopped_by` — [`Bound::TokenBudget`] for an
    ///   allowance spent mid-gather — and only there, because the report that
    ///   would otherwise carry it is not handed back when there is no value to
    ///   hand back with it.
    ///
    /// The stream is complete and closed in every one of those cases, so a sink
    /// with somewhere to put events — a file, a channel — has the whole run.
    /// Only the sink *value* is lost, because it comes back inside the report.
    ///
    /// ```no_run
    /// use serde::Deserialize;
    /// use serde_json::json;
    ///
    /// #[derive(Deserialize)]
    /// struct Review {
    ///     verdict: String,
    /// }
    ///
    /// # async fn example(run: &mut basis::PreparedRun) -> Result<(), basis::RunError> {
    /// let spec = basis::OutputSpec::new(
    ///     "submit_review",
    ///     "call this once you have weighed everything you read on the last turn",
    ///     json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "verdict": { "type": "string", "description": "ship or hold" }
    ///         },
    ///         "required": ["verdict"]
    ///     }),
    /// );
    ///
    /// // The reading happened on an ordinary turn; this one only shapes it.
    /// run.execute(basis::NullSink).await?;
    /// let output = run
    ///     .output::<Review, _, _>(
    ///         "submit your review of what you just read",
    ///         spec,
    ///         basis::NullSink,
    ///         basis::AllowAll,
    ///     )
    ///     .await?;
    ///
    /// // A value, not a paragraph to parse — and what it cost, for a caller
    /// // adding runs up against a budget.
    /// println!("{} ({} tokens)", output.value.verdict, output.report.usage.total_tokens());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn output<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        spec: OutputSpec,
        sink: S,
        approver: A,
    ) -> Result<OutputReport<T, S>, RunError> {
        self.typed_turn(prompt.into(), spec, sink, approver, TurnOptions::default())
            .await
    }

    /// A typed turn with explicit run options.
    ///
    /// Same relationship to [`output`](Self::output) as
    /// [`send_with_options`](Self::send_with_options) has to
    /// [`send`](Self::send): a typed turn is cancellable and boundable like any
    /// other, and a fan-out that gives each of its runs a deadline should not
    /// have to give up types to get one.
    pub async fn output_with_options<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        spec: OutputSpec,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<OutputReport<T, S>, RunError> {
        self.typed_turn(prompt.into(), spec, sink, approver, options)
            .await
    }

    /// One turn, start to finish: header, forwarded events, outcome.
    async fn turn<S: EventSink, A: Approver>(
        &mut self,
        prompt: String,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        let options = bounded(options, &self.bounds);
        drawable(&options)?;
        let turn = self.begin(&prompt, sink, approver)?;

        // Kept rather than passed straight in: mentra takes the options by
        // value, and the clone is how the run's own account of why it ended
        // gets back here. See [`ended_on`].
        let run_options = self.run_options(options);
        let observed = run_options.clone();

        let result = self
            .session
            .append_turn_with_options(vec![ContentBlock::text(prompt)], run_options)
            .await;

        let ended = match &result {
            Ok(message) => Ended::Answered(Some(message.text())),
            Err(error) => Ended::Failed(error),
        };
        self.finish(turn, ended, &observed).await
    }

    /// One typed turn. Identical to [`turn`](Self::turn) but for the one call
    /// in the middle — which is the whole reason both are written this way,
    /// since a second copy of the header-and-forwarding dance is a second thing
    /// to keep in step with the stream contract.
    ///
    /// mentra is asked for a [`Value`] rather than for `T` directly, and basis
    /// deserializes. That costs nothing (the payload is already JSON) and buys
    /// the error distinction: a value that does not fit `T` is basis's own
    /// finding, reported as [`RunError::OutputMismatch`], instead of arriving
    /// as one more `MalformedProviderEvent` indistinguishable from a provider
    /// that misbehaved.
    async fn typed_turn<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: String,
        spec: OutputSpec,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<OutputReport<T, S>, RunError> {
        let options = bounded(options, &self.bounds);
        drawable(&options)?;
        let turn = self.begin(&prompt, sink, approver)?;

        // The same clone the untyped turn keeps, for the same reason: a typed
        // turn is boundable like any other and owes the same account of why it
        // ended.
        let run_options = self.run_options(options);
        let observed = run_options.clone();

        let result = self
            .session
            .append_turn_to_output::<Value>(
                vec![ContentBlock::text(prompt)],
                run_options,
                spec.into_terminal_spec(),
            )
            .await;

        let typed = match result {
            Ok(output) => Ok(serde_json::from_value::<T>(output.value)),
            Err(error) => Err(error),
        };
        let ended = match &typed {
            Ok(Ok(_)) => Ended::Answered(None),
            Ok(Err(mismatch)) => Ended::Mismatched(mismatch),
            Err(error) => Ended::Failed(error),
        };

        let report = self.finish(turn, ended, &observed).await?;

        match typed {
            Ok(Ok(value)) => Ok(OutputReport { value, report }),
            Ok(Err(mismatch)) => Err(RunError::OutputMismatch(mismatch)),
            Err(error) => Err(RunError::Runtime(error)),
        }
    }

    /// Opens a turn: checks the prompt, emits the header, starts forwarding.
    ///
    /// Split from [`finish`](Self::finish) so that everything between them is
    /// exactly one call to mentra. Every entry point above shares this pair, so
    /// no turn can announce itself differently from the others — the same split
    /// mentra makes internally, and for the same reason.
    fn begin<S: EventSink, A: Approver>(
        &self,
        prompt: &str,
        sink: S,
        approver: A,
    ) -> Result<Turn<S>, RunError> {
        if prompt.trim().is_empty() {
            return Err(RunError::EmptyPrompt);
        }

        let permissions = self.session.permission_handle();
        let session_id = self.session.id().to_string();
        let receiver = self.session.subscribe();

        let mut sink = sink;
        sink.emit(header_for(&session_id, &self.run))?;

        let (done, done_rx) = oneshot::channel();
        let forwarder = tokio::spawn(forward_events(
            receiver,
            sink,
            done_rx,
            approver,
            permissions,
            self.levels.clone(),
        ));

        Ok(Turn {
            session_id,
            done,
            forwarder,
        })
    }

    /// Closes a turn opened by [`begin`](Self::begin): stops forwarding, states
    /// the outcome on the stream, and reports.
    ///
    /// Classifying `ended` here rather than at each call site is what keeps one
    /// answer to "did this run succeed" — a second site would be a second
    /// opinion. `observed` is the caller's clone of the options the run was
    /// given, and is what the same single answer to "and what ended it" is read
    /// from.
    async fn finish<S: EventSink>(
        &self,
        turn: Turn<S>,
        ended: Ended<'_>,
        observed: &RunOptions,
    ) -> Result<RunReport<S>, RunError> {
        let Turn {
            session_id,
            done,
            forwarder,
        } = turn;

        // The forwarder stops on this signal rather than on the channel
        // closing, so a sender clone held elsewhere in the runtime cannot
        // strand the task.
        let _ = done.send(());
        let (mut sink, usage) = forwarder.await?;

        let (final_message, outcome, stopped_by) = match ended {
            Ended::Answered(final_message) => {
                (final_message, RunOutcome::Ok, ended_on(observed, None))
            }
            Ended::Failed(error) => (
                None,
                RunOutcome::Error {
                    message: chain_message(error),
                },
                ended_on(observed, Some(error)),
            ),
            // The turn itself completed, so mentra kept the exchange — but the
            // caller asked for a shape and did not get one, and a stream that
            // said "ok" while its caller received an error would be describing
            // a different run from the one that happened.
            Ended::Mismatched(mismatch) => (
                None,
                RunOutcome::Error {
                    message: format!("output did not match the requested type: {mismatch}"),
                },
                None,
            ),
        };

        sink.emit(Event::RunFinished {
            outcome: outcome.clone(),
            stopped_by,
        })?;

        Ok(RunReport {
            session_id,
            model: self.run.model.clone(),
            provider: self.run.provider.clone(),
            final_message,
            outcome,
            stopped_by,
            usage,
            sink,
        })
    }
}

/// A turn in flight: the forwarding task, and the signal that ends it.
struct Turn<S> {
    session_id: String,
    done: oneshot::Sender<()>,
    forwarder: tokio::task::JoinHandle<(S, RunUsage)>,
}

/// How a turn ended, in the terms the stream reports.
///
/// Borrowed rather than owned so a caller can hand the failure over for
/// classification and still return it: the error a typed turn reports to its
/// caller and the message the stream carries have to be the same error.
enum Ended<'a> {
    /// mentra completed the turn. Carries the assistant's final prose, when the
    /// turn had any — a typed turn's answer is not prose and is not put here.
    Answered(Option<String>),
    /// mentra failed the turn.
    Failed(&'a mentra::error::RuntimeError),
    /// The turn completed, but its answer did not fit the requested type.
    Mismatched(&'a serde_json::Error),
}

/// Which bound ended the turn, if one did.
///
/// Two sources, asked in this order because only one of them is the runner's
/// own account. mentra records a graceful early end at the boundary it decides
/// on — reachable here through the caller's clone of the options — while
/// [`tripped_bound`] can only read a failure after the fact. So the record is
/// consulted first and on *both* arms: a run that ends on its token budget with
/// the assistant's answer already committed returns an ordinary `Ok` carrying
/// ordinary prose, and nothing in that result says an allowance is why there is
/// no more of it.
///
/// [`EarlyEnd::StopRequested`] deliberately maps to nothing. A stop is an
/// instruction the caller issued rather than an allowance the run outgrew, and
/// basis has no `Bound` for it — inventing one would put a caller's own stop
/// button on the same exit code as running out of budget.
fn ended_on(observed: &RunOptions, error: Option<&mentra::error::RuntimeError>) -> Option<Bound> {
    match observed.ended_early() {
        Some(EarlyEnd::TokenBudget) => Some(Bound::TokenBudget),
        // `EarlyEnd` is non-exhaustive, and a variant basis has not been taught
        // is not a bound basis can name. Falling through leaves the failure to
        // speak for itself rather than guessing.
        _ => error.and_then(tripped_bound),
    }
}

/// Which of the run's own bounds ended the turn, if one did.
///
/// Classified here, from the typed error, rather than left for someone to
/// recognize in a message later — a caller matching on prose would break the
/// first time mentra reworded one.
fn tripped_bound(error: &mentra::error::RuntimeError) -> Option<Bound> {
    match error {
        mentra::error::RuntimeError::DeadlineExceeded => Some(Bound::Deadline),
        mentra::error::RuntimeError::ToolBudgetExceeded(_) => Some(Bound::ToolBudget),
        // Everything else is a failure of the work, not of the allowance: a
        // provider error, a cancelled turn, an unreadable transcript.
        _ => None,
    }
}

/// Renders `error`'s message together with whatever its `source()` chain adds
/// that the message does not already say.
///
/// thiserror interpolates a `#[source]` straight into `Display` wherever a
/// variant's format string names it, and every
/// [`RuntimeError`](mentra::error::RuntimeError) variant does — so
/// `error.to_string()` already reads several layers deep on its own, down to
/// whatever the innermost wrapped type's `Display` shows. The gap is past
/// that point: `reqwest::Error`'s `Display` only classifies itself ("error
/// sending request for url (...)") and never describes its own `source()`, so
/// a DNS failure, a refused connection, or a TLS handshake error — the actual
/// reason a `ProviderError::Transport` or `ProviderError::Decode` failed —
/// reaches neither `to_string()` nor, since mentra's own stream event for the
/// same failure is built the same way (`Session::finish_turn`), the event
/// stream either. Walking the chain here recovers it, and is the only place
/// in basis that needs to: everywhere else a `RuntimeError`'s `Display`
/// already says everything its sources do.
///
/// Safe to run unconditionally. Nothing reachable from a `RuntimeError` today
/// forwards a request or response body through `source()` — that path is
/// `ProviderError::Http`, which interpolates its body into `Display` directly
/// rather than through a source, and this function does not change what it
/// shows. The substring check below is what keeps a level whose text a parent
/// already interpolated — exactly what happens one hop up, via thiserror's
/// own `{0}` — from being repeated.
fn chain_message(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(source) = cause {
        let text = source.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        cause = source.source();
    }
    message
}

/// Builds the opening line. Kept separate so [`PreparedRun::header`] and the
/// line actually emitted can never drift apart.
fn header_for(session_id: &str, run: &RunContext) -> Event {
    Event::RunStarted {
        schema: EVENT_SCHEMA_VERSION,
        basis: env!("CARGO_PKG_VERSION").to_string(),
        session_id: session_id.to_string(),
        workspace: run.workspace.clone(),
        model: run.model.clone(),
        provider: run.provider.clone(),
        context_files: run
            .context
            .documents()
            .iter()
            .map(|document| ContextFile {
                path: document.path.clone(),
                scope: document.scope.label(),
            })
            .collect(),
        skills_dirs: run.skills_dirs.clone(),
        skills: run
            .skills
            .iter()
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect(),
        templates_dirs: run.templates_dirs.clone(),
        templates: run
            .templates
            .iter()
            .map(|template| TemplateSummary {
                name: template.name.clone(),
                description: template.description.clone(),
                argument_hint: template.argument_hint.clone(),
            })
            .collect(),
        mcp_files: run.mcp_files.clone(),
        mcp_servers: run.mcp_servers.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextDocument, ContextScope};

    #[test]
    fn the_header_lists_context_files_weakest_first() {
        let context = WorkspaceContext::from_documents(vec![
            ContextDocument {
                path: PathBuf::from("/AGENTS.md"),
                scope: ContextScope::Ancestor { depth: 2 },
                content: "outer".to_string(),
            },
            ContextDocument {
                path: PathBuf::from("/repo/AGENTS.md"),
                scope: ContextScope::Workspace,
                content: "inner".to_string(),
            },
        ]);

        let files: Vec<ContextFile> = context
            .documents()
            .iter()
            .map(|document| ContextFile {
                path: document.path.clone(),
                scope: document.scope.label(),
            })
            .collect();

        assert_eq!(files[0].scope, "ancestor:2");
        assert_eq!(files[1].scope, "workspace");
    }

    #[test]
    fn a_tripped_bound_is_told_apart_from_a_failed_run() {
        use mentra::error::RuntimeError;

        assert_eq!(
            tripped_bound(&RuntimeError::DeadlineExceeded),
            Some(Bound::Deadline)
        );
        assert_eq!(
            tripped_bound(&RuntimeError::ToolBudgetExceeded(40)),
            Some(Bound::ToolBudget)
        );

        // A run the provider refused is a failure, and a shell script that
        // retried it as if it had merely run out of time would retry forever.
        assert_eq!(tripped_bound(&RuntimeError::EmptyAssistantResponse), None);
        assert_eq!(tripped_bound(&RuntimeError::Cancelled), None);
    }

    /// The options a run that ended on `end` hands back to whoever kept a clone.
    fn recorded(end: EarlyEnd) -> RunOptions {
        let slot = std::sync::OnceLock::new();
        let _ = slot.set(end);
        RunOptions {
            early_end: std::sync::Arc::new(slot),
            ..RunOptions::default()
        }
    }

    #[test]
    fn a_run_that_answered_still_names_the_budget_that_ended_it() {
        // The case that makes reading mentra's record load-bearing rather than
        // decorative, and the reason both arms consult it: the turn returns an
        // ordinary `Ok` carrying ordinary prose, so nothing in the result tells
        // "the model was done" from "the allowance ran out" except what the
        // runner wrote down at the boundary it decided at.
        //
        // Checked here rather than end to end because basis cannot reach the
        // shape from outside: mentra only re-checks the budget after a
        // committed final message when a steer or a follow-up is queued behind
        // it, and basis exposes neither.
        assert_eq!(
            ended_on(&recorded(EarlyEnd::TokenBudget), None),
            Some(Bound::TokenBudget)
        );
    }

    #[test]
    fn a_budget_that_ends_a_run_owing_an_answer_is_not_read_as_a_provider_failure() {
        // The shape a driven run reaches — `tests/token_budget.rs` — where the
        // failure is real but the reason for it is the allowance. Classifying
        // by error alone would exit 1 here, sending someone after a broken
        // provider when the fix is a larger budget.
        use mentra::error::RuntimeError;

        assert_eq!(
            ended_on(
                &recorded(EarlyEnd::TokenBudget),
                Some(&RuntimeError::EmptyAssistantResponse)
            ),
            Some(Bound::TokenBudget)
        );
    }

    #[test]
    fn a_graceful_stop_is_not_reported_as_a_bound() {
        // A caller's own stop button is an instruction, not an allowance the
        // run outgrew. basis has no `Bound` for it, and borrowing one would give
        // a client's stop the exit code of a run that ran out of budget.
        use mentra::error::RuntimeError;

        assert_eq!(ended_on(&recorded(EarlyEnd::StopRequested), None), None);
        assert_eq!(
            ended_on(
                &recorded(EarlyEnd::StopRequested),
                Some(&RuntimeError::EmptyAssistantResponse)
            ),
            None
        );
    }

    #[test]
    fn a_run_that_recorded_nothing_is_classified_by_its_failure_alone() {
        use mentra::error::RuntimeError;

        assert_eq!(ended_on(&RunOptions::default(), None), None);
        assert_eq!(
            ended_on(
                &RunOptions::default(),
                Some(&RuntimeError::DeadlineExceeded)
            ),
            Some(Bound::Deadline)
        );
    }

    /// A leaf with no further source — what most of `RuntimeError`'s own
    /// variants look like.
    #[derive(Debug)]
    struct Leaf(&'static str);

    impl std::fmt::Display for Leaf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Leaf {}

    /// A wrapper whose `Display` does not repeat its source's text — the
    /// shape `reqwest::Error` takes, and the one `chain_message` exists for.
    #[derive(Debug)]
    struct Opaque {
        own_text: &'static str,
        source: Leaf,
    }

    impl std::fmt::Display for Opaque {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.own_text)
        }
    }

    impl std::error::Error for Opaque {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    /// A wrapper whose `Display` interpolates its source's text directly —
    /// the shape every `RuntimeError` variant takes via thiserror's `{0}`.
    #[derive(Debug)]
    struct Interpolated {
        source: Leaf,
    }

    impl std::fmt::Display for Interpolated {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wrapper failed: {}", self.source)
        }
    }

    impl std::error::Error for Interpolated {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn a_leaf_error_is_left_exactly_as_its_own_display_wrote_it() {
        let error = Leaf("no providers are registered");

        assert_eq!(chain_message(&error), "no providers are registered");
    }

    #[test]
    fn a_source_a_wrappers_display_never_mentions_is_appended() {
        // `reqwest::Error`'s own case: "error sending request for url (...)"
        // says nothing about *why* the request failed, because it never
        // describes its `source()`. Without walking the chain, that reason —
        // here, "connection refused" — is gone the moment `.to_string()` is
        // called, on the report and on mentra's own stream event alike.
        let error = Opaque {
            own_text: "error sending request for url (http://127.0.0.1:1/)",
            source: Leaf("connection refused (os error 61)"),
        };

        assert_eq!(
            chain_message(&error),
            "error sending request for url (http://127.0.0.1:1/): connection refused (os error 61)"
        );
    }

    #[test]
    fn a_source_a_wrappers_display_already_quotes_is_not_repeated() {
        // Exactly what every `RuntimeError` variant does one hop up, via
        // thiserror's `{0}`: the source's text is already in the parent's
        // `Display`, so walking `source()` too would say it twice.
        let error = Interpolated {
            source: Leaf("disk quota exceeded"),
        };

        assert_eq!(
            chain_message(&error),
            "wrapper failed: disk quota exceeded",
            "the source's text must appear once, not twice"
        );
    }

    #[test]
    fn a_chain_three_levels_deep_still_reaches_its_root_cause() {
        struct Middle {
            source: Opaque,
        }

        impl std::fmt::Debug for Middle {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("Middle").finish()
            }
        }

        impl std::fmt::Display for Middle {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "failed to send provider request: {}", self.source)
            }
        }

        impl std::error::Error for Middle {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.source)
            }
        }

        // `RuntimeError::FailedToSendRequest` wrapping `ProviderError::Transport`
        // wrapping `reqwest::Error`: two levels interpolate cleanly into
        // `Display`, and the third — the one that doesn't — is where the
        // actual cause was hiding.
        let error = Middle {
            source: Opaque {
                own_text: "provider transport error: error sending request for url (http://127.0.0.1:1/)",
                source: Leaf("connection refused (os error 61)"),
            },
        };

        assert_eq!(
            chain_message(&error),
            "failed to send provider request: provider transport error: error sending request for url (http://127.0.0.1:1/): connection refused (os error 61)"
        );
    }

    #[test]
    fn a_real_runtime_errors_already_complete_message_is_unchanged() {
        // serde_json's `Display` is already the full story — message, line,
        // and column — and its `source()` is written to skip back to
        // whatever `Display` already showed rather than repeat it, the same
        // shape as most of `RuntimeError`'s own variants. The chain walk must
        // add nothing here, on a real mentra error rather than a synthetic one.
        use mentra::error::RuntimeError;

        let parse_error =
            serde_json::from_str::<Value>("{").expect_err("truncated JSON does not parse");
        let error = RuntimeError::FailedToSerializeTasks(parse_error);

        assert_eq!(chain_message(&error), error.to_string());
    }
}
