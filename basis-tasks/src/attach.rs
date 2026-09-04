//! The attach protocol: one process becomes an agent's executor (ADR-0019).
//!
//! Attach takes the agent's `attach.lock` — one writer, ever — resumes the
//! conversation from mentra's last committed turn, executes, and checkpoints
//! at turn boundaries. `terminal.json`, written atomically as the executor's
//! **last** act, is the completion signal; an agent is resumable iff it does
//! not exist, and every crash before it resolves toward resumable.
//!
//! **Re-driving a turn may repeat its tool side effects.** A checkpoint
//! restores state, never effects — a shell command that ran, ran. A message
//! left in flight by a crash reverts to pending and is driven again.
//!
//! A parent's executor may not write its terminal record while an attached
//! child lacks one: the settle pass here is the scope rule as one ordering
//! constraint, with no resident supervisor to enforce it. The process attached
//! to a parent supervises exactly its own subtree — it drives unfinished
//! children whose locks are free and observes the ones with live executors.
//!
//! # Threading (T3, whole-wave review)
//!
//! Everything in this module that touches a lock or a file runs on tokio's
//! blocking thread pool, never on a caller's own async worker thread — the
//! same discipline G7 (`ca9ddcb`) applied to `basis`'s own memory discovery,
//! at this crate's boundary instead. Concretely:
//!
//! - [`wait_for_terminal`] and [`wait_for_message`] are themselves plain
//!   `async fn`s that never touch a lock or a file directly. Each poll
//!   iteration's work — the terminal read, the non-blocking attach probe,
//!   and (if it wins the attach) the drive itself — happens inside
//!   [`poll_once`]/[`poll_message_once`], each one `tokio::task::spawn_blocking`
//!   call. Between iterations, `tokio::time::sleep` is the only thing either
//!   function awaits directly.
//! - [`drive`]'s own model turns are real `async` work (network calls
//!   through mentra), and they run *inside* that same blocking-pool thread:
//!   `poll_once`/`poll_message_once` borrow a
//!   [`Handle`](tokio::runtime::Handle) before spawning and call
//!   [`Handle::block_on`] on it once attached, so `drive`, `run_model`,
//!   `settle`, and `settle_children`'s own recursive `drive` calls all run
//!   as one unit on that thread — none of their own lock or fs calls need a
//!   second `spawn_blocking` of their own, and nesting one would be
//!   redundant, not incorrect (they are already off any tokio worker
//!   thread). Attempting the reverse — calling `Handle::block_on` from a
//!   thread tokio is already using to drive async tasks — panics outright
//!   ("cannot start a runtime from within a runtime"), which is exactly the
//!   failure mode that makes this ordering load-bearing rather than
//!   cosmetic.
//! - [`is_attached`] (the one lock probe outside the poll loop, for a
//!   timeout's own `attached` field) gets its own small `spawn_blocking` for
//!   the same reason.
//!
//! `client.rs`'s own `blocking` helper carries the same rule for each public
//! `async fn`'s synchronous prelude (an edge check, an enqueue, `spawn`
//! itself) — see its doc.

use std::{
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use basis::{
    AllowAll, Approver, Bound, CancellationToken, DenyAll, DenyAllGate, Event, EventSink,
    PreparedRun, RunOutcome, RunSpec, Runtime, RuntimeBuilder, ShellAccess, TurnOptions, Workspace,
    WorkspaceBuilder,
};
use serde_json::Value;
use tokio::time::{self, Instant};

use crate::{
    Error,
    approve::Approve,
    data_dir::{AgentPaths, DataDir, valid_task_handle},
    events::EventLog,
    inbox,
    live::DriveContext,
    lock,
    state::{
        MAX_RESULT_BYTES, MAX_TASKS, MessageReply, TaskMeta, Terminal, TerminalRecord,
        bounded_text, cancel_requested, load_meta, now_ms, read_terminal, request_cancel,
        save_meta, write_terminal,
    },
};

/// The polling cadence everything waits at: terminal records, contended
/// locks, child settling. Bounded CPU, honest tail latency. Public so a host
/// composing its own loop around [`crate::EventCursor`] — `basis watch`'s own
/// loop, for one — polls at the same cadence this crate's own waits do.
pub const POLL: Duration = Duration::from_millis(100);

/// What a bounded wait produced: the settled payload, or a timeout with
/// enough said about it to retry sensibly.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitOutcome {
    /// The terminal payload, with its exact JSON and typed fields side by
    /// side. For `wait_for_message`, this is the correlated reply or
    /// terminal-tagged payload `message_payload_for_dispatch` resolved.
    Terminal(TerminalRecord),
    /// The bounded wait elapsed; `attached` reports whether a live executor
    /// held the lock at that moment.
    TimedOut { attached: bool },
}

