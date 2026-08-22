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

use tokio::sync::broadcast::error::TryRecvError;

use super::{Event, EventSink, PreparedRun, RunError};

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
    pub async fn compact<S: EventSink>(
        &mut self,
        instructions: Option<&str>,
        sink: &mut S,
    ) -> Result<Option<Compacted>, RunError> {
        // Subscribed before the pass rather than after, because a broadcast
        // receiver only sees what is sent once it exists.
        let mut events = self.session.subscribe();
        let details = self.session.compact(instructions).await?;
        drain(&mut events, sink)?;

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

/// Empties whatever the pass put on the session's stream into the sink.
///
/// A lag is impossible in practice — a compaction emits two events into a
/// channel that holds 512 — so a receiver that reports one has been overtaken
/// by something this function cannot see, and stopping is the honest response:
/// the alternative is a stream that quietly disagrees with what happened.
fn drain<S: EventSink>(
    events: &mut mentra::SessionEventReceiver,
    sink: &mut S,
) -> Result<(), RunError> {
    loop {
        match events.try_recv() {
            // `from_session_event` is the same mapping a turn's forwarder
            // uses, so a compaction announced during a turn and one announced
            // on its own are the same two lines.
            Ok(event) => {
                if let Some(mapped) = Event::from_session_event(&event) {
                    sink.emit(mapped)?;
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Closed | TryRecvError::Lagged(_)) => {
                return Ok(());
            }
        }
    }
}
