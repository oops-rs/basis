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
    assert_eq!(config.projected_tool_result_budget, None);
}

#[test]
fn the_summarizing_triggers_are_still_mentras() {
    // basis has no window-independent reason to choose either number. Pinned
    // against upstream's own defaults rather than literals, so that a number
    // basis inherits cannot be inherited by accident.
    let config = Compaction::default().into_mentra(transcripts());
    let mentra = mentra::agent::CompactionConfig::default();

    assert_eq!(
        config.auto_compact_threshold_tokens,
        mentra.auto_compact_threshold_tokens
    );
    assert_eq!(config.auto_compact_threshold_tokens, Some(50_000));
    assert_eq!(
        config.auto_compact_threshold_percent,
        mentra.auto_compact_threshold_percent
    );
    assert_eq!(config.auto_compact_threshold_percent, Some(75));
    assert_eq!(
        config.preserve_recent_user_tokens,
        mentra.preserve_recent_user_tokens
    );
}

#[test]
fn every_inherited_knob_stays_at_mentras_default() {
    // The four-knob surface is only honest if the remaining defaults are
    // genuinely inherited. The projected byte budget is the deliberate
    // exception: it is pinned off above because it conflicts with the count
    // policy Basis exposes.
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
    assert_eq!(config.projected_tool_result_budget, None);
}

#[test]
fn a_host_that_asks_for_elision_by_number_gets_that_number() {
    let config = Compaction::default()
        .with_keep_recent_tool_results(Some(3))
        .into_mentra(transcripts());

    assert_eq!(config.keep_recent_tool_results, 3);
    assert_eq!(config.projected_tool_result_budget, None);
}

#[test]
fn every_knob_reaches_the_config_mentra_reads() {
    let config = Compaction::default()
        .with_keep_recent_tool_results(Some(7))
        .with_auto_threshold_tokens(Some(180_000))
        .with_auto_threshold_percent(Some(90))
        .with_preserve_recent_user_tokens(4_000)
        .into_mentra(transcripts());

    assert_eq!(config.keep_recent_tool_results, 7);
    assert_eq!(config.auto_compact_threshold_tokens, Some(180_000));
    assert_eq!(config.auto_compact_threshold_percent, Some(90));
    assert_eq!(config.preserve_recent_user_tokens, 4_000);
}

#[test]
fn clearing_the_absolute_threshold_turns_the_percent_trigger_off_with_it() {
    // The two fields are not two independent triggers, and this test used to
    // say they were. mentra resolves them together
    // (`CompactionConfig::auto_compact_threshold`) and reads a cleared
    // absolute number as "off" before it ever looks at the percentage, so a
    // known context window does not switch it back on. Asserted against the
    // resolved threshold rather than the two fields: the fields arriving
    // intact is all the previous version proved, while the behaviour its name
    // claimed was upstream's opposite.
    let config = Compaction::default()
        .with_auto_threshold_tokens(None)
        .into_mentra(transcripts());

    assert_eq!(config.auto_compact_threshold_tokens, None);
    assert_eq!(config.auto_compact_threshold_percent, Some(75));
    assert_eq!(
        config.auto_compact_threshold(Some(200_000)),
        None,
        "a known window must not revive a cleared trigger"
    );
    assert_eq!(config.auto_compact_threshold(None), None);
}

#[test]
fn a_known_window_is_what_the_percent_trigger_reads_when_both_are_set() {
    // The other half of the pair, so the correction above cannot be read as
    // "the percentage never does anything": with the absolute number left in
    // place it is the percentage that decides, and 75% of the window is what
    // mentra resolves to.
    let config = Compaction::default().into_mentra(transcripts());

    assert_eq!(config.auto_compact_threshold(Some(200_000)), Some(150_000));
    assert_eq!(
        config.auto_compact_threshold(None),
        Some(50_000),
        "and an unknown window falls back to the absolute number"
    );
}

#[test]
fn an_unset_percent_pins_the_trigger_to_the_absolute_number() {
    let config = Compaction::default()
        .with_auto_threshold_percent(None)
        .into_mentra(transcripts());

    assert_eq!(config.auto_compact_threshold_percent, None);
    assert_eq!(config.auto_compact_threshold_tokens, Some(50_000));
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
