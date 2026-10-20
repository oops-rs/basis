//! Driving a session that is already built.
//!
//! Splitting this from [`run`](super::run) gives two things. A host that
//! already owns a mentra [`Runtime`](mentra::Runtime) — with its own provider,
//! store, or custom tools — can still use basis's context discovery and event
//! stream instead of reimplementing them. And basis's own tests can drive the
//! whole pipeline against a scripted runtime, so the event contract is checked
//! without a network call.

use std::sync::{Arc, Mutex, PoisonError};

use mentra::{Session, runtime::RunOptions};
use tokio::sync::oneshot;

use self::{
    context::ContextSnapshot,
    forward::forward_events,
    header::header_for,
    outcome::{Ended, chain_message, ended_on},
};
use super::{
    Bound, Effort, EventSink, ModelInfo, OutputAttempt, OutputAttemptReport, OutputDecision,
    OutputFailure, OutputReport, OutputReservation, OutputSpec, ReasoningOptions, RunError,
    RunReport, RunUsage, TurnOptions,
    turn::{bounded, drawable},
};
use crate::{
    approval::Approver,
    context::WorkspaceContext,
    event::{
        Binding, ContextFile, EVENT_SCHEMA_VERSION, Event, RunOutcome, SkillSummary,
        TemplateSummary,
    },
    runtime::RetryPolicy,
    runtime::Role,
    templates::Template,
    workspace::Workspace,
};

mod compact;
mod context;
mod forward;
mod header;
mod observer;
mod outcome;
mod prompt;
mod session;
mod typed;

pub use compact::Compacted;
pub use header::{LoadedSkill, RunContext};
pub use observer::AgentEventTapGuard;
pub use prompt::PromptPart;

fn history_text(message: &mentra::Message) -> Option<(Role, String)> {
    match message.role {
        Role::User => Some((Role::User, message.text())),
        Role::Assistant => Some((Role::Assistant, message.text())),
        Role::Unknown(_) => None,
    }
}

/// A session and the prompt to send it. Nothing has been sent yet.
pub struct PreparedRun {
    // **Declaration order is drop order, and one pair depends on it:**
    // `session` must drop before `agent_row`. The row must outlive the agent's
    // lease, and `session` is what releases that lease — mentra ties it to the
    // session's handle chain. Moving `agent_row` above `session` would change
    // that silently.
    //
    // `workspace`'s position is free, and claiming otherwise would be worse
    // than saying nothing: by the time it drops the session is already gone,
    // and a `Workspace` touches no part of the agent ledger on its way out —
    // its `hooks` hold releases an `Arc<AgentRegistry>` refcount, which
    // mutates no entry.
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
    /// This session's row on the runtime's agent ledger, held for exactly as
    /// long as the run.
    ///
    /// **The row's lifetime is this run's, and it has to be.** A workspace
    /// hands back a `PreparedRun` without attaching itself to it, so a host may
    /// drop the workspace and keep running — and the guard that judges this
    /// session's calls reads that ledger on every one of them. Released with
    /// the workspace, the row would vanish under a live session and leave it
    /// unattributable, which for a bridged name means *allowed*: one client's
    /// session reaching another client's authenticated server
    /// (`docs/proposals/0004`).
    ///
    /// `None` on the [`prepare_with_session`](super::prepare_with_session)
    /// path, where basis minted no session and has no row to hold — the same
    /// posture as `workspace` above, and the one the guards already describe as
    /// unjudged.
    #[allow(dead_code, reason = "held for its Drop")]
    agent_row: Option<crate::runtime::agents::AgentRow>,
    /// The provider retry fallback copied from the [`Runtime`](crate::Runtime)
    /// that minted it.
    ///
    /// A per-run field for a runtime-scoped knob because that is the shape
    /// mentra offers: the policy rides on `RunOptions`, so a runtime's default
    /// has to be carried to each run rather than set once upstream. One
    /// [`TurnOptions`](crate::TurnOptions) may override either half for its
    /// call. [`Workspace`](crate::Workspace) puts it here at mint; a caller on
    /// the [`prepare_with_session`](super::prepare_with_session) path built the
    /// mentra runtime itself and gets mentra's default, which is the only
    /// honest answer when basis was never told about a provider connection.
    retry_policy: RetryPolicy,
    /// What this run's mint knew about its system prompt; see
    /// [`estimated_context_tokens`](Self::estimated_context_tokens).
    ///
    /// Named apart from [`context()`](Self::context)'s own `run: RunContext`
    /// deliberately: the two answer different questions and a shared name
    /// would blur them at every call site.
    context_snapshot: ContextSnapshot,
    /// The minting workspace's remote agents, live (ADR-0031), so the header
    /// each turn opens with names the set as it stands then rather than as it
    /// stood at mint. `None` off the workspace path.
    remote: Option<Arc<crate::agents::remote::RemoteAgents>>,
    /// What each binding reports for this run (ADR-0039), read again each
    /// time a header is written: a set that changed after mint is reported as
    /// it stands.
    bindings: Vec<BindingSource>,
    /// The copy of `spawn` the minting workspace registered on this session
    /// for its remote agents, kept so
    /// [`with_remote_conversations`](Self::with_remote_conversations) can
    /// register it again bound to a key. `None` off the workspace path.
    remote_spawn: Option<crate::tools::SpawnTool>,
}