/// Waits for a task's terminal record, attaching to produce it whenever the
/// lock is free. A contended lock means a live executor exists: observe.
///
/// `ctx` carries the caller's terminal, shown to while this process is the
/// one executing, and its say over `Approve::Prompt`. Nothing is shown for a
/// record that was merely read off disk: there is nothing live about a run
/// that finished before this process asked.
///
/// `timeout` is saturated into a deadline (`Instant::now() + Duration::MAX`
/// panics): a duration too large to represent as a deadline is waited
/// forever rather than refused, which is what asking for one that large
/// means.
///
/// The deadline binds the wait, not the task. Between polls it is checked
/// here; while this process is the executor it travels into [`drive`] on
/// `ctx` and is honored at every turn boundary, so a `wait` that attaches
/// to a long task gives the lock back — with no terminal record, the task
/// still resumable — once its own time is up. The one turn in flight at
/// that moment is bounded by the task's deadline, not the wait's: that is
/// ADR-0019's granularity, not a gap.
///
/// **Threading (G7, `ca9ddcb`, applied at this crate's own boundary):** this
/// function itself never touches a lock or a file — [`poll_once`] and
/// [`is_attached`] do that, each on its own `spawn_blocking` thread, so the
/// `time::sleep` between iterations is the only thing this `async fn` ever
/// awaits directly on the caller's executor.
pub(crate) async fn wait_for_terminal(
    data: &DataDir,
    task: &str,
    timeout: Duration,
    ctx: &DriveContext,
) -> Result<WaitOutcome, String> {
    let deadline = Instant::now().checked_add(timeout);
    let ctx = ctx.clone().until(deadline.map(Instant::into_std));
    loop {
        if let Some(terminal) = poll_once(data.clone(), task.to_string(), ctx.clone()).await? {
            return Ok(WaitOutcome::Terminal(TerminalRecord::from_raw(terminal)));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(WaitOutcome::TimedOut {
                attached: is_attached(data, task).await?,
            });
        }
        time::sleep(POLL).await;
    }
}

/// One [`wait_for_terminal`] iteration, entirely on a blocking thread: the
/// terminal read, the non-blocking attach probe, and — if this call wins the
/// attach — driving the task via a runtime [`Handle`](tokio::runtime::Handle)
/// borrowed for exactly that. `drive`'s own `.await`s (the model turns) still
/// run correctly under this: `Handle::block_on` drives them to completion on
/// this same blocking-pool thread rather than a tokio worker thread, which is
/// the whole point — nothing this reaches (`resolve`, `read_terminal`,
/// `try_attach`, every lock and fs read `drive`'s own call tree makes,
/// `settle_children`'s recursive `drive` calls included) ever runs on one.
///
/// `None` means the iteration made no progress (nothing to attach to yet, or
/// [`drive`] itself backed off — its conversation claimed elsewhere, or the
/// waiter's own deadline reached at a turn boundary) — indistinguishable to
/// the caller from "still running", which is exactly right: both just mean
/// check the deadline, then try again next poll.
async fn poll_once(
    data: DataDir,
    task: String,
    ctx: DriveContext,
) -> Result<Option<Value>, String> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || -> Result<Option<Value>, String> {
        let paths = resolve(&data, &task)?;
        if let Some(terminal) = read_terminal(&paths)? {
            return Ok(Some(terminal));
        }
        match try_attach(&paths)? {
            Some(guard) => handle.block_on(drive(&data, &task, guard, &ctx)),
            None => Ok(None),
        }
    })
    .await
    .unwrap_or_else(|error| Err(format!("poll task: {error}")))
}

/// Whether a live executor currently holds `task`'s attach lock — the one
/// lock probe [`wait_for_terminal`] and [`wait_for_message`] need outside
/// their own poll loop, to answer a timeout's `attached` field. On a
/// blocking thread, like every other lock touch in this module.
async fn is_attached(data: &DataDir, task: &str) -> Result<bool, String> {
    let data = data.clone();
    let task = task.to_string();
    tokio::task::spawn_blocking(move || -> Result<bool, String> {
        let paths = resolve(&data, &task)?;
        Ok(lock::is_held(&paths.attach_lock()))
    })
    .await
    .unwrap_or_else(|error| Err(format!("check attach lock: {error}")))
}

