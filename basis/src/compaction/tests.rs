//! What basis states about compaction, and what it leaves to mentra.

use std::path::Path;

use super::*;

fn transcripts() -> PathBuf {
    PathBuf::from("/store/transcripts")
}

#[test]
fn nothing_the_model_read_is_elided_by_default() {
    // The whole point of the type: `usize::MAX` is mentra's off switch for
    // micro-compaction, and basis's default is off.
    let config = Compaction::default().into_mentra(transcripts());

    assert_eq!(config.keep_recent_tool_results, usize::MAX);
}

#[test]
fn the_summarizing_trigger_is_still_mentras() {
    // basis has no context-window knowledge, so it has no business choosing a
    // trigger. Pinned against upstream's own default rather than a literal, so
    // that a number basis inherits cannot be inherited by accident.
    let config = Compaction::default().into_mentra(transcripts());
    let mentra = mentra::agent::CompactionConfig::default();

    assert_eq!(
        config.auto_compact_threshold_tokens,
        mentra.auto_compact_threshold_tokens
    );
    assert_eq!(config.auto_compact_threshold_tokens, Some(50_000));
    assert_eq!(
        config.preserve_recent_user_tokens,
        mentra.preserve_recent_user_tokens
    );
}

#[test]
fn every_knob_basis_does_not_offer_stays_at_mentras_default() {
    // The three-knob surface is only honest if the other six are genuinely
    // untouched: a field basis quietly restated would be a second opinion to
    // keep in step with upstream's first.
    let config = Compaction::default().into_mentra(transcripts());
    let mentra = mentra::agent::CompactionConfig::default();

    assert_eq!(
        config.summary_max_input_chars,
        mentra.summary_max_input_chars
    );
    assert_eq!(
        config.summary_max_output_tokens,
        mentra.summary_max_output_tokens
    );
    assert_eq!(config.mode, mentra.mode);
    assert_eq!(
        config.preserve_recent_delegation_results,
        mentra.preserve_recent_delegation_results
    );
    assert_eq!(
        config.max_persisted_transcripts,
        mentra.max_persisted_transcripts
    );
}

#[test]
fn a_host_that_asks_for_elision_by_number_gets_that_number() {
    let config = Compaction::default()
        .with_keep_recent_tool_results(Some(3))
        .into_mentra(transcripts());

    assert_eq!(config.keep_recent_tool_results, 3);
}

#[test]
fn every_knob_reaches_the_config_mentra_reads() {
    let config = Compaction::default()
        .with_keep_recent_tool_results(Some(7))
        .with_auto_threshold_tokens(Some(180_000))
        .with_preserve_recent_user_tokens(4_000)
        .into_mentra(transcripts());

    assert_eq!(config.keep_recent_tool_results, 7);
    assert_eq!(config.auto_compact_threshold_tokens, Some(180_000));
    assert_eq!(config.preserve_recent_user_tokens, 4_000);
}

#[test]
fn an_unset_threshold_never_summarizes() {
    // Distinct from a very large one: mentra reads `None` as "do not check",
    // so nothing shortens the history and an oversized turn fails at the
    // provider instead.
    let config = Compaction::default()
        .with_auto_threshold_tokens(None)
        .into_mentra(transcripts());

    assert_eq!(config.auto_compact_threshold_tokens, None);
}

#[test]
fn setters_return_new_values() {
    let base = Compaction::default();
    let derived = base.with_keep_recent_tool_results(Some(1));

    assert_eq!(base.keep_recent_tool_results, None, "the original moved");
    assert_eq!(derived.keep_recent_tool_results, Some(1));
}

#[test]
fn the_snapshot_directory_is_the_one_it_was_given() {
    // The field basis does not offer as a knob, because the caller that knows
    // it is the runtime and a second answer here could disagree with it.
    let config = Compaction::default().into_mentra(PathBuf::from("/elsewhere/transcripts"));

    assert_eq!(config.transcript_dir, Path::new("/elsewhere/transcripts"));
}
