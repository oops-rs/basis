//! Compacting a conversation because someone asked.
//!
//! mentra already compacts on a threshold, and the model can already ask for
//! it through the `compact` intrinsic. The person whose conversation it is
//! could not — which is the one case where the *instruction* matters, because
//! only they know that the migration plan is worth keeping and the log
//! spelunking is not.
//!
//! # Why the sink is drained rather than written to
//!
//! [`PreparedRun::compact`] answers twice: the return value goes to the
//! caller, and the events go to whoever is reading the stream. Both matter —
//! a sink that watched the history shrink with nothing to explain it would be
//! describing a conversation nobody could account for.
//!
//! The events are mentra's own, not basis's re-derivation of them.
//! `Session::compact` installs the same agent-event tap a turn installs, for
//! the duration of the pass, so `CompactionStarted` and `CompactionCompleted`
//! reach the session's stream exactly as they do when a threshold fires. What
//! is missing outside a turn is only the *other* half: the forwarder that
//! carries that stream into a run's sink runs per turn, and this is not a
//! turn. So this subscribes first and drains after, which is what makes an
//! on-demand pass indistinguishable from an automatic one on the wire.
//!
//! Draining after rather than forwarding concurrently is enough because the
//! pass emits at its end — mentra applies the compaction and *then* announces
//! it — and because the two events cannot outrun a broadcast channel that
//! holds 512. A turn needs the concurrent forwarder for a different reason:
//! it has to answer permission requests while the turn is blocked on them,
//! and a summarizing pass asks for nothing.
//!
//! # Why a failed pass is announced too
//!
//! The same argument, read the other way. A pass that fails is a model call
//! that happened and cost something, and the conversation a client is
//! watching does not shrink — so a stream that says nothing leaves that
//! client with a `/compact` that produced no observable effect and no reason.
//! mentra does not fill that in: `Session::compact` hands the error back and,
//! unlike `finish_turn`, puts no `SessionEvent::Error` on the stream for it.
//!
//! So basis emits one [`Event::Error`], from the error it is already holding,
//! and only when the drain forwarded none. This is not a workaround for a
//! runtime hole (ADR-0005) — there is nothing hidden upstream to recover here.
//! It is basis's own verb accounting for its own outcome, exactly as the
//! success path does, and the guard means an upstream event would take
//! precedence rather than double up.
//!
//! # And why a bounded pass is announced the same way
//!
//! [`PreparedRun::compact_with_options`] can end a pass on a cancellation
//! token or a deadline, and mentra reports both as what they are —
//! `RuntimeError::Cancelled`, `RuntimeError::DeadlineExceeded` — rather than
//! as a summarizer that refused. The caller therefore has the bound in the
//! type it matches on, which is the half that matters for control flow.
//!
//! The stream gets the same single [`Event::Error`] a failure gets, and that
//! is deliberate rather than an omission. The alternative is silence, and
//! silence is what the failure path already rejected: a client that asked for
//! a conversation to shrink and watched nothing happen is owed a line either
//! way. What a client needs is to tell the two apart, and it can — the
//! message is mentra's own (`operation cancelled`, `deadline exceeded`, and
//! not the summarizer's complaint), and `recoverable` is false for both
//! because mentra classifies a bound as terminal, so nothing here invites a
//! retry into a stop somebody asked for. basis has no `Bound` to report on
//! this verb the way [`RunReport`](crate::RunReport) does for a turn, because
//! there is no report: a pass returns a value, not a run.

use mentra::{compaction::CompactionBounds, runtime::is_transient_runtime_error};
use tokio::sync::broadcast::error::TryRecvError;

use super::{Event, EventSink, PreparedRun, RunError, TurnOptions, bounded};

/// What a summarizing pass did.
///
/// basis's own shape rather than mentra's `CompactionDetails`, for the same
/// reason [`Event`] is: what basis publishes should not move because a runtime
/// internal did. The field names are [`Event::CompactionCompleted`]'s, because
/// a caller reading the return value and a client reading the stream are
/// looking at one pass and should not have to learn two vocabularies for it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Compacted {
    /// How many transcript items the summary stands in for.
    pub replaced_items: usize,
    /// How many were kept verbatim: the continuation tail, and whatever recent
    /// user text mentra's `preserve_recent_user_tokens` protected.
    pub preserved_items: usize,
    /// What the transcript is now — the length the next turn will send.
    pub transcript_len: usize,
    /// How many concrete facts — file paths, command outcomes — the pass
    /// pulled out of the discarded prefix to carry forward by name.
    pub extracted_facts: usize,
    /// The opening of the summary the model wrote, for a client with a line to
    /// put it on. Not the summary: that is in the transcript.
    pub summary_preview: String,
}