/// Waits for one correlated message reply, attaching to produce it whenever
/// the lock is free. Returns the dispatch payload (reply, or terminal tagged
/// with the message id).
///
/// Nothing is shown while it drives. The caller asked for *one message's*
/// reply, and the turns this process may have to run to reach it can belong
/// to other messages entirely — streaming them would answer a question nobody
/// asked, on the stream the answer is supposed to arrive on. `Approve::Prompt`
/// still answers through `prompt_host`, because approval and visibility are
/// independent facts.
///
/// `timeout` is saturated into a deadline, as [`wait_for_terminal`]'s is: a
/// duration too large to represent as a deadline waits forever rather than
/// panicking or refusing — and, as there, it bounds the wait rather than
/// the task: past it no further turn starts under this process, the one in
/// flight finishes under the task's own deadline, and the lock is released
/// with the message still pending for whoever attaches next.
///
/// **Threading:** as [`wait_for_terminal`] — [`poll_message_once`] carries
/// every lock and fs touch this makes onto a blocking thread; this `async fn`
/// only ever awaits that and, between iterations, `time::sleep`.
pub(crate) async fn wait_for_message(
    data: &DataDir,
    task: &str,
    message_id: &str,
    timeout: Duration,
    prompt_host: Option<Arc<dyn crate::approve::PromptHost>>,
) -> Result<WaitOutcome, String> {
    let deadline = Instant::now().checked_add(timeout);
    let ctx = DriveContext::new(None, prompt_host).until(deadline.map(Instant::into_std));
    loop {
        match poll_message_once(
            data.clone(),
            task.to_string(),
            message_id.to_string(),
            ctx.clone(),
        )
        .await?
        {
            MessagePoll::Resolved(payload) => {
                return Ok(WaitOutcome::Terminal(TerminalRecord::from_raw(payload)));
            }
            // A turn ran (for this message or another) but did not resolve
            // ours: recheck immediately, the way the pre-thread-split loop
            // did with its own `continue` — no reason to sleep when there is
            // fresh state to read.
            MessagePoll::Drove => continue,
            MessagePoll::Idle => {}
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(WaitOutcome::TimedOut {
                attached: is_attached(data, task).await?,
            });
        }
        time::sleep(POLL).await;
    }
}

/// What one [`wait_for_message`] iteration found.
enum MessagePoll {
    /// The message's dispatch payload — a reply, or a terminal record tagged
    /// with the message id.
    Resolved(Value),
    /// This iteration drove a turn (this task's, via [`drive`]) but it was
    /// not the one the caller is waiting on; state may have changed, so the
    /// next iteration should look again before waiting out the poll cadence.
    Drove,
    /// Nothing to read and nothing to attach to (or attaching was
    /// contended); the ordinary "still waiting" case.
    Idle,
}

/// One [`wait_for_message`] iteration, entirely on a blocking thread — see
/// [`poll_once`], which this is the message-scoped twin of: it checks
/// `message_id`'s own dispatch payload rather than only the task's terminal,
/// and only attaches while the task itself has no terminal yet.
async fn poll_message_once(
    data: DataDir,
    task: String,
    message_id: String,
    ctx: DriveContext,
) -> Result<MessagePoll, String> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || -> Result<MessagePoll, String> {
        let paths = resolve(&data, &task)?;
        let messages = inbox::load(&paths)?;
        let terminal = read_terminal(&paths)?;
        if let Some(payload) =
            inbox::message_payload_for_dispatch(&task, &messages, &message_id, terminal.as_ref())?
        {
            return Ok(MessagePoll::Resolved(payload));
        }
        if terminal.is_none()
            && let Some(guard) = try_attach(&paths)?
        {
            // `None` is a drive that made no progress — its conversation
            // claimed elsewhere, or the waiter's deadline reached at a turn
            // boundary — and reporting it as `Drove` would send the caller
            // straight back here to attach and yield again, never reaching
            // its own deadline check. It is `Idle`: nothing changed.
            return Ok(match handle.block_on(drive(&data, &task, guard, &ctx))? {
                Some(_) => MessagePoll::Drove,
                None => MessagePoll::Idle,
            });
        }
        Ok(MessagePoll::Idle)
    })
    .await
    .unwrap_or_else(|error| Err(format!("poll message: {error}")))
}

pub(crate) fn resolve(data: &DataDir, task: &str) -> Result<AgentPaths, String> {
    data.agent_dir(task)
        .filter(AgentPaths::exists)
        .ok_or_else(|| format!("no task directory for {task}"))
}

pub(crate) fn try_attach(paths: &AgentPaths) -> Result<Option<lock::Lock>, String> {
    lock::try_exclusive(&paths.attach_lock())
        .map_err(|error| format!("acquire task attach lock: {error}"))
}

/// Requests downward cancellation: markers for the target and every attached
/// (non-detached, non-terminal) descendant, honored at each executor's next
/// turn boundary — or at the next attach for an agent nobody holds.
pub(crate) fn cancel_tree(data: &DataDir, task: &str) -> Result<(), String> {
    let mut queue = vec![task.to_string()];
    let mut visited = 0_usize;
    while let Some(current) = queue.pop() {
        visited += 1;
        if visited > MAX_TASKS {
            break;
        }
        let Some(paths) = data.agent_dir(&current).filter(AgentPaths::exists) else {
            continue;
        };
        if read_terminal(&paths)?.is_some() {
            continue;
        }
        if !cancel_requested(&paths) {
            request_cancel(&paths, Some(task))?;
        }
        queue.extend(children_of(data, &current)?);
    }
    Ok(())
}

