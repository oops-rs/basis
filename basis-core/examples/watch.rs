//! `basis watch`, deleted as a feature and rebuilt as a page of host code.
//!
//! ADR-0014 retired the `watch` subcommand because it was three things wearing
//! one name: a timer, a change-detector, and per-iteration bounds. Only the
//! first was ever a scheduler opinion, and it was never basis's to hold — so the
//! timer here is `tokio::time::sleep`, the change-detector is
//! `Workspace::fingerprint`, and the bounds are the ones any run nobody is
//! watching should be stating anyway.
//!
//! This file is kept less as documentation than as a standing acceptance test.
//! If the loop below ever stops being trivial, the regression is in basis's API
//! and not in the recipe.
//!
//! ```sh
//! export BASIS_API_KEY=…                    # or ANTHROPIC_API_KEY, etc.
//! export BASIS_BASE_URL=http://…/v1         # optional
//! export BASIS_MODEL=…                      # optional
//! cargo run -p basis-core --example watch -- /repo "fix whatever the tests say" 60
//! ```

use std::{env, time::Duration};

use basis_core::{ModelSelector, NullSink, RunOutcome, RunReport, RunSpec, Snapshot, Workspace};

/// What one iteration may spend.
///
/// Stated here because nothing states it for us: with no scheduler shipped
/// there is no period for basis to guess a deadline from, so ADR-0014 makes
/// bounding explicit everywhere. Both are graceful — whatever the model
/// committed before a bound tripped is kept.
const DEADLINE: Duration = Duration::from_secs(300);
const TOOL_BUDGET: usize = 40;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args.next().unwrap_or_else(|| ".".to_string());
    let prompt = args
        .next()
        .unwrap_or_else(|| "Summarize what changed here, in one sentence.".to_string());
    let interval = Duration::from_secs(
        args.next()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(60_u64),
    );

    // Everything expensive happens once, outside the loop: context discovery,
    // the credential, the model, skills, templates, MCP connections. Each tick
    // mints a run from what is already resolved (ADR-0010).
    let workspace = Workspace::builder(&path)
        .with_model(selected_model())
        .open()
        .await?;
    let spec = RunSpec::new(prompt)
        .with_deadline(DEADLINE)
        .with_tool_budget(TOOL_BUDGET);
    println!(
        "watching {} every {interval:?} with {}",
        workspace.root().display(),
        workspace.model()
    );

    let mut baseline = None;
    loop {
        // Blocking — it spawns `git` and stats every file — and called inline
        // because this loop has nothing else to do while it waits. A host with
        // a runtime to keep responsive wants `tokio::task::spawn_blocking`.
        let snapshot = workspace.fingerprint();

        // Only a fingerprint that is both known and equal may skip. `Unknown`
        // means "I could not tell", and the asymmetry that governs this loop is
        // that a false "changed" costs tokens while a false "unchanged" stops
        // the loop working — so anything else runs.
        if let Snapshot::Known(observed) = &snapshot
            && Some(*observed) == baseline
        {
            println!("unchanged at {} — skipping", observed.hex());
        } else {
            let report = workspace.prepare(spec.clone())?.execute(NullSink).await?;
            println!("ran: {}", summarize(&report));

            // Recorded *after* the run, so the run's own edits do not retrigger
            // it, and only after one that succeeded, so a transient failure
            // cannot write off an unchanged workspace forever. ADR-0014 moved
            // this policy to the caller because the caller is where the
            // definition of "success" now lives — this line is that definition.
            if report.succeeded() {
                baseline = workspace.fingerprint().fingerprint();
            }
        }

        tokio::time::sleep(interval).await;
    }
}

/// `BASIS_MODEL` when it is set, the provider's newest otherwise — the same
/// selection the other examples make.
fn selected_model() -> ModelSelector {
    match env::var("BASIS_MODEL") {
        Ok(id) => ModelSelector::Id(id),
        Err(_) => ModelSelector::NewestAvailable,
    }
}

/// One line per iteration, and the distinction ADR-0015 asks a script to be
/// able to draw: a run that failed is not the same event as a run that ran out
/// of the time it was given.
fn summarize<S>(report: &RunReport<S>) -> String {
    match (&report.outcome, report.stopped_by) {
        (RunOutcome::Ok, _) => report
            .final_message
            .as_deref()
            .unwrap_or("(no message)")
            .trim()
            .to_string(),
        (RunOutcome::Error { .. }, Some(bound)) => format!("stopped by {bound:?}"),
        (RunOutcome::Error { message }, None) => format!("failed: {message}"),
    }
}
