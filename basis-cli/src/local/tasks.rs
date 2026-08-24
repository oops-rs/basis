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
//! having" is the first row here that has one, which is what
//! `spawn --continue` resolves against — one implementation, so a handle
//! `list` printed is a handle `--session` accepts.
//!
//! Which makes the row order load-bearing, and it is the order of last
//! activity, not of birth: a task started this morning and worked in a minute
//! ago is the conversation you are in, and the one spawned after lunch and
//! abandoned is not. The age each row prints is that same activity, so the
//! listing is ordered by a fact it shows — `--json` keeps `started_ms` for
//! anyone who wanted the birthday. See [`last_activity_ms`] for what counts.

use std::{path::Path, process::ExitCode};

use basis::RunUsage;
use serde_json::{Value, json};

use crate::{cli::ListArgs, exit::EXIT_OK};

use super::{
    data_dir::{AgentPaths, DataDir, canonical_workspace, valid_task_handle, workspace_key},
    error::{ClientError, probe_state},
    inbox, lock,
    render::print_hint,
    state::{MessageRecord, TaskMeta, load_meta, now_ms, read_terminal},
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
    /// When this task was last worked in, per [`last_activity_ms`]. What the
    /// rows are ordered and aged by, and what `--continue` picks by.
    pub last_activity_ms: u64,
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
            "last_activity_ms": self.last_activity_ms,
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
            age(self.last_activity_ms, now),
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

/// Every task recorded for `workspace`, last worked in first, or `None` when
/// nothing has ever run there.
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

/// The agent directories under one workspace key, last worked in first.
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
        // An unreadable inbox costs this row its messages, not its place: the
        // executor's own clock is still a complete-enough answer, and the same
        // rule the unreadable-neighbour skip above follows.
        let messages = inbox::load(&paths).unwrap_or_default();
        // And an unreadable terminal record costs this row its state, not the
        // list its rows: `task_state` already answers "unknown" for a terminal
        // whose `state` field is not a string, and a record that does not
        // parse is the same fact one level down. Only in the survey — `wait`
        // and `watch` on the damaged task itself still fail loudly, because
        // asking about one task is a different question from listing them all.
        let state = task_state(&paths).unwrap_or_else(|_| "unknown".to_string());
        summaries.push(TaskSummary {
            state,
            started_ms: meta.created_ms,
            last_activity_ms: last_activity_ms(&meta, &messages),
            prompt: first_line(&meta.prompt),
            agent_id: meta.agent_id,
            usage: meta.usage,
            task,
        });
    }
    // Last worked in first, ties broken by handle so two tasks touched in the
    // same millisecond still list in a stable order rather than the
    // directory's.
    summaries.sort_by(|left, right| {
        right
            .last_activity_ms
            .cmp(&left.last_activity_ms)
            .then_with(|| left.task.cmp(&right.task))
    });
    Ok(summaries)
}