/// The attached (non-detached) children of `task`, terminal or not.
fn children_of(data: &DataDir, task: &str) -> Result<Vec<String>, String> {
    let Some((key, _)) = valid_task_handle(task) else {
        return Ok(Vec::new());
    };
    let agents = data.agents_dir(key);
    let entries = match std::fs::read_dir(&agents) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("scan workspace agents: {error}")),
    };
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("scan workspace agents: {error}"))?;
        let id = entry.file_name().to_string_lossy().into_owned();
        let handle = format!("{key}/{id}");
        let Some(paths) = data.agent_dir(&handle) else {
            continue;
        };
        let Ok(meta) = load_meta(&paths) else {
            continue;
        };
        if !meta.detached && meta.parent.as_deref() == Some(task) {
            children.push(handle);
        }
    }
    Ok(children)
}

/// Executes the agent to its terminal record while holding the attach lock,
/// and returns the raw terminal payload — or `None` when this attempt did
/// not settle the task: the conversation it would resume is already claimed
/// by another task's executor (see [`try_conversation`]), or the waiter
/// `ctx` drives for ran out of time at a turn boundary (see
/// [`DriveContext::until`]). Either way the lock this call did hold (this
/// task's own attach lock, dropped with `guard` on any return) is released
/// with no terminal record written, so the task is exactly as resumable as
/// before, and the caller's poll loop checks its own deadline and retries —
/// the same observe-don't-race contract a contended [`try_attach`] already
/// means to its own caller, one layer down. For the claimed-conversation
/// case that is T2(b)'s double-continuation race; for the timed-out waiter
/// it is README's "waiting is not owning".
///
/// There is no residue to accept here any more. A "…for this session"
/// approval answered during a drive used to persist in the store's
/// `rules.json` until the task's next attach, whose resume cleared it — so a
/// task nobody ever reattached kept those rows, reasons included,
/// indefinitely. mentra 0.27's `PermissionRuleScope::Process` (mentra#53) is
/// what basis's approval flow remembers into now: a rung owned by the live
/// session driving this attempt, never written to the store. It dies with
/// this process — whichever of the eight returns below ends it, deadline
/// abandonment and a crash included — and there is nothing left for a next
/// attach to clear.
pub(crate) async fn drive(
    data: &DataDir,
    task: &str,
    mut guard: lock::Lock,
    ctx: &DriveContext,
) -> Result<Option<Value>, String> {
    let paths = resolve(data, task)?;
    // Someone may have finished the task between our probe and our lock.
    if let Some(terminal) = read_terminal(&paths)? {
        return Ok(Some(terminal));
    }
    guard.write_fingerprint();
    let mut meta = load_meta(&paths)?;
    if meta.pending_terminal.is_none() {
        let turns = match existing_conversation(&meta) {
            Some(agent_id) => {
                let (key, _) = valid_task_handle(task)
                    .ok_or_else(|| format!("malformed task handle {task}"))?;
                match try_conversation(data, key, &agent_id)? {
                    Some(_conversation) => run_model(data, task, &paths, &mut meta, ctx).await?,
                    None => return Ok(None),
                }
            }
            None => run_model(data, task, &paths, &mut meta, ctx).await?,
        };
        if matches!(turns, Turns::Yielded) {
            return Ok(None);
        }
    }
    Ok(Some(settle(data, &paths, &mut meta, ctx).await?))
}

/// How [`run_model`]'s turn loop ended.
#[derive(Debug, PartialEq, Eq)]
enum Turns {
    /// A completion is recorded in `meta.pending_terminal`; the settle pass
    /// turns it into the terminal record.
    Recorded,
    /// The waiter's own deadline passed at a turn boundary with work still
    /// queued. Nothing is recorded, nothing is settled: the next attach picks
    /// the task up at exactly this boundary.
    Yielded,
}

/// The conversation this task's next turn resumes, if it resumes one at all
/// — its own prior attach, or what it was minted to continue. `None` is a
/// brand-new conversation, the only case [`try_conversation`] is skipped for:
/// nothing else can already be driving a conversation that does not exist
/// yet. The same two-branch read [`run_model`] makes of `reattached`, kept in
/// one place so `drive`'s pre-check and `run_model`'s own resume agree by
/// construction.
fn existing_conversation(meta: &TaskMeta) -> Option<String> {
    if meta.agent_id.is_empty() {
        meta.continues.clone()
    } else {
        Some(meta.agent_id.clone())
    }
}

/// Tries one conversation's lock, non-blocking — the conversation-scoped
/// counterpart of [`try_attach`]. Two tasks that both record `continues`
/// against the same agent id (T2's double-continuation race) — or, on a
/// reattach, a second process holding a stale idea of this same task — must
/// not both call `Workspace::resume` on it at once; `None` here means
/// somebody already is, and the caller's contract is to observe that, not
/// race it.
fn try_conversation(
    data: &DataDir,
    key: &str,
    agent_id: &str,
) -> Result<Option<lock::Lock>, String> {
    let path = data
        .conversation_lock(key, agent_id)
        .map_err(|error| format!("prepare conversation lock: {error}"))?;
    lock::try_exclusive(&path).map_err(|error| format!("acquire conversation lock: {error}"))
}