/// What reports one binding for a run (ADR-0039): read each time a header
/// is written, so a set that is live (ADR-0031) is reported as it stands
/// then, and one that is fixed costs a clone.
pub type BindingSource = Arc<dyn Fn() -> Binding + Send + Sync>;

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
            .field("bindings", &self.bindings.len())
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
            agent_row: None,
            retry_policy: RetryPolicy::default(),
            context_snapshot: ContextSnapshot::default(),
            remote: None,
            bindings: Vec::new(),
            remote_spawn: None,
        }
    }

    /// Keeps this run's conversations with remote agents under the host's
    /// `key` rather than under this run's own agent (ADR-0035).
    ///
    /// By default, what `spawn "@<agent> …"` remembers — the conversation it
    /// continues and a task still outstanding, such as one waiting for an
    /// answer — belongs to the calling agent, so a host that mints a fresh
    /// agent per message of one chat loses it between messages: the answer
    /// to a remote agent's question arrives in a new agent, and starts a new
    /// remote task. Bound to one key, every run shares one record: the next
    /// run's `@<agent> <answer>` answers the question the last one was asked,
    /// and `@<agent> <task>` continues the same remote context. Runs on
    /// different keys never see each other's.
    ///
    /// The record lives beside this runtime's other remote-conversation records, in a file
    /// named for the key (any string; one that is not a plain file name is
    /// made into one, so a key never names a path elsewhere), and is read
    /// afresh on every call. A runtime whose history is kept nowhere keeps it
    /// in memory, shared by the runs of one workspace open. Two runs on one
    /// key at once are last writer wins; see ADR-0035.
    /// [`Workspace::remote_conversation`](crate::Workspace::remote_conversation)
    /// reads where a key's conversation with an agent stands.
    ///
    /// A no-op for a run no workspace minted, which has no remote agents.
    /// Delegated children keep their own conversations in memory, as before.
    #[must_use]
    pub fn with_remote_conversations(mut self, key: impl Into<String>) -> Self {
        if let Some(copy) = &self.remote_spawn {
            self.session
                .register_tool(copy.clone().remembering_under(key.into()));
        }
        self
    }

    /// Remembers the copy of `spawn` the workspace registered for this run.
    pub(crate) fn with_remote_spawn(self, copy: crate::tools::SpawnTool) -> Self {
        Self {
            remote_spawn: Some(copy),
            ..self
        }
    }

    /// Lets this run's header follow the minting workspace's remote agents.
    pub(crate) fn with_remote_agents(
        self,
        remote: Arc<crate::agents::remote::RemoteAgents>,
    ) -> Self {
        Self {
            remote: Some(remote),
            ..self
        }
    }

    /// Adds a binding to what this run's header reports (ADR-0039):
    /// `source` is read each time a header is written. basis registers its
    /// MCP and A2A sets this way; a host with a binding of its own does the
    /// same, under a `kind` of its own.
    #[must_use]
    pub fn with_binding(mut self, source: BindingSource) -> Self {
        self.bindings.push(source);
        self
    }

    /// The header line, with the live half read now: [`context`](Self::context)
    /// describes the run as minted, but a long-lived session's later turns
    /// should not announce a set of remote agents a reload already replaced.
    fn header_now(&self, session_id: &str) -> Event {
        let mut header = header_for(session_id, &self.run);
        if let (Some(remote), Event::RunStarted { remote_agents, .. }) = (&self.remote, &mut header)
        {
            *remote_agents = remote.names();
        }
        if let Event::RunStarted { bindings, .. } = &mut header {
            *bindings = self.bindings.iter().map(|source| source()).collect();
        }
        header
    }

    /// Hands this run the ledger row its session was recorded under, to hold
    /// for its life. See the field.
    pub(crate) fn with_agent_row(mut self, row: crate::runtime::agents::AgentRow) -> Self {
        self.agent_row = Some(row);
        self
    }

    fn observe_usage(&self) -> (Arc<Mutex<RunUsage>>, AgentEventTapGuard) {
        let usage = Arc::new(Mutex::new(RunUsage::default()));
        let observed = Arc::clone(&usage);
        let tap = self.register_agent_event_tap(move |event| {
            let mut tally = observed.lock().unwrap_or_else(PoisonError::into_inner);
            *tally = tally.recording_agent(event);
        });
        (usage, tap)
    }

    fn finish_usage(usage: Arc<Mutex<RunUsage>>, tap: AgentEventTapGuard) -> RunUsage {
        drop(tap);
        *usage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the system prompt this run's mint opened with.
    ///
    /// [`Workspace::minted`](crate::workspace::Workspace) is the only caller.
    /// A fresh prepare records the final per-run prompt Basis handed Mentra; a
    /// resume records the prompt the *persisted* agent carries, read back off
    /// the resumed session, which may differ from what this workspace would
    /// mint today and is the one the run will actually send.
    /// [`prepare_with_session`](super::prepare_with_session),
    /// the path with no workspace, leaves this unknown: there is no workspace
    /// to ask and no session config basis put there.
    pub(crate) fn with_context_snapshot(self, system_prompt: Option<String>) -> Self {
        Self {
            context_snapshot: ContextSnapshot::new(system_prompt),
            ..self
        }
    }

    /// Carries the minting runtime's provider retry fallback onto this run.
    ///
    /// Set by [`Workspace`](crate::Workspace) at mint, at the one place both
    /// `prepare` and `resume` go through. Per-turn overrides compose on top in
    /// [`run_options`](Self::run_options).
    pub(crate) fn with_retry_policy(self, retry_policy: RetryPolicy) -> Self {
        Self {
            retry_policy,
            ..self
        }
    }

    /// The mentra options one turn runs on: what the caller asked for, filled
    /// in from this run's bounds by the caller, with the runtime's retry policy
    /// as the fallback.
    ///
    /// The one place basis composes a [`RunOptions`], so the untyped and typed
    /// turns below cannot drift about what a turn carries — and so a test can
    /// read the same value a turn is about to be driven on.
    pub(crate) fn run_options(&self, options: TurnOptions) -> RunOptions {
        let mut run_options = options.into_run_options(self.retry_policy);
        // basis's own strategy goes in front of the host's (ADR-0042): when
        // the model ends its turn with delegations still running in the
        // background, they are joined first, and their results reach it
        // before its answer is final. Only a session with a ledger row keeps
        // such delegations, so only one gets the strategy.
        if let Some(row) = &self.agent_row {
            run_options.round_strategy = Some(Arc::new(crate::tools::spawn::JoinAtEnd::new(
                Arc::clone(row.registry()),
                row.agent_id(),
                run_options.round_strategy.take(),
            )));
        }
        run_options
    }

    /// Sets what every turn on this run may spend.
    ///
    /// [`Workspace`](crate::Workspace) installs [`RunSpec`](crate::RunSpec)'s
    /// bounds here at mint; a host that built its own session says so itself.
    /// Limits and retry defaults are inherited. Cancellation, graceful stop,
    /// and round strategy belong to one call and arrive through
    /// [`send_with_options`](Self::send_with_options) instead.
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
    /// registrations on the runtime, and its MCP connections. A
    /// caller that drops the workspace at mint and drives the run afterwards
    /// runs every turn with the workspace's hooks silently unenforced —
    /// dropping a live registration is what deregisters it, which is correct
    /// for a retired workspace and catastrophic for one that was
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

    /// The header line this run will open with, before anything is sent.
    pub fn header(&self) -> Event {
        self.header_now(&self.session.id().to_string())
    }

    /// Sends the configured prompt, streaming into `sink` and putting every
    /// consequential call to `approver`.
    ///
    /// Pass [`AllowAll`](crate::AllowAll) for a run that has no approver of its
    /// own. The stream
    /// always opens with [`Event::RunStarted`] and always closes with
    /// [`Event::RunFinished`], including when the turn fails: by then the
    /// stream has content a client needs to be able to finish reading.
    ///
    /// The approver runs on the forwarding task while the turn is blocked
    /// waiting on it, which is what makes an interactive answer possible at
    /// all — and what means an approver must answer rather than defer. One that
    /// cannot answer denies; see [`Approver`].
    ///
    /// The session survives, so this can be called again — see
    /// [`send_with_options`](Self::send_with_options) for a turn with a
    /// different prompt.
    pub async fn execute_with_approver<S: EventSink, A: Approver>(
        &mut self,
        sink: S,
        approver: A,
    ) -> Result<RunReport<S>, RunError> {
        self.execute_with_approver_and_options(sink, approver, TurnOptions::default())
            .await
    }

    /// Sends the configured prompt with both an approver and explicit run
    /// options — a cancellation token, a deadline, a tool budget.
    ///
    /// The one-shot path is bounded by its config but had no way to be
    /// *stopped*: a token belongs to one call, so it cannot travel in a config
    /// that mints many. This is where it arrives, and it is what a host driving
    /// a one-prompt run behind a UI needs, exactly as
    /// [`send_with_options`](Self::send_with_options) serves a conversation.
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

    /// Sends a prompt that is not only text — a screenshot, a diagram, a photo
    /// of a whiteboard — on the same conversation.
    ///
    /// Additive to [`send_with_options`](Self::send_with_options) rather than
    /// replacing it, because the overwhelming majority of turns are a line of
    /// text and should not have to build a vector to say so. `send_with_options`
    /// is this with one [`PromptPart::Text`].
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

    /// Whether this conversation ends inside a turn a crash interrupted after
    /// the model had acted on it: tool calls ran, and the model has not
    /// answered since.
    ///
    /// Read off a resumed run before sending anything. Recovery closed any
    /// call that was still running with an error result saying its effect is
    /// unknown — it is never run again by itself — so the conversation is
    /// valid as it stands, and the turn is finished with
    /// [`continue_interrupted_with_options`](Self::continue_interrupted_with_options)
    /// rather than by asking its prompt a second time. A turn the crash cut
    /// off before the model committed anything is not one of these: it rolled
    /// back, and asking its prompt again is the way to re-drive it.
    pub fn has_interrupted_turn(&self) -> bool {
        self.session.has_interrupted_turn()
    }

    /// Finishes an interrupted turn (see
    /// [`has_interrupted_turn`](Self::has_interrupted_turn)) from where the
    /// conversation ends, without sending a prompt: the model is asked again
    /// with what the turn already did, and the turn runs on exactly as one
    /// [`send_with_options`](Self::send_with_options) starts — the same
    /// header, forwarded events, approver, hooks, bounds and report.
    ///
    /// A conversation with no interrupted turn fails the turn with mentra's
    /// `NoInterruptedTurn`, reported like any failed turn.
    pub async fn continue_interrupted_with_options<S: EventSink, A: Approver>(
        &mut self,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        self.turn(TurnInput::Continue, sink, approver, options)
            .await
    }

    /// One turn, start to finish: header, forwarded events, outcome.
    async fn turn<S: EventSink, A: Approver>(
        &mut self,
        input: impl Into<TurnInput>,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<RunReport<S>, RunError> {
        let input = input.into();
        let options = bounded(options, &self.bounds);
        drawable(&options)?;
        let turn = match &input {
            TurnInput::Prompt(parts) => self.begin(parts, sink, approver)?,
            TurnInput::Continue => self.open_turn(sink, approver)?,
        };
        let (usage, usage_tap) = self.observe_usage();

        // Kept rather than passed straight in: mentra takes the options by
        // value, and the clone is how the run's own account of why it ended
        // gets back here. See [`ended_on`].
        let run_options = self.run_options(options);
        let observed = run_options.clone();

        let result = match input {
            TurnInput::Prompt(parts) => {
                self.session
                    .append_turn_with_options(prompt::into_blocks(parts), run_options)
                    .await
            }
            TurnInput::Continue => self.session.continue_interrupted(run_options).await,
        };
        let usage = Self::finish_usage(usage, usage_tap);

        let ended = match &result {
            Ok(message) => Ended::Answered(Some(message.text())),
            Err(error) => Ended::Failed(error),
        };
        let mut report = self.finish(turn, ended, &observed, usage).await?;
        report.failure = result.err();

        Ok(report)
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
        self.open_turn(sink, approver)
    }

    /// [`begin`](Self::begin) past the prompt check: emits the header and
    /// starts forwarding. A continuation opens here directly — it sends no
    /// prompt, so there is none to check.
    fn open_turn<S: EventSink, A: Approver>(
        &self,
        sink: S,
        approver: A,
    ) -> Result<Turn<S>, RunError> {
        let permissions = self.session.permission_handle();
        let session_id = self.session.id().to_string();
        let receiver = self.session.subscribe();

        let mut sink = sink;
        sink.emit(self.header_now(&session_id))?;

        let (done, done_rx) = oneshot::channel();
        let forwarder = tokio::spawn(async move {
            forward_events(receiver, sink, done_rx, approver, permissions).await
        });

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
        usage: RunUsage,
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
        let mut sink = forwarder.await?;

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
                // The same question gets the same answer as on `Answered`,
                // from the same place: this turn *completed*, so whether an
                // allowance ended it is the run's own record to give
                // ([`ended_on`]) and not a `None` this arm invents. A hardcoded
                // answer here would be the second opinion these three arms
                // exist to prevent.
                ended_on(observed, None),
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
            failure: None,
            stopped_by,
            usage,
            sink,
        })
    }
}

/// What a turn sends: a prompt, or nothing at all because it finishes an
/// interrupted one.
enum TurnInput {
    Prompt(Vec<PromptPart>),
    Continue,
}

impl From<Vec<PromptPart>> for TurnInput {
    fn from(parts: Vec<PromptPart>) -> Self {
        Self::Prompt(parts)
    }
}

/// A turn in flight: the forwarding task, and the signal that ends it.
struct Turn<S> {
    session_id: String,
    done: oneshot::Sender<()>,
    forwarder: tokio::task::JoinHandle<S>,
}
