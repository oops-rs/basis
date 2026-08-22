//! The workspace's tasks, read straight off its directories.
//!
//! ADR-0019 made the filesystem the coordination surface, and this is the verb
//! that says so out loud: `basis list` takes no lock and derives every state
//! from the two facts an executor already publishes — whether `terminal.json`
//! exists, and whether anyone holds `attach.lock`. It is the same derivation
//! `wait` and `watch` use, reached through the same
//! [`probe_state`], so the three verbs cannot
//! disagree about what a task is doing.
//!
//! It mints nothing. A workspace that has never run anything is reported as
//! having no tasks, through [`DataDir::described_workspace`] rather than
//! `ensure_workspace`, because a listing that created the directory proving
//! its own answer wrong is not an observation.
//!
//! The scan answers a second question too. "The conversation I was just
//! having" is the newest row here that has one, which is what
//! `spawn --continue` resolves against — one implementation, so a handle
//! `list` printed is a handle `--session` accepts.

use std::{path::Path, process::ExitCode};

use basis::RunUsage;
use serde_json::{Value, json};

use crate::{cli::ListArgs, exit::EXIT_OK};

use super::{
    data_dir::{AgentPaths, DataDir, canonical_workspace, valid_task_handle, workspace_key},
    error::{ClientError, probe_state},
    lock,
    render::print_hint,
    state::{load_meta, now_ms, read_terminal},
};

/// How many rows a bare `basis list` prints.
///
/// A workspace holds up to `MAX_TASKS` agents and a person scanning a terminal
/// reads the last screenful, so the default is a screenful-ish and `--all` is
/// the way to ask for the rest. Stated on stderr when it elides anything: a
/// bound nobody is told about is indistinguishable from missing data.
const DEFAULT_LIMIT: usize = 50;

/// How much of a prompt's first line a row carries. A row is an index entry,
/// not the prompt — `basis watch <ID>` has the whole run.
const PROMPT_BUDGET: usize = 64;

/// One task, as `list` reports it and as `--continue` picks from it.
#[derive(Debug, Clone)]
pub(crate) struct TaskSummary {
    pub task: String,
    /// `running`, `resumable`, or whatever the terminal record settled on.
    pub state: String,
    pub started_ms: u64,
    /// The prompt's first line, bounded by [`PROMPT_BUDGET`].
    pub prompt: String,
    /// The mentra conversation this task minted or continued. Empty until a
    /// first attach prepares one, which is why a never-attached task cannot be
    /// continued: there is nothing yet to continue.
    pub agent_id: String,
    pub usage: RunUsage,
}

impl TaskSummary {
    fn payload(&self) -> Value {
        let mut payload = json!({
            "task": self.task,
            "state": self.state,
            "started_ms": self.started_ms,
            "prompt": self.prompt,
            "continuable": !self.agent_id.is_empty(),
        });
        // Absent rather than zeroed, the same rule the terminal record and the
        // finish line follow: nothing reported is not a measurement of nothing.
        if self.usage != RunUsage::default() {
            payload["usage"] = json!(self.usage);
        }
        payload
    }

    fn row(&self, now: u64) -> String {
        format!(
            "{}  {:<9}  {:>8}  {}",
            self.task,
            self.state,
            age(self.started_ms, now),
            self.prompt
        )
    }
}