/// Runs the recorded work to a pending completion — or to the turn boundary
/// at which the waiter `ctx` drives for gave up ([`Turns::Yielded`]). Model,
/// configuration, and workspace failures become `Failed` completions; only
/// metadata-persistence failures propagate as errors, leaving the agent
/// resumable.
async fn run_model(
    data: &DataDir,
    task: &str,
    paths: &AgentPaths,
    meta: &mut TaskMeta,
    ctx: &DriveContext,
) -> Result<Turns, String> {
    if meta.deadline_passed() {
        return record_pending(
            paths,
            meta,
            Terminal::Failed {
                error: "task deadline elapsed before the next turn".to_string(),
            },
            Some(Bound::Deadline),
        );
    }
    // A cancel before any turn — on a never-attached or between-attaches
    // agent — settles without opening a workspace or touching the model.
    if cancel_requested(paths) {
        return record_pending(paths, meta, Terminal::Cancelled, None);
    }
    inbox::revert_in_flight(paths)?;

    let events = match EventLog::open(paths) {
        Ok(log) => Arc::new(Mutex::new(log)),
        Err(error) => {
            return record_failure(
                paths,
                meta,
                format!("open task event journal: {error}"),
                None,
            );
        }
    };
    // Ahead of the run config: the first unusable option is what the task
    // fails with, and `--provider` was read before `--effort` while both
    // halves of the options lived in one config.
    let runtime = match task_runtime(data, task, meta) {
        Ok(runtime) => runtime,
        Err(error) => return record_failure(paths, meta, error, None),
    };
    let (builder, spec) = match run_parts(meta, runtime) {
        Ok(parts) => parts,
        Err(error) => return record_failure(paths, meta, error, None),
    };
    let workspace = match builder.open().await {
        Ok(workspace) => Arc::new(workspace),
        Err(error) => return record_failure(paths, meta, error.to_string(), None),
    };
    // The run carries the workspace through the whole turn loop below, not
    // just the mint: the workspace's hook registration and MCP connections
    // end when it drops, and a task's `.basis/hooks.json` must keep its say over
    // every turn (see `PreparedRun::with_workspace`).
    //
    // Three ways to open the conversation, and only the first is a *re*-open:
    // a task that has attached before picks its own agent back up, a task
    // minted with `--continue`/`--session` picks up the one it was told to
    // continue, and everything else starts a new one. The middle case is a
    // resume to mentra and a first attach to basis — its prompt has not been
    // asked yet, which is exactly what `answered_before` below preserves.
    let reattached = !meta.agent_id.is_empty();
    let existing = existing_conversation(meta);
    let prepared = match existing.as_deref() {
        Some(agent_id) => workspace.resume(agent_id, spec),
        None => workspace.prepare(spec),
    };
    // Gated here rather than beside the per-turn `approver` below, and that is
    // forced: mentra's attachment has to be in place before the first turn, and
    // this task's recorded mode cannot change while this process drives it.
    let mut run = match prepared {
        Ok(run) => gated(run.with_workspace(workspace), meta.options.approve),
        Err(error) => return record_failure(paths, meta, error.to_string(), None),
    };
    if !reattached {
        meta.agent_id = run.agent_id().to_string();
        meta.answered_before = run.answered_turns();
        meta.updated_ms = now_ms();
        save_meta(paths, meta)?;
    }

    // Resume recovery: an assistant turn committed *past what this task
    // inherited* means the recorded prompt was already answered, and
    // re-executing it would duplicate the conversation. The last committed
    // assistant text stands in for the crashed process's unrecorded result.
    let mut initial_done = false;
    let mut last_result = String::new();
    let mut last_stopped_by: Option<Bound> = None;
    if reattached {
        initial_done = run.answered_turns() > meta.answered_before;
        if let Some(text) = run.last_assistant_text() {
            last_result = text;
        }
    }

    let cancellation = CancellationToken::default();
    loop {
        // The turn boundary: cancel markers and deadlines — the task's and
        // the waiter's — are honored here, and only here. A turn already
        // running is never cut short by any of them except the task's own
        // deadline (below, around the execution itself).
        if cancel_requested(paths) {
            return record_pending(paths, meta, Terminal::Cancelled, None);
        }
        let remaining = remaining_deadline(meta.deadline_at_ms);
        if remaining.as_ref().is_some_and(Duration::is_zero) {
            return record_pending(
                paths,
                meta,
                Terminal::Failed {
                    error: "task deadline elapsed before the next turn".to_string(),
                },
                Some(Bound::Deadline),
            );
        }
        // The waiter's deadline stops the *next turn*, not the task: with
        // one still to run (the prompt, or a pending message) this attach
        // ends here, meta saved, nothing recorded, so the next attach
        // resumes at this same boundary. With nothing left to run there is
        // no turn to withhold, and the completion below is recorded as
        // usual — a finished task is not kept unsettled by an impatient
        // observer. Checked before `start_next`, which would otherwise mark
        // a message in flight that this process is not going to drive.
        if ctx.waiter_expired() && (!initial_done || inbox::has_pending(paths)?) {
            meta.updated_ms = now_ms();
            save_meta(paths, meta)?;
            return Ok(Turns::Yielded);
        }
        let message = if initial_done {
            inbox::start_next(paths)?
        } else {
            None
        };
        if initial_done && message.is_none() {
            let (result, truncated) = bounded_text(last_result, MAX_RESULT_BYTES);
            meta.result_truncated = truncated;
            return record_pending(paths, meta, Terminal::Succeeded { result }, last_stopped_by);
        }

        let mut turn = TurnOptions::default().with_cancel(cancellation.clone());
        if let Some(remaining) = remaining {
            turn = turn.with_deadline(remaining);
        }
        let approver = match approver(meta.options.approve, ctx) {
            Ok(approver) => approver,
            Err(error) => return record_failure(paths, meta, error.to_string(), None),
        };
        let sink = FileSink {
            log: Arc::clone(&events),
            ctx: ctx.clone(),
        };
        let completed_message = message.as_ref().map(|(id, _)| id.clone());
        let execution = async {
            match message {
                Some((_, body)) => run.send_with_options(body, sink, approver, turn).await,
                None => {
                    run.execute_with_approver_and_options(sink, approver, turn)
                        .await
                }
            }
        };
        let report = match remaining {
            Some(remaining) => match time::timeout(remaining, execution).await {
                Ok(report) => report,
                Err(_) => {
                    cancellation.cancel();
                    return record_pending(
                        paths,
                        meta,
                        Terminal::Failed {
                            error: "task deadline elapsed during the turn".to_string(),
                        },
                        Some(Bound::Deadline),
                    );
                }
            },
            None => execution.await,
        };
        let report = match report {
            Ok(report) => report,
            Err(error) => return record_failure(paths, meta, error.to_string(), None),
        };
        // Banked per turn, not per attach: a task settles under one terminal
        // record but its turns may be driven by several processes, and a
        // crash between two of them must not un-spend what the first one did.
        meta.usage = meta.usage.plus(report.usage);
        meta.updated_ms = now_ms();
        save_meta(paths, meta)?;

        let stopped_by = report.stopped_by;
        match report.outcome {
            RunOutcome::Error { message } => {
                return if cancel_requested(paths) {
                    record_pending(paths, meta, Terminal::Cancelled, None)
                } else {
                    record_failure(paths, meta, message, stopped_by)
                };
            }
            RunOutcome::Ok => {
                let result = report.final_message.unwrap_or_default();
                if let Some(id) = completed_message {
                    let (reply, result_truncated) = bounded_text(result.clone(), MAX_RESULT_BYTES);
                    inbox::finish(
                        paths,
                        &id,
                        Some(MessageReply {
                            result: reply,
                            result_truncated,
                            stopped_by,
                        }),
                    )?;
                }
                initial_done = true;
                last_result = result;
                last_stopped_by = stopped_by;
            }
            // An outcome this build does not know — the enum is
            // `#[non_exhaustive]` — is recorded as the failure it is rather
            // than guessed into a success.
            outcome => {
                return record_failure(
                    paths,
                    meta,
                    format!("unrecognized run outcome: {}", outcome.type_tag()),
                    stopped_by,
                );
            }
        }
    }
}

