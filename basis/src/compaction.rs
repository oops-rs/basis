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
//! budget in that decision: it happens on the fourth tool call of a conversation
//! as readily as on the four-hundredth, on a 1M-token model as readily as on a
//! small one. Mentra 0.23 makes the request-only rewrite observable through
//! [`Event::RequestToolResultsElided`](crate::Event::RequestToolResultsElided),
//! without changing the canonical transcript. mentra's own default is
//! `usize::MAX` — keep everything — and basis agrees, so this is one knob
//! basis states only to make the same default explicit: elision is opt-in a
//! host asks for **by number**
//! ([`with_keep_recent_tool_results`](Compaction::with_keep_recent_tool_results)),
//! which is exactly how mentra models the off switch it already has:
//! `keep_recent == usize::MAX` returns the history untouched.
//!
//! **Real compaction** is the summarizing pass: the transcript is written to a
//! snapshot file, an older prefix of it is replaced by a model-written summary,
//! and the recent tail is preserved. It fires three ways, and it announces
//! itself the same way every time
//! ([`Event::CompactionStarted`](crate::Event::CompactionStarted) /
//! [`CompactionCompleted`](crate::Event::CompactionCompleted)):
//!
//! - The estimated request size crosses `auto_threshold_tokens` — an absolute
//!   token count, for a model whose context window basis does not know.
//! - Or, when the model's window *is* known
//!   ([`ModelInfo::context_window`](mentra::ModelInfo::context_window) —
//!   populated from Gemini's model listing, `None` for Anthropic and the
//!   Responses transport), it crosses `auto_threshold_percent` of that window
//!   instead, which wins over the absolute number whenever both apply. This is
//!   the knob that did not exist until mentra could ask the model itself how
//!   big it is: 50,000 tokens is most of a small model's window and a rounding
//!   error in a 1M-token one, so no single constant was ever going to be right
//!   for both. [`PreparedRun::context_window`](crate::PreparedRun::context_window)
//!   and
//!   [`estimated_context_tokens`](crate::PreparedRun::estimated_context_tokens)
//!   are how a host reads the same two figures for itself, ahead of whichever
//!   trigger fires first.
//!
//!   The two are one setting resolved together, not two triggers that fire
//!   independently: mentra reads a cleared `auto_threshold_tokens` as *off*
//!   before it consults the percentage at all
//!   (`CompactionConfig::auto_compact_threshold`), so the absolute number is
//!   the switch for both and the percentage only ever chooses *where* an
//!   already-armed trigger sits. basis said the opposite here until the
//!   behaviour was checked against upstream; the pair of tests named for it
//!   in this module's `tests` now pins each half.
//! - Or the provider refuses a request as too long
//!   (`ProviderError::ContextLengthExceeded`), regardless of either threshold —
//!   including with `auto_threshold_tokens` cleared. mentra compacts once and
//!   retries the same request; a second overflow after that is not retried
//!   again. So `with_auto_threshold_tokens(None)` still means basis never
//!   compacts *ahead of* running out of room, not that a conversation which
//!   does is guaranteed to fail — the provider's own refusal gets the one
//!   attempt basis's own trigger would have spent earlier.
//!
//! # What a failed automatic pass looks like
//!
//! Like nothing. Both events above are built from a *success*: mentra
//! synthesizes `CompactionStarted` and `CompactionCompleted` together from the
//! one `ContextCompacted` a finished pass emits, so there is no "started" line
//! to leave dangling — and no line of any kind when the pass does not finish.
//! An automatic pass that fails is retried up to three times and then dropped
//! (`Agent::auto_compact_if_needed`, mentra 0.23.4 `src/agent/compact.rs`),
//! which is a reasonable posture — the run continues on an unshortened
//! history rather than dying over a summary — but it is taken silently: no
//! event, no hook, nothing a sink can see. The retries in between surface as
//! [`Event::Retry`](crate::Event::Retry), indistinguishable from a model
//! request's own.
//!
//! basis does not paper over that. Inferring a failure from an estimate that
//! crossed a threshold with no compaction after it would be a guess dressed as
//! an event, and the fix belongs where the failure is (ADR-0005). What a host
//! *can* rely on is the pass it asks for itself:
//! [`PreparedRun::compact`](crate::PreparedRun::compact) reports its failure on
//! the stream as well as to the caller.
//!
//! Neither kind of pass is counted in [`RunUsage`](crate::RunUsage) — see
//! there for why.
//!
//! # What is not here
//!
//! mentra's `CompactionConfig` has eleven fields. This exposes four: the three
//! triggers above and how much recent user text a summarizing pass must leave
//! alone. The mutually exclusive projected-byte budget is pinned off: exposing
//! it needs a Basis-owned policy shape rather than two knobs that can disagree.
//! The summary's input and output caps, local versus remote summarization, and
//! how many snapshots are kept remain defaults nobody has had a reason to move.
//! A knob basis offers is a knob basis has to keep meaning; they arrive when a
//! host asks, with the case that asked.
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
    /// The estimated request size that triggers a summarizing pass, for a
    /// model whose context window is unknown, and the switch for the
    /// window-relative trigger too: `None` turns automatic summarizing off
    /// outright, including through
    /// [`auto_threshold_percent`](Self::auto_threshold_percent).
    auto_threshold_tokens: Option<usize>,
    /// The percentage of a *known* context window that triggers a summarizing
    /// pass, winning over `auto_threshold_tokens` when both apply. `None`
    /// pins the trigger to the absolute number even when the window is known.
    /// Only consulted while `auto_threshold_tokens` is set.
    auto_threshold_percent: Option<u8>,
    /// How much recent user text a summarizing pass must leave verbatim.
    preserve_recent_user_tokens: usize,
}

