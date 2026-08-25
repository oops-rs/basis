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

use std::path::Path;

use basis::RunUsage;
use serde_json::{Value, json};

use crate::{
    data_dir::{AgentPaths, DataDir, canonical_workspace, valid_task_handle, workspace_key},
    inbox, lock,
    state::{MessageRecord, TaskMeta, load_meta, read_terminal},
};

/// How much of a prompt's first line a row carries. A row is an index entry,
/// not the prompt — `basis watch <ID>` has the whole run.
const PROMPT_BUDGET: usize = 64;

/// One task, as [`Tasks::list`](crate::Tasks::list) reports it and as a
/// `--continue`-shaped continuation picks from it.
#[derive(Debug, Clone)]
pub struct TaskSummary {
    pub task: String,
    /// `running`, `resumable`, or whatever the terminal record settled on.
    pub state: String,
    pub started_ms: u64,
    /// When this task was last worked in, per [`last_activity_ms`]. What the
    /// list is ordered by, and what a continuation picks by.
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
    /// The `basis list --json` row shape (ADR-0015): the fields above, plus
    /// `continuable` and, when the task spent anything, `usage`. Kept here
    /// rather than duplicated by a caller building its own object, so a
    /// script reading `--json` and a host reading this struct's JSON agree by
    /// construction.
    pub fn payload(&self) -> Value {
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

/// The task's honest state while unfinished: `running` only when a live
/// executor observably holds the attach lock, `resumable` otherwise. The same
/// two-fact derivation [`Tasks::wait`](crate::Tasks::wait) and
/// [`Tasks::watch`](crate::Tasks::watch) settle a timeout's `attached` field
/// with, so all three answer "what is this task doing" the same way.
pub fn probe_state(attached: bool) -> &'static str {
    if attached { "running" } else { "resumable" }
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

/// Why [`named`] refused a handle — the distinction its caller needs to map
/// onto its own vocabulary ([`Error::invalid_reference`](crate::Error::invalid_reference)
/// versus an ordinary failure, in `Tasks`; `ClientError::usage` versus
/// `ClientError::new` in `basis-cli`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NamedError {
    /// Malformed grammar, or a handle from another workspace — an argument
    /// that was never going to resolve here, whatever else settles.
    InvalidReference(String),
    /// Well-formed, this workspace, but no task recorded under it — a state
    /// fact (the directory is gone, or was never there), not a bad argument.
    NotFound(String),
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
) -> Result<&'a TaskSummary, NamedError> {
    let Some((key, _)) = valid_task_handle(handle) else {
        return Err(NamedError::InvalidReference(format!(
            "`{handle}` is not a task handle"
        )));
    };
    if key != workspace_key {
        return Err(NamedError::InvalidReference(format!(
            "task {handle} belongs to another workspace; list it where it was started"
        )));
    }
    summaries
        .iter()
        .find(|summary| summary.task == handle)
        .ok_or_else(|| NamedError::NotFound(format!("no task directory for {handle}")))
}

/// The prompt's first line, bounded.
fn first_line(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(PROMPT_BUDGET) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MessageState, RunOptions};

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

    /// The listing is ordered by activity, and `--json` carries that same
    /// clock alongside the birthday rather than only the one it is ordered
    /// by — a caller wanting either arithmetic gets both.
    #[test]
    fn a_json_row_carries_both_clocks() {
        let mut worked = summary("w/t", "succeeded", 0, "agent-1");
        worked.last_activity_ms = 7_140_000;

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
            matches!(error, NamedError::InvalidReference(_)),
            "{error:?}"
        );
        assert!(
            format!("{error:?}").contains("another workspace"),
            "{error:?}"
        );

        let malformed = named(&summaries, here, "not-a-handle").expect_err("refused");
        assert!(
            matches!(malformed, NamedError::InvalidReference(_)),
            "{malformed:?}"
        );
        assert!(format!("{malformed:?}").contains("not a task handle"));

        let found = named(&summaries, here, &summaries[0].task).expect("in this workspace");
        assert_eq!(found.task, summaries[0].task);
    }

    /// Well-formed and this workspace's key, but no such task is recorded —
    /// a state fact, distinct from an argument that could never have
    /// resolved: `resolve_continuation` maps this to an ordinary `Error`
    /// rather than `invalid_reference`.
    #[test]
    fn a_handle_that_fits_the_grammar_but_names_nothing_is_not_found_not_invalid() {
        let here = "0123456789abcdef";
        let missing = format!("{here}/{:032x}", 99);
        let summaries = vec![summary(&format!("{here}/{:032x}", 1), "succeeded", 1, "a")];

        let error = named(&summaries, here, &missing).expect_err("refused");
        assert!(matches!(error, NamedError::NotFound(_)), "{error:?}");
        assert!(
            format!("{error:?}").contains("no task directory"),
            "{error:?}"
        );
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