impl PreparedRun {
    /// Compacts this conversation now, without waiting for a threshold.
    ///
    /// `instructions` says what to keep — "hold on to the migration plan, drop
    /// the log spelunking" — and is **added** to the standing continuity
    /// requirements rather than replacing them, so asking for one extra thing
    /// cannot cost a caller the file paths and command outcomes every summary
    /// needs. `None` asks for the standing ones alone.
    ///
    /// This is a model call: the summary is written by the same provider the
    /// conversation runs on, and it is billed and can fail like any other
    /// request. It is not a turn, though — no prompt is committed, the
    /// transcript gains no exchange, and nothing is sent afterwards.
    ///
    /// Billed by the provider, but *not* accounted by basis. mentra reports no
    /// usage for a summarizing call — the engine reads the response's content
    /// and drops its `usage` (mentra 0.24.0 `src/compaction.rs`
    /// `summarize_locally`; the provider-native remote path has no usage on
    /// its response type at all) — so no `UsageReport` is emitted, and what
    /// basis tallies is what mentra reports. A pass therefore adds nothing to
    /// any [`RunReport::usage`](crate::RunReport) and is not charged against
    /// [`Bounds::token_budget`](crate::Bounds) or a
    /// [`BudgetPool`](crate::BudgetPool). Basis does not estimate the
    /// difference: a made-up number in a field documented as *reported*
    /// (see [`RunUsage`](crate::RunUsage)) would be worse than a known gap,
    /// and the gap is upstream's to close (ADR-0005).
    ///
    /// A failure reaches the sink as one [`Event::Error`](crate::Event) before
    /// it reaches the caller as an `Err`, so a client watching the stream is
    /// told why a conversation it expected to shrink did not. The transcript
    /// is untouched in that case and the next turn goes out on the history
    /// this pass did not shorten.
    ///
    /// `Ok(None)` means there was nothing to compact, which is the honest
    /// answer for a conversation that has not spoken yet: the last turn is
    /// always preserved whole, exactly as it is for the model's own `compact`
    /// intrinsic, so a session with only that has no older prefix to summarize.
    /// Nothing is emitted in that case either — a lone "compacting…" on a
    /// client's stream, with no second line, is worse than silence.
    ///
    /// The sink is borrowed rather than taken. Every other verb on a run hands
    /// its sink back inside a report, and there is no report here: two events
    /// and a value are the whole of what happened.
    ///
    /// Bounded by whatever the run itself was configured with
    /// ([`bounds`](Self::bounds)) and by nothing else. For a pass a caller
    /// can take back — a stop button, a deadline of its own — see
    /// [`compact_with_options`](Self::compact_with_options), which this is.
    pub async fn compact<S: EventSink>(
        &mut self,
        instructions: Option<&str>,
        sink: &mut S,
    ) -> Result<Option<Compacted>, RunError> {
        self.compact_with_options(instructions, sink, TurnOptions::default())
            .await
    }