/// Records a completion on `meta`, durably. Always [`Turns::Recorded`] on
/// success, so a turn loop can `return record_pending(..)` as its own
/// verdict.
fn record_pending(
    paths: &AgentPaths,
    meta: &mut TaskMeta,
    completion: Terminal,
    stopped_by: Option<Bound>,
) -> Result<Turns, String> {
    if matches!(completion, Terminal::Cancelled) {
        meta.result_truncated = false;
        meta.stopped_by = None;
    } else {
        meta.stopped_by = stopped_by;
    }
    meta.pending_terminal = Some(completion);
    meta.updated_ms = now_ms();
    save_meta(paths, meta)?;
    Ok(Turns::Recorded)
}

fn record_failure(
    paths: &AgentPaths,
    meta: &mut TaskMeta,
    message: String,
    stopped_by: Option<Bound>,
) -> Result<Turns, String> {
    let (error, _) = bounded_text(message, MAX_RESULT_BYTES);
    meta.result_truncated = false;
    record_pending(paths, meta, Terminal::Failed { error }, stopped_by)
}

/// The settle pass: parent scope as one ordering constraint, then two writes
/// under one hold of the inbox lock — the unanswered sweep, then the
/// terminal record, in that order and no other. A concurrent enqueue either
/// lands before the sweep or is refused by the terminal record it would
/// otherwise miss; a crash between the two writes leaves the sweep durable
/// and no terminal record, so the task is still resumable and the next
/// attach's `meta.pending_terminal` sends it straight back here — see
/// [`inbox::finish_unanswered_durably`] for why the order is the other way
/// round from how it reads.
async fn settle(
    data: &DataDir,
    paths: &AgentPaths,
    meta: &mut TaskMeta,
    ctx: &DriveContext,
) -> Result<Value, String> {
    reconsider_cancel(paths, meta)?;
    let cancel_children = !matches!(meta.pending_terminal, Some(Terminal::Succeeded { .. }));
    settle_children(data, meta, cancel_children, ctx).await?;
    // A cancel that arrived while children settled still lands before the
    // terminal record, exactly as the daemon replaced a pending completion.
    reconsider_cancel(paths, meta)?;

    let payload = meta
        .terminal_payload()
        .expect("a completion was recorded before settling");
    let _inbox_lock = inbox::finish_unanswered_durably(paths)?;
    write_terminal(paths, &payload)?;
    Ok(payload)
}