pub(crate) fn list(args: ListArgs) -> Result<ExitCode, ClientError> {
    let workspace = match args.workspace.clone() {
        Some(path) => path,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };
    let data = DataDir::discover().map_err(|error| format!("open task data directory: {error}"))?;
    // A workspace nothing has ever run in lists nothing, which is a complete
    // answer rather than an error.
    let summaries: Vec<TaskSummary> = workspace_tasks(&data, &workspace)?.unwrap_or_default();

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
            println!("{}", summary.row(now));
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

/// Every task recorded for `workspace`, newest first, or `None` when nothing
/// has ever run there.
///
/// The digest is checked against the path it claims to describe before a
/// single row is read: two workspaces sharing an FNV key would otherwise list
/// each other's tasks, and a handle copied out of that list would name work
/// somewhere else entirely.
pub(crate) fn workspace_tasks(
    data: &DataDir,
    workspace: &Path,
) -> Result<Option<Vec<TaskSummary>>, String> {
    let canonical = canonical_workspace(workspace)
        .map_err(|error| format!("resolve workspace {}: {error}", workspace.display()))?;
    let key = workspace_key(&canonical);
    match data.described_workspace(&key) {
        None => return Ok(None),
        Some(described) if described != canonical => {
            return Err(format!(
                "workspace key collision: {key} describes {}, not {}",
                described.display(),
                canonical.display()
            ));
        }
        Some(_) => {}
    }
    Ok(Some(scan(data, &key)?))
}

/// The agent directories under one workspace key, newest first.
///
/// A directory whose metadata cannot be read is skipped rather than fatal: a
/// half-written agent dir is what a `kill -9` during `spawn` leaves, and one
/// unreadable neighbour must not cost a person the list of everything else.
fn scan(data: &DataDir, key: &str) -> Result<Vec<TaskSummary>, String> {
    let agents = data.agents_dir(key);
    let entries = match std::fs::read_dir(&agents) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("scan workspace agents: {error}")),
    };

    let mut summaries = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("scan workspace agents: {error}"))?;
        let task = format!("{key}/{}", entry.file_name().to_string_lossy());
        let Some(paths) = data.agent_dir(&task).filter(AgentPaths::exists) else {
            continue;
        };
        let Ok(meta) = load_meta(&paths) else {
            continue;
        };
        summaries.push(TaskSummary {
            state: task_state(&paths)?,
            started_ms: meta.created_ms,
            prompt: first_line(&meta.prompt),
            agent_id: meta.agent_id,
            usage: meta.usage,
            task,
        });
    }
    // Newest first, ties broken by handle so two tasks minted in the same
    // millisecond still list in a stable order rather than the directory's.
    summaries.sort_by(|left, right| {
        right
            .started_ms
            .cmp(&left.started_ms)
            .then_with(|| left.task.cmp(&right.task))
    });
    Ok(summaries)
}

/// A task's state, derived exactly as `wait` and `watch` derive it: the
/// terminal record first, because it is immutable and repeatably observable,
/// then the attach lock, which is the only evidence a live executor leaves.
fn task_state(paths: &AgentPaths) -> Result<String, String> {
    match read_terminal(paths)? {
        Some(terminal) => Ok(terminal["state"].as_str().unwrap_or("unknown").to_string()),
        None => Ok(probe_state(lock::is_held(&paths.attach_lock())).to_string()),
    }
}

/// The conversation `--continue` picks up: the newest task in this workspace
/// that has one.
///
/// Tasks that never minted an agent are skipped rather than refused. A
/// `--resumable` spawn nobody attached to is a perfectly ordinary thing to
/// have lying around, and "continue what I was doing" plainly does not mean
/// it: there is no conversation there to continue.
pub(crate) fn latest_conversation(summaries: &[TaskSummary]) -> Option<&TaskSummary> {
    summaries
        .iter()
        .find(|summary| !summary.agent_id.is_empty())
}

/// The task a handle names, when it names one in this workspace.
///
/// A handle from another workspace is refused rather than searched for: the
/// key is half the handle, and a task's conversation belongs to the workspace
/// whose context and tools it ran with.
pub(crate) fn named<'a>(
    summaries: &'a [TaskSummary],
    workspace_key: &str,
    handle: &str,
) -> Result<&'a TaskSummary, ClientError> {
    let Some((key, _)) = valid_task_handle(handle) else {
        return Err(ClientError::usage(format!(
            "`{handle}` is not a task handle; `basis list` prints them"
        )));
    };
    if key != workspace_key {
        return Err(ClientError::usage(format!(
            "task {handle} belongs to another workspace; run `basis list` where it was started"
        )));
    }
    summaries
        .iter()
        .find(|summary| summary.task == handle)
        .ok_or_else(|| ClientError::new(format!("no task directory for {handle}")))
}

fn next_step(summaries: &[TaskSummary]) -> String {
    match latest_conversation(summaries) {
        Some(_) => "basis spawn --continue <PROMPT>".to_string(),
        None => "basis spawn <PROMPT>".to_string(),
    }
}

