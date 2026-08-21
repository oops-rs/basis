//! What a workspace's agent forgets, and when.
//!
//! mentra shrinks an agent's history in two unrelated ways, and only one of
//! them is what the word *compaction* suggests.
//!
//! **Micro-compaction** runs on every provider request
//! (`Agent::micro_compacted_history`, reached from mentra's turn runner before
//! each call). It walks the tool results in the transcript and replaces the
//! content of every one past the most recent `keep_recent_tool_results` with
//! `[Previous: used <tool>]`, for any result over 100 bytes. There is no token
//! budget in that decision and no event when it fires: it happens on the fourth
//! tool call of a conversation as readily as on the four-hundredth, on a
//! 1M-token model as readily as on a small one. mentra's default is **3**, so a
//! coding agent that reads five files and then edits one edits from a
//! transcript where the first two reads are blank.
//!
//! basis's default is to keep every tool result. A harness that silently blanks
//! what the model just read is worse at the job than one that does not, and the
//! cost of keeping them is paid in tokens the host can see and price. Elision is
//! therefore an opt-in a host asks for **by number**
//! ([`with_keep_recent_tool_results`](Compaction::with_keep_recent_tool_results)),
//! which is exactly how mentra models the off switch it already has:
//! `keep_recent == usize::MAX` returns the history untouched.
//!
//! **Real compaction** is the summarizing pass: the transcript is written to a
//! snapshot file, an older prefix of it is replaced by a model-written summary,
//! and the recent tail is preserved. It fires when the estimated request size
//! crosses `auto_compact_threshold_tokens`, and it does announce itself. basis
//! keeps mentra's trigger unchanged, because choosing a better number needs the
//! model's context window and **nothing in basis or mentra knows it** — a
//! window-relative trigger is an upstream capability, not something to guess at
//! here (`docs/REDESIGN.md` §2 records the candidate).
//!
//! # What is not here
//!
//! mentra's `CompactionConfig` has nine fields. This exposes three: the two
//! triggers above and how much recent user text a summarizing pass must leave
//! alone. The rest — the summary's input and output caps, local versus remote
//! summarization, how many snapshots are kept — are defaults nobody has had a
//! reason to move, and a knob basis offers is a knob basis has to keep
//! meaning. They arrive when a host asks, with the case that asked.
//!
//! `transcript_dir` is deliberately not a knob at all: where a workspace's
//! files go is settled by
//! [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir),
//! and a second answer here could disagree with it — so the conversion to
//! mentra's config takes the directory as a parameter rather than reading a
//! field.

use std::path::PathBuf;

/// How much of a conversation reaches the model, for every run a workspace
/// mints.
///
/// Set with [`WorkspaceBuilder::with_compaction`](crate::WorkspaceBuilder::with_compaction);
/// unset, `Compaction::default()` applies. `with_*` returns a new value, so a
/// host can keep one of these beside its other defaults and finish it
/// differently per workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compaction {
    /// How many of the most recent tool results keep their content on the way
    /// to the provider. `None` — basis's default — keeps every one of them.
    keep_recent_tool_results: Option<usize>,
    /// The estimated request size that triggers a summarizing pass. `None`
    /// never triggers one.
    auto_threshold_tokens: Option<usize>,
    /// How much recent user text a summarizing pass must leave verbatim.
    preserve_recent_user_tokens: usize,
}

/// Keeps every tool result, and leaves both of mentra's summarizing numbers
/// where mentra put them.
///
/// The two it leaves alone are *read off* mentra's own default rather than
/// copied into a constant here. basis has no basis for choosing either — the
/// trigger needs a context window nothing in this process knows, and the
/// preserved-user-text budget is a property of how mentra summarizes — so
/// restating them as literals would mean maintaining a second opinion that can
/// silently drift from the first.
impl Default for Compaction {
    fn default() -> Self {
        let mentra = mentra::agent::CompactionConfig::default();

        Self {
            // The one number basis does choose, and it chooses none: see this
            // module's header for why a harness should not blank what the
            // model just read.
            keep_recent_tool_results: None,
            auto_threshold_tokens: mentra.auto_compact_threshold_tokens,
            preserve_recent_user_tokens: mentra.preserve_recent_user_tokens,
        }
    }
}

impl Compaction {
    /// Elides all but the `keep` most recent tool results from every provider
    /// request, or keeps all of them with `None`.
    ///
    /// A host asks for this by number when it knows something basis does not:
    /// that its tool results are large, repetitive, and cheaply re-derived, and
    /// that the tokens are worth more than the transcript. What it costs is
    /// stated plainly — the model stops being able to see results it was shown,
    /// with no event marking the moment, and any turn that depends on comparing
    /// an early result against a late one becomes unreliable.
    ///
    /// `Some(0)` elides every tool result over 100 bytes, including the one
    /// that just came back.
    pub fn with_keep_recent_tool_results(self, keep: Option<usize>) -> Self {
        Self {
            keep_recent_tool_results: keep,
            ..self
        }
    }

    /// Triggers a summarizing pass once an estimated request reaches `tokens`,
    /// or never with `None`.
    ///
    /// The estimate is mentra's, over the serialized messages plus the system
    /// prompt, and it is not the provider's accounting. A host that knows its
    /// model's context window is the only party in a position to set this
    /// meaningfully; basis does not know it and so does not move mentra's
    /// number.
    ///
    /// `None` is a real posture, not a way of saying *later*: nothing then
    /// shortens the history, and a conversation that outgrows the window fails
    /// at the provider instead. It suits a run that is bounded by construction
    /// — one prompt, a handful of turns — where a summarizing pass would only
    /// ever cost a model call it did not need.
    pub fn with_auto_threshold_tokens(self, tokens: Option<usize>) -> Self {
        Self {
            auto_threshold_tokens: tokens,
            ..self
        }
    }

    /// Keeps at least `tokens` of the most recent user turns verbatim through a
    /// summarizing pass.
    ///
    /// What the user actually asked for is the thing a summary is least able to
    /// stand in for, which is why mentra protects a budget of it and why this
    /// is the third knob rather than one of the six that stayed behind.
    pub fn with_preserve_recent_user_tokens(self, tokens: usize) -> Self {
        Self {
            preserve_recent_user_tokens: tokens,
            ..self
        }
    }

    /// The mentra config this describes, filed under `transcript_dir`.
    ///
    /// The directory is a parameter rather than a field because the caller that
    /// knows it is not the caller that configures this: snapshots belong beside
    /// the history store, and which directory that is was settled on the
    /// [`Runtime`](crate::Runtime) (ADR-0018). Taking it here is what keeps
    /// there from being two answers.
    ///
    /// Every field this does not name stays at mentra's default, which is the
    /// point of the three-knob surface: basis states what it has a reason to
    /// state and inherits the rest, so an upstream improvement to summarization
    /// arrives rather than being shadowed by a copy of the old numbers.
    pub(crate) fn into_mentra(self, transcript_dir: PathBuf) -> mentra::agent::CompactionConfig {
        mentra::agent::CompactionConfig {
            // mentra's documented off switch (`memory::compaction::
            // micro_compact_history` returns the history untouched at
            // `usize::MAX`), which is what makes "keep everything" a
            // configuration of upstream rather than a fork of it.
            keep_recent_tool_results: self.keep_recent_tool_results.unwrap_or(usize::MAX),
            auto_compact_threshold_tokens: self.auto_threshold_tokens,
            preserve_recent_user_tokens: self.preserve_recent_user_tokens,
            transcript_dir,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