/// Keeps every tool result, and leaves every one of mentra's summarizing
/// numbers where mentra put them.
///
/// The three it leaves alone are *read off* mentra's own default rather than
/// copied into a constant here. basis has no basis for choosing any of
/// them — the window-relative trigger is a fact about the model, and the
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
            auto_threshold_percent: mentra.auto_compact_threshold_percent,
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
    /// stated plainly — the model stops being able to see results it was shown.
    /// The request-only event reports that loss, but any turn that depends on
    /// comparing an early result against a late one still becomes unreliable.
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
    /// for a model whose context window basis does not know — or turns
    /// automatic summarizing off entirely, with `None`.
    ///
    /// The estimate is mentra's, over the serialized messages plus the system
    /// prompt, and it is not the provider's accounting.
    /// [`with_auto_threshold_percent`](Self::with_auto_threshold_percent) wins
    /// over this whenever the window *is* known, so this is the fallback a
    /// host sets for a model it cannot ask, or the only number that matters
    /// once the percentage is cleared.
    ///
    /// `None` is the off switch for *both* triggers, not just for this one.
    /// mentra resolves the pair together and returns "no threshold" as soon as
    /// this is cleared, without ever reading the percentage
    /// (`CompactionConfig::auto_compact_threshold`), so a host that wants the
    /// window-relative trigger and no absolute one has to leave a number here
    /// — a large one, if it wants the percentage to be what decides. There is
    /// no basis-side repair for that and there should not be one: the
    /// threshold is the runtime's to resolve (ADR-0005).
    ///
    /// `None` is a real posture, not a way of saying *later*: nothing here
    /// shortens the history ahead of time, and a conversation that outgrows
    /// the window gets exactly the recovery every other posture also gets — the
    /// provider's own refusal triggers one compaction and one retry (see this
    /// module's header) — rather than a second, earlier attempt basis would
    /// have made on its own guess. It suits a run that is bounded by
    /// construction — one prompt, a handful of turns — where an earlier pass
    /// would only ever cost a model call it did not need.
    pub fn with_auto_threshold_tokens(self, tokens: Option<usize>) -> Self {
        Self {
            auto_threshold_tokens: tokens,
            ..self
        }
    }

    /// Triggers a summarizing pass once an estimated request reaches `percent`
    /// of the model's context window, when that window is known — or pins the
    /// trigger to [`with_auto_threshold_tokens`](Self::with_auto_threshold_tokens)'s
    /// absolute number regardless, with `None`.
    ///
    /// The window is known when
    /// [`PreparedRun::context_window`](crate::PreparedRun::context_window)
    /// returns `Some` — today, a workspace whose provider is Gemini; Anthropic
    /// and the Responses transport do not report one, so this has no effect for
    /// them until they do. Values above 100 are treated as 100.
    ///
    /// It has no effect either while
    /// [`with_auto_threshold_tokens`](Self::with_auto_threshold_tokens) is
    /// `None`, which is that knob's off switch for the whole mechanism — see
    /// there.
    ///
    /// basis reads mentra's own default (75) here rather than choosing one:
    /// unlike the absolute number, a percentage is not a guess about any
    /// particular model, so there is nothing basis knows that would make a
    /// different figure more honest.
    pub fn with_auto_threshold_percent(self, percent: Option<u8>) -> Self {
        Self {
            auto_threshold_percent: percent,
            ..self
        }
    }

    /// Keeps at least `tokens` of the most recent user turns verbatim through a
    /// summarizing pass.
    ///
    /// What the user actually asked for is the thing a summary is least able to
    /// stand in for, which is why mentra protects a budget of it and why this
    /// is a knob rather than one of the six that stayed behind.
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
    /// point of the four-knob surface: basis states what it has a reason to
    /// state and inherits the rest, so an upstream improvement to summarization
    /// arrives rather than being shadowed by a copy of the old numbers.
    pub(crate) fn into_mentra(self, transcript_dir: PathBuf) -> mentra::agent::CompactionConfig {
        mentra::agent::CompactionConfig {
            // mentra's documented off switch (`memory::compaction::
            // micro_compact_history` returns the history untouched at
            // `usize::MAX`), which is what makes "keep everything" a
            // configuration of upstream rather than a fork of it.
            keep_recent_tool_results: self.keep_recent_tool_results.unwrap_or(usize::MAX),
            // Basis exposes the established count policy only. Mentra 0.23's
            // byte-budget mode is mutually exclusive with it, so inheriting a
            // future non-None default would silently ignore the value above.
            projected_tool_result_budget: None,
            auto_compact_threshold_tokens: self.auto_threshold_tokens,
            auto_compact_threshold_percent: self.auto_threshold_percent,
            preserve_recent_user_tokens: self.preserve_recent_user_tokens,
            transcript_dir,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