/// The prompt's first line, bounded.
fn first_line(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(PROMPT_BUDGET) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

/// How long ago, in the coarsest unit that still says something.
///
/// Relative rather than absolute because a list is read to find the run you
/// remember, and "2h ago" answers that where a timestamp asks you to subtract.
/// `--json` carries `started_ms` for anything that needs the arithmetic.
fn age(started_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(started_ms) / 1_000;
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
            prompt: "fix the failing test".to_string(),
            agent_id: agent_id.to_string(),
            usage: RunUsage::default(),
        }
    }

    #[test]
    fn a_row_carries_the_handle_its_follow_up_needs() {
        let row = summary("w/t", "succeeded", 0, "agent-1").row(7_200_000);

        assert!(row.starts_with("w/t"), "{row}");
        assert!(row.contains("succeeded"), "{row}");
        assert!(row.contains("2h ago"), "{row}");
        assert!(row.contains("fix the failing test"), "{row}");
    }

    #[test]
    fn a_json_row_says_whether_it_can_be_continued() {
        let started = summary("w/t", "succeeded", 5, "agent-1").payload();
        assert_eq!(started["task"], "w/t");
        assert_eq!(started["state"], "succeeded");
        assert_eq!(started["started_ms"], 5);
        assert_eq!(started["continuable"], true);
        assert!(
            started.get("usage").is_none(),
            "a task that reported nothing claims no measurement: {started}"
        );

        let never_attached = summary("w/t2", "resumable", 5, "").payload();
        assert_eq!(
            never_attached["continuable"], false,
            "an agent nobody attached to has no conversation yet"
        );
    }

    #[test]
    fn a_json_row_carries_what_the_task_spent() {
        let mut spent = summary("w/t", "succeeded", 5, "agent-1");
        spent.usage = RunUsage {
            input_tokens: 900,
            output_tokens: 100,
            ..RunUsage::default()
        };

        assert_eq!(spent.payload()["usage"]["input_tokens"], 900);
    }

    /// `--continue` means "the conversation I was just having", and a
    /// `--resumable` agent nobody ever attached to is not one.
    #[test]
    fn continue_takes_the_newest_task_that_has_a_conversation() {
        let summaries = vec![
            summary("w/newest", "resumable", 300, ""),
            summary("w/middle", "succeeded", 200, "agent-middle"),
            summary("w/oldest", "succeeded", 100, "agent-oldest"),
        ];

        assert_eq!(
            latest_conversation(&summaries).map(|summary| summary.task.as_str()),
            Some("w/middle")
        );
        assert!(
            latest_conversation(&summaries[..1]).is_none(),
            "a workspace whose only task never ran has nothing to continue"
        );
    }

    #[test]
    fn a_handle_from_another_workspace_is_refused_rather_than_searched_for() {
        let here = "0123456789abcdef";
        let elsewhere = format!("fedcba9876543210/{:032x}", 1);
        let summaries = vec![summary(&format!("{here}/{:032x}", 1), "succeeded", 1, "a")];

        let error = named(&summaries, here, &elsewhere).expect_err("refused");
        assert!(
            format!("{error:?}").contains("another workspace"),
            "{error:?}"
        );

        let malformed = named(&summaries, here, "not-a-handle").expect_err("refused");
        assert!(format!("{malformed:?}").contains("not a task handle"));

        let found = named(&summaries, here, &summaries[0].task).expect("in this workspace");
        assert_eq!(found.task, summaries[0].task);
    }

    #[test]
    fn ages_are_read_in_the_coarsest_unit_that_still_says_something() {
        assert_eq!(age(0, 30_000), "just now");
        assert_eq!(age(0, 5 * 60_000), "5m ago");
        assert_eq!(age(0, 3 * 3_600_000), "3h ago");
        assert_eq!(age(0, 3 * 86_400_000), "3d ago");
        assert_eq!(age(500, 0), "just now", "a clock that went backwards");
    }

    #[test]
    fn a_row_carries_one_bounded_line_of_the_prompt() {
        assert_eq!(first_line("fix the test\nthen push"), "fix the test");
        assert!(first_line(&"x".repeat(200)).ends_with('…'));
        assert_eq!(
            first_line(&"界".repeat(100)).chars().count(),
            PROMPT_BUDGET + 1
        );
    }
}