fn reconsider_cancel(paths: &AgentPaths, meta: &mut TaskMeta) -> Result<(), String> {
    if cancel_requested(paths) && !matches!(meta.pending_terminal, Some(Terminal::Cancelled)) {
        record_pending(paths, meta, Terminal::Cancelled, None)?;
    }
    Ok(())
}

/// Blocks until every attached child holds a terminal record. Children whose
/// locks are free are driven here — the attached process is the supervisor of
/// its own subtree; children with live executors are observed. A failing or
/// cancelled parent cancels its children first; a parent past its own
/// deadline stops waiting politely and cancels too (its children's deadlines
/// can only be narrower, so this converges).
async fn settle_children(
    data: &DataDir,
    meta: &TaskMeta,
    cancel_children: bool,
    ctx: &DriveContext,
) -> Result<(), String> {
    loop {
        let mut unfinished = Vec::new();
        for child in children_of(data, &meta.id)? {
            let Some(paths) = data.agent_dir(&child).filter(AgentPaths::exists) else {
                continue;
            };
            if read_terminal(&paths)?.is_none() {
                unfinished.push((child, paths));
            }
        }
        if unfinished.is_empty() {
            return Ok(());
        }
        let cancel = cancel_children || meta.deadline_passed();
        let mut remaining = false;
        for (child, paths) in unfinished {
            if cancel && !cancel_requested(&paths) {
                request_cancel(&paths, Some(&meta.id))?;
            }
            match try_attach(&paths)? {
                Some(guard) => {
                    // A child driven here is somebody else's run: this
                    // process is finishing it to keep the scope rule, not
                    // showing it to whoever asked about the parent. `None`
                    // (its conversation is claimed elsewhere) leaves it
                    // unfinished for the next pass, same as a contended
                    // attach lock.
                    if Box::pin(drive(data, &child, guard, &ctx.hidden()))
                        .await?
                        .is_none()
                    {
                        remaining = true;
                    }
                }
                None => remaining = true,
            }
        }
        if remaining {
            time::sleep(POLL).await;
        }
    }
}

/// Applies the recorded process/workspace options to the runtime recipe, then
/// builds this turn's run spec.
fn run_parts(
    meta: &TaskMeta,
    runtime: RuntimeBuilder,
) -> Result<(WorkspaceBuilder, RunSpec), String> {
    let options = &meta.options;
    let (runtime, mut builder) = crate::configure_builders(
        runtime,
        Workspace::builder(Path::new(&meta.workspace)),
        options.provider.as_deref(),
        options.base_url.as_deref(),
        options.model.as_deref(),
        ShellAccess::from_flag(!options.no_shell),
    )
    .map_err(|error| error.to_string())?;
    builder = builder.with_runtime_builder(runtime);
    // Recorded as the type it is; `load_meta` has already folded the
    // pre-0.6 two-string spelling into this one field.
    if let Some(system_prompt) = options.system_prompt.clone() {
        builder = builder.with_system_prompt(system_prompt);
    }

    let mut spec = RunSpec::new(meta.prompt.clone());
    if let Some(effort) = options.effort {
        spec = spec.with_effort(effort);
    }
    if let Some(remaining) = remaining_deadline(meta.deadline_at_ms) {
        spec = spec.with_deadline(remaining.max(Duration::from_millis(1)));
    }
    if let Some(tool_budget) = options.tool_budget {
        spec = spec.with_tool_budget(tool_budget);
    }
    if let Some(token_budget) = options.token_budget {
        spec = spec.with_token_budget(token_budget);
    }
    Ok((builder, spec))
}

