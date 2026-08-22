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
    Session,
    runtime::{ProviderRetry, RunOptions},
};
use tokio::sync::oneshot;

use self::{
    forward::forward_events,
    outcome::{Ended, chain_message, ended_on},
};
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

mod compact;
mod forward;
mod outcome;
mod prompt;
mod typed;

pub use compact::Compacted;
pub use prompt::PromptPart;

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

    /// Renames this conversation, and persists the new name.
    ///
    /// The name is what [`store::list`](crate::store::list) — and so ACP's
    /// `session/list` — reports as a conversation's title, and mentra fixes it
    /// at creation otherwise. That is the wrong moment for it: a session is
    /// opened before anyone knows what it will be about, so a host that mints
    /// one per conversation is stuck offering a list of identical placeholders.
    ///
    /// Nothing derives a name here. What a conversation should be called is a
    /// convention of whatever is driving it — its first prompt, a ticket id,
    /// what the user typed — and basis has no opinion to impose (PROPOSAL.md
    /// Bet 4).
    pub fn set_name(&mut self, name: impl Into<String>) -> Result<(), RunError> {
        self.session.set_name(name)?;

        Ok(())
    }

    /// The name this conversation is filed under.
    pub fn name(&self) -> &str {
        self.session.name()
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

        Ok(())
    }

    /// The level this session's next turn will be sent with.
    ///
    /// Read off the session rather than tracked here, which is what makes it
    /// an answer about the conversation rather than about this handle on it: a
    /// run whose [`RunSpec`](crate::RunSpec) or whose repository's
    /// `config.json` named an effort had it applied at mint, before anything
    /// called [`set_effort`](Self::set_effort), and a tracked copy reported
    /// `None` for a session demonstrably running at `high`.
    ///
    /// `None` means no level is being requested — the provider's own default —
    /// and not that nobody has asked yet. A level mentra has grown and basis
    /// has no name for also reads as `None`, because reporting the wrong one
    /// is worse than reporting none; see [`Effort`]'s `TryFrom`.
    pub fn effort(&self) -> Option<Effort> {
        self.session
            .reasoning()
            .and_then(|reasoning| reasoning.effort)
            .and_then(|effort| Effort::try_from(effort).ok())
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
        self.turn(vec![PromptPart::Text(prompt)], sink, approver, options)
            .await
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
        self.turn(
            vec![PromptPart::Text(prompt.into())],
            sink,
            approver,
            TurnOptions::default(),
        )
        .await
    }

    /// Sends a prompt that is not only text — a screenshot, a diagram, a photo
    /// of a whiteboard — on the same conversation.
    ///
    /// Additive to [`send`](Self::send) rather than replacing it, because the
    /// overwhelming majority of turns are a line of text and should not have to
    /// build a vector to say so. `send` is this with one
    /// [`PromptPart::Text`].
    ///
    /// The parts reach the model in the order they are given, which is
    /// load-bearing: "look at this, and tell me what changed" reads differently
    /// depending on which side of the image the question is on.
    ///
    /// Every provider mentra serves carries inline image bytes — the Responses
    /// transport as a `data:` URL, Anthropic as a base64 source, Gemini as
    /// `inlineData` — so this is portable in a way an image *URL* is not; see
    /// [`PromptPart`] for why basis offers only the bytes. A media type a
    /// particular model does not accept fails the turn, with the provider's own
    /// reason on the stream.
    ///
    /// ```no_run
    /// use basis::PromptPart;
    ///
    /// # async fn example(run: &mut basis::PreparedRun, png: Vec<u8>) -> Result<(), basis::RunError> {
    /// run.send_parts(
    ///     vec![
    ///         PromptPart::text("this is what the page renders as"),
    ///         PromptPart::image("image/png", png),
    ///         PromptPart::text("the footer overlaps the last row — why?"),
    ///     ],
    ///     basis::NullSink,
    ///     basis::AllowAll,
    ///     basis::TurnOptions::default(),
    /// )
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_parts<S: EventSink, A: Approver>(
        &mut self,
        parts: Vec<PromptPart>,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        self.turn(parts, sink, approver, options).await
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
        self.turn(
            vec![PromptPart::Text(prompt.into())],
            sink,
            approver,
            options,
        )
        .await
    }

    /// One turn, start to finish: header, forwarded events, outcome.
    async fn turn<S: EventSink, A: Approver>(
        &mut self,
        parts: Vec<PromptPart>,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        let options = bounded(options, &self.bounds);
        drawable(&options)?;
        let turn = self.begin(&parts, sink, approver)?;

        // Kept rather than passed straight in: mentra takes the options by
        // value, and the clone is how the run's own account of why it ended
        // gets back here. See [`ended_on`].
        let run_options = self.run_options(options);
        let observed = run_options.clone();

        let result = self
            .session
            .append_turn_with_options(prompt::into_blocks(parts), run_options)
            .await;

        let ended = match &result {
            Ok(message) => Ended::Answered(Some(message.text())),
            Err(error) => Ended::Failed(error),
        };
        self.finish(turn, ended, &observed).await
    }

    /// Opens a turn: checks the prompt, emits the header, starts forwarding.
    ///
    /// Split from [`finish`](Self::finish) so that everything between them is
    /// exactly one call to mentra. Every entry point above shares this pair, so
    /// no turn can announce itself differently from the others — the same split
    /// mentra makes internally, and for the same reason.
    fn begin<S: EventSink, A: Approver>(
        &self,
        parts: &[PromptPart],
        sink: S,
        approver: A,
    ) -> Result<Turn<S>, RunError> {
        // Emptiness is asked of the whole prompt rather than of its text: a
        // client that attached a screenshot and typed no caption sent
        // something, and refusing that would be refusing the attachment.
        if prompt::says_nothing(parts) {
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

        // Stated even when every counter is zero: this producer *did* report,
        // and "the provider said nothing" is a fact worth being able to read
        // off the line rather than infer from its absence.
        sink.emit(Event::RunFinished {
            outcome: outcome.clone(),
            stopped_by,
            usage: Some(usage),
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
mod tests;