/// When anything last happened in a task, read off the two files that already
/// record it: `meta.json`, which the executor rewrites as it attaches, banks a
/// turn, and settles, and `inbox.json`, where a sender stamps the message it
/// enqueued.
///
/// Derived rather than stored, so neither writer has to reach into the other's
/// file. `send` runs under the inbox lock while an executor may be holding the
/// attach lock, and a second writer on `meta.json` is a lost update waiting to
/// happen — the banked usage of a turn that finished between that sender's
/// read and its write. The two clocks already exist; the maximum of them is
/// the fact, and nothing new has to be kept consistent (ADR-0019).
///
/// **Reading is not activity.** `watch`, `list`, and `wait` on a settled task
/// write nothing here, and must not: looking at a run is not being in the
/// conversation, and a `watch` left open in another terminal would otherwise
/// decide what `--continue` picks up. `wait` on an *unsettled* task does
/// count — it attaches and runs turns, which is the executor working rather
/// than a reader looking.
///
/// Never earlier than the start. A record that carries no activity — one
/// written before basis recorded any — resolves by `created_ms` rather than by
/// zero, which would sort every task predating this field behind every task
/// after it.
fn last_activity_ms(meta: &TaskMeta, messages: &[MessageRecord]) -> u64 {
    let sent = messages
        .iter()
        .map(|message| message.created_ms)
        .max()
        .unwrap_or_default();
    meta.created_ms.max(meta.updated_ms).max(sent)
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

/// The conversation `--continue` picks up: the task in this workspace last
/// worked in that has one.
///
/// The first row, because `scan` has already put the summaries in that order —
/// which is the whole reason the order is what it is. Picking by start time
/// answers a different question, and answers it wrong the moment two
/// conversations are open at once: the one you replied to is the one you are
/// in, whatever the birthdays say.
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
/// `--json` carries both `last_activity_ms` and `started_ms` for anything that
/// needs the arithmetic, or the other clock.
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
    use crate::local::state::{MessageState, RunOptions};

    /// A row that has been touched exactly once, when it started — the shape
    /// every assertion below that is not about activity wants.
    fn summary(task: &str, state: &str, started_ms: u64, agent_id: &str) -> TaskSummary {
        TaskSummary {
            task: task.to_string(),
            state: state.to_string(),
            started_ms,
            last_activity_ms: started_ms,
            prompt: "fix the failing test".to_string(),
            agent_id: agent_id.to_string(),
            usage: RunUsage::default(),
        }
    }

    fn meta(created_ms: u64, updated_ms: u64) -> TaskMeta {
        let mut meta = TaskMeta::new(
            "w/t".to_string(),
            None,
            true,
            "/repo".to_string(),
            "fix the failing test".to_string(),
            RunOptions::default(),
            None,
        );
        meta.created_ms = created_ms;
        meta.updated_ms = updated_ms;
        meta
    }

    fn message(created_ms: u64) -> MessageRecord {
        MessageRecord {
            id: format!("m{created_ms}"),
            body: "more".to_string(),
            state: MessageState::Pending,
            created_ms,
            reply: None,
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
    fn continue_takes_the_first_task_in_the_list_that_has_a_conversation() {
        let summaries = vec![
            summary("w/untouched", "resumable", 300, ""),
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

    /// The rule `--continue` rides on. Both writers count and neither owns the
    /// answer: the executor stamps `meta.json` as it works, a sender stamps
    /// the message it enqueued, and the later of the two is when this task was
    /// last a conversation somebody was in.
    #[test]
    fn activity_is_the_latest_thing_either_writer_recorded() {
        assert_eq!(
            last_activity_ms(&meta(100, 300), &[]),
            300,
            "a task nobody has written to since its last turn"
        );
        assert_eq!(
            last_activity_ms(&meta(100, 300), &[message(200), message(700)]),
            700,
            "a message sent after the last turn is the newer fact"
        );
        assert_eq!(
            last_activity_ms(&meta(100, 900), &[message(700)]),
            900,
            "and so is a turn run after the last message"
        );
        assert_eq!(
            last_activity_ms(&meta(100, 0), &[]),
            100,
            "a record written before basis kept this clock falls back to its start"
        );
    }

    /// The listing is ordered by activity, so the age it prints has to be that
    /// same activity — a column measuring one clock beside an order following
    /// another is a list that cannot be read.
    #[test]
    fn a_row_ages_by_when_it_was_last_worked_in() {
        let mut worked = summary("w/t", "succeeded", 0, "agent-1");
        worked.last_activity_ms = 7_140_000;

        let row = worked.row(7_200_000);
        assert!(
            row.contains("1m ago"),
            "not 2h, which is only how long ago it started: {row}"
        );

        let payload = worked.payload();
        assert_eq!(payload["started_ms"], 0, "the birthday survives in --json");
        assert_eq!(payload["last_activity_ms"], 7_140_000);
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
