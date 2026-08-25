//! `basis list`: this workspace's tasks, over `basis_tasks::Tasks::list`.
//!
//! The scan, the ordering, and the JSON shape are `basis-tasks`'s
//! ([`basis_tasks::TaskSummary`]); what is here is the terminal-facing half —
//! the plain-text row, the default-50 screenful, and the hint.

use std::process::ExitCode;

use basis_tasks::{TaskSummary, Tasks, now_ms};
use serde_json::json;

use crate::{cli::ListArgs, exit::EXIT_OK};

use super::{error::ClientError, render::print_hint};

/// How many rows a bare `basis list` prints.
///
/// A workspace holds up to [`basis_tasks::MAX_TASKS`] tasks and a person
/// scanning a terminal reads the last screenful, so the default is a
/// screenful-ish and `--all` is the way to ask for the rest. Stated on
/// stderr when it elides anything: a bound nobody is told about is
/// indistinguishable from missing data.
const DEFAULT_LIMIT: usize = 50;

pub(crate) fn list(args: ListArgs) -> Result<ExitCode, ClientError> {
    let workspace = match args.workspace.clone() {
        Some(path) => path,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };
    let tasks = Tasks::open(workspace)?;
    let summaries = tasks.list()?;

    let shown = if args.all {
        summaries.len()
    } else {
        summaries.len().min(DEFAULT_LIMIT)
    };
    let now = now_ms();
    for summary in &summaries[..shown] {
        if args.json {
            println!("{}", summary.payload());
        } else {
            println!("{}", row(summary, now));
        }
    }

    if summaries.is_empty() && !args.json {
        eprintln!("basis: no tasks in this workspace");
    }
    if shown < summaries.len() {
        eprintln!(
            "basis: showing the {shown} most recent of {}; use `--all` for the rest",
            summaries.len()
        );
    }
    print_hint(&json!({"next": next_step(&summaries)}));
    Ok(ExitCode::from(EXIT_OK))
}

fn row(summary: &TaskSummary, now: u64) -> String {
    format!(
        "{}  {:<9}  {:>8}  {}",
        summary.task,
        summary.state,
        age(summary.last_activity_ms, now),
        summary.prompt
    )
}

fn next_step(summaries: &[TaskSummary]) -> String {
    if summaries.iter().any(|summary| !summary.agent_id.is_empty()) {
        "basis spawn --continue <PROMPT>".to_string()
    } else {
        "basis spawn <PROMPT>".to_string()
    }
}

/// How long ago, in the coarsest unit that still says something.
///
/// Relative rather than absolute because a list is read to find the run you
/// remember, and "2h ago" answers that where a timestamp asks you to
/// subtract. `--json` carries both `last_activity_ms` and `started_ms` for
/// anything that needs the arithmetic, or the other clock.
fn age(since_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(since_ms) / 1_000;
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 172_800 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(task: &str, state: &str, started_ms: u64, agent_id: &str) -> TaskSummary {
        TaskSummary {
            task: task.to_string(),
            state: state.to_string(),
            started_ms,
            last_activity_ms: started_ms,
            prompt: "fix the failing test".to_string(),
            agent_id: agent_id.to_string(),
            usage: basis::RunUsage::default(),
        }
    }

    #[test]
    fn a_row_carries_the_handle_its_follow_up_needs() {
        let printed = row(&summary("w/t", "succeeded", 0, "agent-1"), 7_200_000);

        assert!(printed.starts_with("w/t"), "{printed}");
        assert!(printed.contains("succeeded"), "{printed}");
        assert!(printed.contains("2h ago"), "{printed}");
        assert!(printed.contains("fix the failing test"), "{printed}");
    }

    /// The listing is ordered by activity, so the age a row prints has to be
    /// that same activity — a column measuring one clock beside an order
    /// following another is a list that cannot be read.
    #[test]
    fn a_row_ages_by_when_it_was_last_worked_in() {
        let mut worked = summary("w/t", "succeeded", 0, "agent-1");
        worked.last_activity_ms = 7_140_000;

        let printed = row(&worked, 7_200_000);
        assert!(
            printed.contains("1m ago"),
            "not 2h, which is only how long ago it started: {printed}"
        );
    }

    #[test]
    fn ages_are_read_in_the_coarsest_unit_that_still_says_something() {
        assert_eq!(age(0, 30_000), "just now");
        assert_eq!(age(0, 5 * 60_000), "5m ago");
        assert_eq!(age(0, 3 * 3_600_000), "3h ago");
        assert_eq!(age(0, 3 * 86_400_000), "3d ago");
        assert_eq!(age(500, 0), "just now", "a clock that went backwards");
    }

    /// `--continue` means "the conversation I was just having", so the hint
    /// changes the moment any task in the list has one.
    #[test]
    fn the_hint_offers_continue_only_once_a_conversation_exists() {
        let none = [summary("w/t", "resumable", 0, "")];
        assert_eq!(next_step(&none), "basis spawn <PROMPT>");

        let some = [summary("w/t", "succeeded", 0, "agent-1")];
        assert_eq!(next_step(&some), "basis spawn --continue <PROMPT>");
    }
}
