//! The session underneath the run, and everything read or set through it.
//!
//! Split from [`prepared`](super) for the parent's size, along the seam that
//! was already there: every method here is a proxy onto mentra's `Session` —
//! handing it out whole, reading its transcript, or changing what the next
//! turn will run on. Nothing here opens a turn, so nothing here has to agree
//! with anything about how one is announced; the entry points next door are
//! what compose these into a run.

use mentra::{Session, agent::AgentEvent};

use super::{
    AgentEventTapGuard, Effort, ModelInfo, PreparedRun, ReasoningOptions, Role, RunContext,
    RunError, history_text,
};

impl PreparedRun {
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

    /// Replaces the tool authorizer for *this session only*, over the one its
    /// runtime installed.
    ///
    /// The runtime's authorizer is basis's own
    /// [`ApprovalGate`](crate::ApprovalGate), which answers nothing: every
    /// consequential call comes back as a `Prompt`, and a `Prompt` is resolved
    /// by a remembered rule before the run's [`Approver`](crate::Approver) is
    /// ever consulted. That order is mentra's and it is deliberate — but it
    /// means a policy the *approver* enforces can be pre-empted by a durable
    /// rule someone seeded through
    /// [`session()`](Self::session)`.permission_handle()`. An authorizer's own
    /// `Allow` and `Deny` are terminal instead: mentra returns them unchanged,
    /// consulting no rule and emitting no permission request. A host with a
    /// posture that must not be answerable by a remembered rule puts it here
    /// rather than in its approver.
    ///
    /// Scoped to this session's agent and the descendants it spawns after this
    /// call. Sibling sessions keep the runtime's. **Live-only**: mentra does
    /// not persist the attachment, so a conversation picked back up with
    /// [`Workspace::resume`](crate::Workspace::resume) arrives without it and
    /// must be given it again — the reason `basis-acp` installs its mode gate
    /// in `AcpSession::new`, which both `session/new` and `session/load` go
    /// through.
    ///
    /// Install **before the first turn**, which is what taking the run by
    /// value enforces at the type level: a run already lent to a turn cannot
    /// be moved. A subagent template taken from the session earlier, and any
    /// child already spawned, keep the authorizer they inherited.
    ///
    /// The replacement is total, not layered — mentra has no way to hand back
    /// the runtime authorizer being displaced, so an implementation here owns
    /// the whole question, `ApprovalGate`'s
    /// [`is_consequential`](crate::approval::is_consequential) filter included.
    /// One that answered `Prompt` for a read would put every read to whoever
    /// is approving.
    pub fn with_tool_authorizer<A>(self, authorizer: A) -> Self
    where
        A: crate::approval::ToolAuthorizer + 'static,
    {
        // Destructured rather than updated in place: mentra's `Session` is
        // consumed by its own `with_tool_authorizer`, so there is no borrow of
        // this run that could hold the rest of it together across the move.
        let Self {
            session,
            run,
            bounds,
            workspace,
            agent_row,
            retry_policy,
            context_snapshot,
        } = self;

        Self {
            session: session.with_tool_authorizer(authorizer),
            run,
            bounds,
            workspace,
            agent_row,
            retry_policy,
            context_snapshot,
        }
    }

    /// Forgets every "…for this session" approval answer this conversation
    /// holds, now instead of at its next attach.
    ///
    /// The documented duration of a
    /// [`…ForSession`](crate::ApprovalDecision::AllowForSession) answer is the
    /// live session: it dies when the conversation is next attached
    /// ([`Workspace::resume`](crate::Workspace) clears it). That boundary
    /// never arrives for a conversation nobody resumes — a one-shot run's,
    /// say — whose answers would otherwise sit in the runtime store's
    /// `rules.json` indefinitely, reasons included. A host whose run ends
    /// with the process calls this when the conversation is done; the
    /// answers' effect is unchanged either way, because nothing consults
    /// them between the last run and the attach that would have cleared
    /// them. Project- and global-scope rules are durable by definition and
    /// stay.
    ///
    /// Best-effort by construction at the call site, not in the signature:
    /// this returns the store's own error so the caller decides whether
    /// cleanup failure may mask the run's result — the CLI warns and exits
    /// with the run's code.
    pub fn forget_session_answers(&self) -> Result<(), RunError> {
        self.session
            .permission_handle()
            .clear_scope(mentra::session::PermissionRuleScope::Session)?;
        Ok(())
    }

    /// Gives the session back, ending this run's involvement.
    ///
    /// A workspace this run was keeping alive
    /// ([`with_workspace`](Self::with_workspace)) is dropped here with the
    /// rest of the run, and its hooks and MCP connections end with it.
    ///
    /// **"Mentra's alone" is not quite what comes back, and the difference
    /// matters.** The session keeps the tool audience it was minted in, so
    /// while any open of that directory is alive its calls are still composed
    /// into that open's interception chain and still judged by
    /// [`ForeignToolGuard`](crate::runtime::agents::ForeignToolGuard). What
    /// keeps that honest is the agent ledger row, which is held by the
    /// workspace as well as by this run — so handing the session back does not
    /// make it unattributable, which for a bridged name would mean *allowed*
    /// (`docs/proposals/0004`).
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
    /// **A floor, not the real number.** Mentra may add a task-reminder banner
    /// and a skill-description block on top of the system prompt Basis
    /// configured — additions that are Mentra-private, so that gap is not
    /// basis's to close. A resumed run is estimated against the prompt the
    /// *persisted* agent carries, read back off the resumed session, rather
    /// than the workspace's current default: the two differ whenever the
    /// original run carried a profile override. A run built through
    /// [`prepare_with_session`](crate::prepare_with_session) knows no prompt at
    /// all — there is no workspace to ask — and its floor excludes one
    /// entirely. Useful beside
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
}
