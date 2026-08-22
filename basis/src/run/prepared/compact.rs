//! Compacting a conversation because someone asked.
//!
//! mentra already compacts on a threshold, and the model can already ask for
//! it through the `compact` intrinsic. The person whose conversation it is
//! could not — which is the one case where the *instruction* matters, because
//! only they know that the migration plan is worth keeping and the log
//! spelunking is not.
//!
//! # Why basis emits the events itself
//!
//! mentra installs the tap that carries an agent event onto a session's event
//! stream inside `Session::begin_turn` and drops it in `finish_turn`
//! (`mentra/src/session/handle.rs`). `Session::compact` opens no turn, so the
//! `AgentEvent::ContextCompacted` it produces has no tap to travel on and
//! reaches no subscriber at all: a run's sink would see the history shrink
//! with nothing on the stream to explain it.
//!
//! So [`PreparedRun::compact`] answers both ways. The return value is the
//! answer to the caller, and the two events are the answer to whoever is
//! reading the stream — built here from what mentra returned, in the order and
//! with the fields mentra's own in-turn mapping uses (`session/mapping.rs`'s
//! `map_compaction`), so a client cannot tell an on-demand pass from an
//! automatic one. When that gap closes upstream this file emits nothing and
//! the events arrive on the stream like every other; until then, emitting
//! nothing would be the quieter failure.

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
        let Some(details) = self.session.compact(instructions).await? else {
            return Ok(None);
        };

        let compacted = Compacted {
            replaced_items: details.replaced_items,
            preserved_items: details.preserved_items,
            transcript_len: details.resulting_transcript_len,
            extracted_facts: details.extracted_facts_count,
            summary_preview: details.summary_preview,
        };

        sink.emit(Event::CompactionStarted {
            agent_id: details.agent_id.clone(),
        })?;
        sink.emit(Event::CompactionCompleted {
            agent_id: details.agent_id,
            replaced_items: compacted.replaced_items,
            preserved_items: compacted.preserved_items,
            transcript_len: compacted.transcript_len,
            extracted_facts: compacted.extracted_facts,
            summary_preview: compacted.summary_preview.clone(),
        })?;

        Ok(Some(compacted))
    }
}