/// The base recipe for this task's own runtime: store and command identity.
/// [`run_parts`] applies the recorded provider and endpoint through the same
/// concrete builder mapping as the attended CLI route.
///
/// The exported `BASIS_DATA_DIR` is absolute because `DataDir` resolves its
/// root once at construction (see `data_dir::absolutize`): a child re-reads
/// this variable from its own working directory, so a relative value here
/// would name a second data directory rather than this one.
///
/// One runtime per task (ADR-0018): the environment below names *this* task,
/// and a runtime's command environment is fixed for every workspace on it, so
/// two concurrent tasks sharing one runtime would tell their subprocesses the
/// same task id. The store lands under the workspace's key, which is what
/// lets any later process resume any agent.
fn task_runtime(data: &DataDir, task: &str, meta: &TaskMeta) -> Result<RuntimeBuilder, String> {
    let (key, _) =
        valid_task_handle(task).ok_or_else(|| format!("malformed task handle {task}"))?;
    let mut runtime = Runtime::builder()
        .with_store_dir(data.store_dir(key))
        .with_command_environment(crate::BASIS_TASK_ID, task)
        .with_command_environment(crate::BASIS_DATA_DIR, data.root().to_string_lossy());
    if let Some(parent) = &meta.parent {
        runtime = runtime.with_command_environment(crate::BASIS_PARENT_TASK_ID, parent);
    }
    Ok(runtime)
}

/// `mode`'s approver for one attach, given what this process brought to it.
///
/// Under ADR-0019 the executor is whichever process holds the attach lock, so
/// whether `Prompt` is answerable is a property of the attacher rather than
/// of the task — see [`PromptHost`](crate::approve::PromptHost).
fn approver(mode: Approve, ctx: &DriveContext) -> Result<Box<dyn Approver>, Error> {
    crate::approve::validate_approval(mode, ctx.can_ask())?;
    Ok(match mode {
        Approve::Always => Box::new(AllowAll),
        Approve::Never => Box::new(DenyAll),
        // `ctx.can_ask()` having just returned true is what makes this
        // `expect` honest: `validate` above already refused `Prompt` for a
        // context with no host, or one that cannot currently ask.
        Approve::Prompt => ctx
            .approver()
            .expect("validate confirmed a host that can ask"),
    })
}

/// The session authorizer `mode` needs over the runtime's, if any.
///
/// Only [`Approve::Never`] needs one, and this is why: mentra resolves the
/// runtime gate's `Prompt` against the conversation's remembered rules *before*
/// the [`approver`] above is consulted, so a durable Global- or Project-scope
/// allow — seeded through the session's permission handle, and outliving both
/// the attach-time clear (session scope only) and every later attach — answers
/// ahead of [`DenyAll`] and lets a task recorded to refuse write anyway.
/// [`DenyAllGate`] states the refusal where mentra treats it as final. The two
/// say the same thing in the same words; only the layer differs.
///
/// **[`Always`](Approve::Always) and [`Prompt`](Approve::Prompt) install
/// nothing**, deliberately. Both permit consequential work, so a standing allow
/// is a host saying yes in advance rather than an override of anything — and
/// installing an authorizer *replaces* whatever the runtime carries rather than
/// layering over it, which for those two would cost a bound or a posture the
/// runtime had for no gain. A refusal cannot cost either: it answers from the
/// request alone and awaits nobody, so `Prompt`'s wait on
/// [`PromptHost`](crate::approve::PromptHost) is untouched and nothing here
/// gains something new to wait on.
///
/// Read once, before the first turn, unlike [`approver`] — which is rebuilt per
/// turn because whether `Prompt` is *answerable* is a property of the attaching
/// process (ADR-0019). The mode itself is the task's, recorded at spawn, and no
/// attach can change it; a task that must switch posture is a different task.
fn gated(run: PreparedRun, mode: Approve) -> PreparedRun {
    match mode {
        Approve::Never => run.with_tool_authorizer(DenyAllGate),
        Approve::Always | Approve::Prompt => run,
    }
}

pub(crate) fn earlier_deadline(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn remaining_deadline(deadline_at: Option<u64>) -> Option<Duration> {
    deadline_at.map(|deadline| Duration::from_millis(deadline.saturating_sub(now_ms())))
}

/// The executor's event sink: every event lands in `events.jsonl`, and — when
/// a shell is waiting on this process — on that terminal as it happens.
///
/// One serialization feeds both, because the journal's shape is already the
/// shape the renderer reads — the terminal borrows it and the journal takes
/// it, so an event is never copied to be shown. Both kinds of failure are
/// swallowed: observability never fails the run, and a closed stdout says
/// nobody is reading, not that the work should stop.
struct FileSink {
    log: Arc<Mutex<EventLog>>,
    ctx: DriveContext,
}

impl EventSink for FileSink {
    fn emit(&mut self, event: Event) -> io::Result<()> {
        if let Ok(value) = serde_json::to_value(&event) {
            self.ctx.show(&value);
            if let Ok(mut log) = self.log.lock() {
                let _ = log.append(event);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