    /// Compacts this conversation now, under bounds the caller can trip.
    ///
    /// [`compact`](Self::compact) with a way to stop it. A summarizing pass is
    /// a full provider round trip over the longest transcript this
    /// conversation has ever had — which is exactly the moment a person
    /// reaches for stop — and until mentra 0.24 it ran to completion whatever
    /// the caller did afterwards. `options`' cancellation token and deadline
    /// are the two that apply, so a host driving `/compact` from a UI gets the
    /// same cancel it already has over a turn.
    ///
    /// Only those two, deliberately, and the choice is mentra's own
    /// (`CompactionBounds::from_run_options`): a graceful
    /// [`stop`](crate::TurnOptions::stop) ends a *turn* at its next round
    /// boundary with everything committed, and abandoning a summary half-way
    /// is not that. The tool and token budgets do not apply either — mentra
    /// reports no usage for a summarizing call, so there is nothing for them
    /// to measure (see [`compact`](Self::compact)) — and a spent
    /// [`BudgetPool`](crate::BudgetPool) does not refuse a pass for the same
    /// reason.
    ///
    /// A bound left unset falls back to the run's own, exactly as it does for
    /// [`send_with_options`](Self::send_with_options): attaching a token says
    /// something about stopping, not about limits.
    ///
    /// Reaching a bound leaves the transcript exactly as it was — mentra
    /// checks before it issues the request and again while it is in flight,
    /// and applies nothing it did not finish — and reports as the bound rather
    /// than as a refused summarizer: the caller gets
    /// [`RunError::Runtime`](crate::RunError::Runtime) carrying
    /// `RuntimeError::Cancelled` or `RuntimeError::DeadlineExceeded`, and the
    /// stream gets one [`Event::Error`](crate::Event) reading `operation
    /// cancelled` or `deadline exceeded` with `recoverable` false. That is the
    /// same announcement a failed pass gets and it is the right one: a client
    /// that expected the conversation to shrink is owed a line saying it did
    /// not, and the two cases are told apart by what the line *says* — mentra
    /// classifies both bounds as terminal, so `recoverable` never invites a
    /// retry into a stop somebody asked for.
    pub async fn compact_with_options<S: EventSink>(
        &mut self,
        instructions: Option<&str>,
        sink: &mut S,
        options: TurnOptions,
    ) -> Result<Option<Compacted>, RunError> {
        // Resolved through the turn's own merge and its own conversion, so the
        // deadline a compaction honors is the one a turn would have honored:
        // an absolute instant wins over a relative one, and there is no second
        // opinion here about which.
        let bounds =
            CompactionBounds::from_run_options(&self.run_options(bounded(options, &self.bounds)));

        // Subscribed before the pass rather than after, because a broadcast
        // receiver only sees what is sent once it exists.
        let mut events = self.session.subscribe();
        let result = self.session.compact_with_bounds(instructions, bounds).await;
        let announced = drain(&mut events, sink)?;

        let details = match result {
            Ok(details) => details,
            Err(error) => {
                if !announced {
                    sink.emit(Event::Error {
                        // The predicate mentra's own `finish_turn` applies to
                        // a failed turn, so the field means one thing across
                        // the stream: whether waiting and trying again is what
                        // the failure calls for, not whether the conversation
                        // survived. It did — a refused pass leaves the
                        // transcript exactly as it was, and the next turn goes
                        // out on the history this one did not shorten.
                        recoverable: is_transient_runtime_error(&error),
                        message: error.to_string(),
                    })?;
                }
                return Err(error.into());
            }
        };

        let Some(details) = details else {
            return Ok(None);
        };

        let compacted = Compacted {
            replaced_items: details.replaced_items,
            preserved_items: details.preserved_items,
            transcript_len: details.resulting_transcript_len,
            extracted_facts: details.extracted_facts_count,
            summary_preview: details.summary_preview,
        };

        Ok(Some(compacted))
    }
}

/// Empties whatever the pass put on the session's stream into the sink, and
/// reports whether any of it already announced a failure.
///
/// The answer is what keeps [`PreparedRun::compact`]'s own error event from
/// becoming a duplicate. mentra says nothing on the stream when a summarizing
/// pass fails — `Session::compact` returns the error and, unlike
/// `finish_turn`, emits no `SessionEvent::Error` for it — so today this is
/// always `false` on the failing path and basis speaks. If mentra grows that
/// event, its line is the one the client gets and basis stays quiet, which is
/// the right precedence: the runtime's own account of its own failure.
///
/// A lag is impossible in practice — a compaction emits two events into a
/// channel that holds 512 — so a receiver that reports one has been overtaken
/// by something this function cannot see, and stopping is the honest response:
/// the alternative is a stream that quietly disagrees with what happened.
fn drain<S: EventSink>(
    events: &mut mentra::SessionEventReceiver,
    sink: &mut S,
) -> Result<bool, RunError> {
    let mut announced_failure = false;
    loop {
        match events.try_recv() {
            // `from_session_event` is the same mapping a turn's forwarder
            // uses, so a compaction announced during a turn and one announced
            // on its own are the same two lines.
            Ok(event) => {
                if let Some(mapped) = Event::from_session_event(&event) {
                    announced_failure |= matches!(mapped, Event::Error { .. });
                    sink.emit(mapped)?;
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Closed | TryRecvError::Lagged(_)) => {
                return Ok(announced_failure);
            }
        }
    }
}
