//! Driving a session that is already built.
//!
//! Splitting this from [`run`](super::run) gives two things. A host that
//! already owns a mentra [`Runtime`](mentra::Runtime) — with its own provider,
//! store, or custom tools — can still use basis's context discovery and event
//! stream instead of reimplementing them. And basis's own tests can drive the
//! whole pipeline against a scripted runtime, so the event contract is checked
//! without a network call.

use std::sync::{Arc, Mutex, PoisonError};

use mentra::{
    Session,
    agent::AgentEvent,
    runtime::{ProviderRetry, RunOptions},
};
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
    event::{ContextFile, EVENT_SCHEMA_VERSION, Event, RunOutcome, SkillSummary, TemplateSummary},
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
    /// The provider retry fallback copied from the [`Runtime`](crate::Runtime)
    /// that minted it.
    ///
    /// A per-run field for a runtime-scoped knob because that is the shape
    /// mentra offers: the schedule rides on `RunOptions`, so a runtime's
    /// default has to be carried to each run rather than set once upstream.
    /// One [`TurnOptions`](crate::TurnOptions) may override it for its call.
    /// [`Workspace`](crate::Workspace) puts it here at mint; a caller on the
    /// [`prepare_with_session`](super::prepare_with_session) path built the
    /// mentra runtime itself and gets mentra's default, which is the only
    /// honest answer when basis was never told about a provider connection.
    provider_retry: ProviderRetry,
    /// The retry-count fallback carried from the same runtime and for the same
    /// reason. A turn may override it independently of the schedule.
    retry_budget: usize,
    /// What this run's mint knew about its system prompt; see
    /// [`estimated_context_tokens`](Self::estimated_context_tokens).
    ///
    /// Named apart from [`context()`](Self::context)'s own `run: RunContext`
    /// deliberately: the two answer different questions and a shared name
    /// would blur them at every call site.
    context_snapshot: ContextSnapshot,
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
            provider_retry: ProviderRetry::default(),
            retry_budget: RunOptions::default().retry_budget,
            context_snapshot: ContextSnapshot::default(),
        }
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
    /// A fresh prepare records the final per-run prompt Basis handed Mentra;
    /// resume records `None`, because Mentra 0.23 exposes no reader for the
    /// persisted agent config and its prompt may differ from the workspace's
    /// current default. [`prepare_with_session`](super::prepare_with_session),
    /// the path with no workspace, also leaves this unknown.
    pub(crate) fn with_context_snapshot(self, system_prompt: Option<String>) -> Self {
        Self {
            context_snapshot: ContextSnapshot::new(system_prompt),
            ..self
        }
    }

    /// Carries the minting runtime's provider retry fallbacks onto this run.
    ///
    /// Set by [`Workspace`](crate::Workspace) at mint, at the one place both
    /// `prepare` and `resume` go through. Per-turn overrides compose on top in
    /// [`run_options`](Self::run_options).
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
    /// in from this run's bounds by the caller, with the runtime's retry
    /// schedule and count as fallbacks.
    ///
    /// The one place basis composes a [`RunOptions`], so the untyped and typed
    /// turns below cannot drift about what a turn carries — and so a test can
    /// read the same value a turn is about to be driven on.
    pub(crate) fn run_options(&self, options: TurnOptions) -> RunOptions {
        options.into_run_options(self.provider_retry, self.retry_budget)
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

    /// The header line this run will open with, before anything is sent.
    pub fn header(&self) -> Event {
        header_for(&self.session.id().to_string(), &self.run)
    }

    /// The session this run drives, for a host that wants mentra's own surface
    /// — branching, the transcript tree, subagents — alongside basis's.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Registers a lossless in-process observer for this run's agent events.
    ///
    /// The callback receives Mentra's complete [`AgentEvent`] values unchanged,
    /// synchronously and in occurrence order, before they enter the bounded
    /// broadcast stream. In particular, tool calls and results retain their
    /// complete provider-neutral input, structured content, and error payloads.
    /// A cancellation ends the sequence with [`AgentEvent::RunFailed`].
    ///
    /// The callback executes inline on the operation emitting the event. It
    /// must return promptly and must not block or panic: blocking stalls that
    /// operation, and a panic propagates through it. It must not re-enter an
    /// event-emitting operation or drop a tap guard from inside the callback.
    ///
    /// Keep the returned [`AgentEventTapGuard`] alive for as long as observation
    /// is required. Dropping it waits for any invocation already in flight and
    /// then unregisters; do not drop it while holding a resource that callback
    /// needs. Registration does not replay earlier events.
    pub fn register_agent_event_tap(
        &self,
        tap: impl Fn(&AgentEvent) + Send + Sync + 'static,
    ) -> AgentEventTapGuard {
        AgentEventTapGuard::new(self.session.register_agent_event_tap(tap))
    }

    /// Mutably exposes Mentra's session.
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

    /// The persisted agent id: the handle
    /// [`resume`](crate::Workspace::resume) takes.
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

    /// This run's committed conversation as runtime roles plus assembled text.
    ///
    /// Narrower than [`history`](Self::history): only the two chat roles a host
    /// commonly needs, and only the assembled text each one said. Unknown roles
    /// stay in `history()` for callers that want the full transcript.
    pub fn text_history(&self) -> impl DoubleEndedIterator<Item = (Role, String)> + '_ {
        self.session.history().iter().filter_map(history_text)
    }

    /// How many of the assistant's turns this run's history has committed.
    ///
    /// The count, not the presence: a session resumed with `--continue` or
    /// `--session` arrives with answers already on it, and "has this run
    /// answered yet" is only a question a count can settle against a
    /// watermark taken earlier — one taken right after mint, before anything
    /// was asked, tells a caller recovering from a crash mid-turn whether the
    /// last committed message is the crashed turn's own answer or one it
    /// inherited.
    ///
    /// The fact [`history`](Self::history) alone does not expose: reading it
    /// off `history()` directly means matching on `mentra::Role`, which pulls
    /// a host into a dependency on mentra's own type for a question basis can
    /// just answer. This is the narrower of the two fixes — it settles
    /// exactly that one count rather than growing `history()`'s element type
    /// a role of basis's own, which a caller wanting the *text* of a message
    /// still would not need.
    pub fn answered_turns(&self) -> usize {
        self.session
            .history()
            .iter()
            .filter(|message| matches!(message.role, Role::Assistant))
            .count()
    }

    /// The newest assistant text this run's history has committed, if any.
    ///
    /// The text [`answered_turns`](Self::answered_turns) only counts. A host
    /// recovering from a crash mid-turn asks two questions in sequence — did
    /// the recorded prompt already get answered, and *what was the answer* —
    /// and both are questions about `mentra::Role` that basis can settle
    /// without pulling the caller into matching on it (ADR-0003: the
    /// in-process Rust consumer is what the API is judged by; `basis-tasks`
    /// carried a whole `mentra` dependency for this one match before this
    /// existed).
    ///
    /// `None` when nothing has answered yet, which a fresh run and a resumed
    /// conversation with no assistant turn both are. Owned rather than
    /// borrowed because the text is assembled from the message's parts, not
    /// stored as one string.
    pub fn last_assistant_text(&self) -> Option<String> {
        self.session
            .history()
            .iter()
            .rev()
            .find(|message| matches!(message.role, Role::Assistant))
            .map(|message| message.text())
    }

    /// This run's model's context window, when it is known.
    ///
    /// Read from the live session, so it is whatever mentra is compacting
    /// against right now. Known when the model was resolved through the
    /// provider's listing and that listing reports one — mentra looks a
    /// pinned id up there too (`bfe952b`), so `--model`, a repository's
    /// `config.json` and `WorkspaceBuilder::with_model` all get a window when the
    /// provider publishes one. Gemini's listing does, as `inputTokenLimit`;
    /// Anthropic's and the OpenAI wires' do not, and neither does a server
    /// that cannot list. `None` for a run whose lossy
    /// [`set_model`](Self::set_model) wrapper has since moved onto a model
    /// named by id alone; the complete
    /// [`set_resolved_model`](Self::set_resolved_model) preserves a supplied
    /// window. Also `None` for a resumed
    /// conversation that is no longer on the model its workspace resolved —
    /// mentra does not persist a window, and `Workspace::resume` reapplies
    /// the workspace's model only while the conversation is still on it.
    pub fn context_window(&self) -> Option<usize> {
        self.session.context_window()
    }

    /// Estimates how many tokens the next request would spend on this run's
    /// history and system prompt, using mentra's own estimator
    /// ([`mentra::memory::estimated_request_tokens`]) — the same one mentra's
    /// auto-compaction threshold is compared against.
    ///
    /// **A floor, not the real number.** On a freshly prepared workspace run,
    /// Mentra may add a task-reminder banner and a skill-description block on
    /// top of the system prompt Basis configured. On a resumed run the floor
    /// excludes the entire system prompt: Mentra 0.23 exposes no persisted
    /// `AgentConfig` reader, and substituting the current workspace default
    /// would be wrong when the original run carried a profile override. The
    /// effective prompt is private, so nothing outside Mentra can close either
    /// gap. Useful beside
    /// [`context_window`](Self::context_window) for a host deciding whether to
    /// compact or warn before mentra's own trigger would.
    pub fn estimated_context_tokens(&self) -> usize {
        self.context_snapshot
            .estimated_tokens(self.session.history())
    }

    /// What this run is about, minus the session.
    pub fn context(&self) -> &RunContext {
        &self.run
    }

    /// Switches the complete resolved model this conversation's later turns
    /// run on, keeping the provider it was opened with.
    ///
    /// Takes effect from the next turn: mentra threads the model into each
    /// model request as it builds it, so a turn already in flight finishes on
    /// the model it started with. It also persists — mentra rewrites the agent
    /// record — so a session resumed in another process comes back on the model
    /// it was last set to.
    ///
    /// The [`ModelInfo`] is handed to mentra unchanged, including its context
    /// window. It is not checked against a catalogue: listing would be provider
    /// activity at switch time, and host-resolved metadata is already the
    /// caller's contract. An id the provider rejects fails on the next turn,
    /// where that provider can say why. Mentra currently retains the id,
    /// provider, and context window but neither exposes nor persists the
    /// display name, description, or creation time, so those display fields
    /// cannot be read back through this API.
    ///
    /// The provider is *not* switchable here. Its identity must equal this
    /// run's provider exactly; a mismatch is refused before mentra, catalogue,
    /// model-request, or tool activity. A run built on one provider's
    /// credential and endpoint (ADR-0018) has no second connection to move to.
    ///
    /// This is a between-turn API. A [`RoundStrategy`](crate::RoundStrategy)
    /// may make its own in-turn model adjustment, but that does not pass
    /// through this method and does not rewrite basis's header/report context.
    ///
    /// Mentra's setter mutates its live agent before persisting the record. If
    /// that write fails, the returned error can therefore leave mentra's live
    /// session changed while basis's [`RunContext`] still names the old model.
    /// A host that needs failure-atomic phase switches should use volatile
    /// history; the Nous Gate 1a attached lifecycle does so.
    pub fn set_resolved_model(&mut self, model: ModelInfo) -> Result<(), RunError> {
        if model.provider.as_str() != self.run.provider {
            return Err(RunError::ResolvedModelProviderMismatch {
                model: model.id.clone(),
                model_provider: model.provider.as_str().to_string(),
                runtime_provider: self.run.provider.clone(),
            });
        }

        let model_id = model.id.clone();
        self.session.set_model(model)?;
        // The context is what `header()` and every report read the model from.
        // Leaving it stale would have the stream describe a run that is no
        // longer happening. This assignment deliberately follows the session
        // call, so a rejected switch cannot rewrite the public header/report.
        self.run.model = model_id;

        Ok(())
    }

    /// Switches later turns to `model` by id, keeping the current provider.
    ///
    /// Compatibility wrapper over [`set_resolved_model`](Self::set_resolved_model).
    /// It is deliberately lossy: an id carries no context window or display
    /// metadata, so [`context_window`](Self::context_window) becomes `None`.
    /// Hosts that already resolved a [`ModelInfo`] should use the complete API.
    pub fn set_model(&mut self, model: impl Into<String>) -> Result<(), RunError> {
        let provider = self.run.provider.clone();
        self.set_resolved_model(ModelInfo::new(model, provider))
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

    /// Sets complete provider-neutral reasoning options from the next turn on.
    ///
    /// `None` clears the request and restores the provider's own default. The
    /// options are forwarded whole without rewriting any other
    /// [`ProviderRequestOptions`](crate::ProviderRequestOptions) field. They
    /// are provider-neutral: adapters that support summaries map
    /// [`ReasoningOptions::summary`] to their wire, while adapters without a
    /// summary control ignore that field.
    ///
    /// Persisted and deferred exactly as
    /// [`set_resolved_model`](Self::set_resolved_model) is. Mentra likewise
    /// mutates the live reasoning posture before its persistence write, so an
    /// error can leave that live value changed; the Nous Gate 1a lifecycle
    /// avoids that persistence edge with volatile history.
    pub fn set_reasoning(&mut self, reasoning: Option<ReasoningOptions>) -> Result<(), RunError> {
        self.session.set_reasoning(reasoning)?;

        Ok(())
    }

    /// The complete reasoning options this session's next turn will receive.
    pub fn reasoning(&self) -> Option<&ReasoningOptions> {
        self.session.reasoning()
    }

    /// Asks the model for one effort level from the next turn on.
    ///
    /// Compatibility wrapper over [`set_reasoning`](Self::set_reasoning).
    /// It is deliberately lossy: changing effort through this method clears
    /// any configured reasoning summary. A caller preserving complete options
    /// should use [`set_reasoning`](Self::set_reasoning).
    pub fn set_effort(&mut self, effort: Option<Effort>) -> Result<(), RunError> {
        self.set_reasoning(effort.map(|effort| ReasoningOptions {
            effort: Some(effort.into()),
            summary: None,
        }))
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
        self.reasoning()
            .and_then(|reasoning| reasoning.effort)
            .and_then(|effort| Effort::try_from(effort).ok())
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
        let (usage, usage_tap) = self.observe_usage();

        // Kept rather than passed straight in: mentra takes the options by
        // value, and the clone is how the run's own account of why it ended
        // gets back here. See [`ended_on`].
        let run_options = self.run_options(options);
        let observed = run_options.clone();

        let result = self
            .session
            .append_turn_with_options(prompt::into_blocks(parts), run_options)
            .await;
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

        let permissions = self.session.permission_handle();
        let session_id = self.session.id().to_string();
        let receiver = self.session.subscribe();

        let mut sink = sink;
        sink.emit(header_for(&session_id, &self.run))?;

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

/// A turn in flight: the forwarding task, and the signal that ends it.
struct Turn<S> {
    session_id: String,
    done: oneshot::Sender<()>,
    forwarder: tokio::task::JoinHandle<S>,
}
