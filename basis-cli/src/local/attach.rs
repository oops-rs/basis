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

use std::{
    io::{self, IsTerminal},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use basis::{
    AllowAll, Approver, Bound, CancellationToken, DenyAll, Effort, Event, EventSink, ModelSelector,
    RunConfig, RunOutcome, Runtime, RuntimeBuilder, ShellAccess, TurnOptions, provider,
};
use serde_json::Value;
use tokio::time::{self, Instant};

use crate::approver::TerminalApprover;

use super::{
    data_dir::{AgentPaths, DataDir, valid_task_handle},
    events::EventLog,
    inbox, lock,
    render::Live,
    state::{
        MAX_RESULT_BYTES, MAX_TASKS, MessageReply, PendingTerminal, TaskMeta, bounded_text,
        cancel_requested, load_meta, now_ms, read_terminal, request_cancel, save_meta,
        write_terminal,
    },
};

/// The polling cadence everything waits at: terminal records, contended
/// locks, child settling. Bounded CPU, honest tail latency.
pub(crate) const POLL: Duration = Duration::from_millis(100);

pub(crate) enum WaitOutcome {
    /// The raw terminal payload, as `terminal.json` holds it.
    Terminal(Value),
    /// The bounded wait elapsed; `attached` reports whether a live executor
    /// held the lock at that moment.
    TimedOut { attached: bool },
}

/// Waits for a task's terminal record, attaching to produce it whenever the
/// lock is free. A contended lock means a live executor exists: observe.
///
/// `live` is the caller's terminal, shown to while this process is the one
/// executing. It stays silent for a record that was merely read off disk:
/// there is nothing live about a run that finished before this process asked.
pub(crate) async fn wait_for_terminal(
    data: &DataDir,
    task: &str,
    timeout: Duration,
    live: &Live,
) -> Result<WaitOutcome, String> {
    let paths = resolve(data, task)?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(terminal) = read_terminal(&paths)? {
            return Ok(WaitOutcome::Terminal(terminal));
        }
        if let Some(guard) = try_attach(&paths)? {
            return Ok(WaitOutcome::Terminal(drive(data, task, guard, live).await?));
        }
        if Instant::now() >= deadline {
            return Ok(WaitOutcome::TimedOut {
                attached: lock::is_held(&paths.attach_lock()),
            });
        }
        time::sleep(POLL).await;
    }
}

/// Waits for one correlated message reply, attaching to produce it whenever
/// the lock is free. Returns the dispatch payload (reply, or terminal tagged
/// with the message id).
///
/// Nothing is shown while it drives. The caller asked for *one message's*
/// reply, and the turns this process may have to run to reach it can belong
/// to other messages entirely — streaming them would answer a question nobody
/// asked, on the stream the answer is supposed to arrive on.
pub(crate) async fn wait_for_message(
    data: &DataDir,
    task: &str,
    message_id: &str,
    timeout: Duration,
) -> Result<WaitOutcome, String> {
    let paths = resolve(data, task)?;
    let deadline = Instant::now() + timeout;
    loop {
        let messages = inbox::load(&paths)?;
        let terminal = read_terminal(&paths)?;
        if let Some(payload) =
            inbox::message_payload_for_dispatch(task, &messages, message_id, terminal.as_ref())?
        {
            return Ok(WaitOutcome::Terminal(payload));
        }
        if terminal.is_none()
            && let Some(guard) = try_attach(&paths)?
        {
            drive(data, task, guard, &Live::hidden()).await?;
            continue;
        }
        if Instant::now() >= deadline {
            return Ok(WaitOutcome::TimedOut {
                attached: lock::is_held(&paths.attach_lock()),
            });
        }
        time::sleep(POLL).await;
    }
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
/// and returns the raw terminal payload. The lock is released on return.
pub(crate) async fn drive(
    data: &DataDir,
    task: &str,
    mut guard: lock::Lock,
    live: &Live,
) -> Result<Value, String> {
    let paths = resolve(data, task)?;
    // Someone may have finished the task between our probe and our lock.
    if let Some(terminal) = read_terminal(&paths)? {
        return Ok(terminal);
    }
    guard.write_fingerprint();
    let mut meta = load_meta(&paths)?;
    if meta.pending_terminal.is_none() {
        run_model(data, task, &paths, &mut meta, live).await?;
    }
    settle(data, &paths, &mut meta).await
}

/// Runs the recorded work to a pending completion. Model, configuration, and
/// workspace failures become `Failed` completions; only metadata-persistence
/// failures propagate as errors, leaving the agent resumable.
async fn run_model(
    data: &DataDir,
    task: &str,
    paths: &AgentPaths,
    meta: &mut TaskMeta,
    live: &Live,
) -> Result<(), String> {
    if meta.deadline_passed() {
        return record_pending(
            paths,
            meta,
            PendingTerminal::Failed {
                error: "task deadline elapsed before the next turn".to_string(),
            },
            Some("deadline".to_string()),
        );
    }
    // A cancel before any turn — on a never-attached or between-attaches
    // agent — settles without opening a workspace or touching the model.
    if cancel_requested(paths) {
        return record_pending(paths, meta, PendingTerminal::Cancelled, None);
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
    let config = match run_config(meta) {
        Ok(config) => config,
        Err(error) => return record_failure(paths, meta, error, None),
    };
    let (builder, spec) = config.split();
    let workspace = match builder.with_runtime_builder(runtime).open().await {
        Ok(workspace) => Arc::new(workspace),
        Err(error) => return record_failure(paths, meta, error.to_string(), None),
    };
    // The run carries the workspace through the whole turn loop below, not
    // just the mint: the workspace's hook registration and MCP connections
    // end when it drops, and a task's `.basis/hooks.json` must keep its say over
    // every turn (see `PreparedRun::with_workspace`).
    let resumed = !meta.agent_id.is_empty();
    let prepared = if resumed {
        workspace.resume(&meta.agent_id, spec)
    } else {
        workspace.prepare(spec)
    };
    let mut run = match prepared {
        Ok(run) => run.with_workspace(workspace),
        Err(error) => return record_failure(paths, meta, error.to_string(), None),
    };
    if !resumed {
        meta.agent_id = run.agent_id().to_string();
        meta.updated_ms = now_ms();
        save_meta(paths, meta)?;
    }

    // Resume recovery: a committed assistant turn means the recorded prompt
    // was already answered; re-executing it would duplicate the conversation.
    // The last committed assistant text stands in for the crashed process's
    // unrecorded result.
    let mut initial_done = false;
    let mut last_result = String::new();
    let mut last_stopped_by: Option<String> = None;
    if resumed {
        for message in run.history() {
            if matches!(message.role, mentra::Role::Assistant) {
                initial_done = true;
                last_result = message.text();
            }
        }
    }

    let cancellation = CancellationToken::default();
    loop {
        // The turn boundary: cancel markers and deadlines are honored here.
        if cancel_requested(paths) {
            return record_pending(paths, meta, PendingTerminal::Cancelled, None);
        }
        let remaining = remaining_deadline(meta.deadline_at_ms);
        if remaining.as_ref().is_some_and(Duration::is_zero) {
            return record_pending(
                paths,
                meta,
                PendingTerminal::Failed {
                    error: "task deadline elapsed before the next turn".to_string(),
                },
                Some("deadline".to_string()),
            );
        }
        let message = if initial_done {
            inbox::start_next(paths)?
        } else {
            None
        };
        if initial_done && message.is_none() {
            let (result, truncated) = bounded_text(last_result, MAX_RESULT_BYTES);
            meta.result_truncated = truncated;
            return record_pending(
                paths,
                meta,
                PendingTerminal::Succeeded { result },
                last_stopped_by,
            );
        }

        let mut turn = TurnOptions::default().with_cancel(cancellation.clone());
        if let Some(remaining) = remaining {
            turn = turn.with_deadline(remaining);
        }
        let approver = match approver(&meta.options.approve) {
            Ok(approver) => approver,
            Err(error) => return record_failure(paths, meta, error, None),
        };
        let sink = FileSink {
            log: Arc::clone(&events),
            live: live.clone(),
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
                        PendingTerminal::Failed {
                            error: "task deadline elapsed during the turn".to_string(),
                        },
                        Some("deadline".to_string()),
                    );
                }
            },
            None => execution.await,
        };
        let report = match report {
            Ok(report) => report,
            Err(error) => return record_failure(paths, meta, error.to_string(), None),
        };
        let stopped_by = report.stopped_by.map(bound_name);
        match report.outcome {
            RunOutcome::Error { message } => {
                return if cancel_requested(paths) {
                    record_pending(paths, meta, PendingTerminal::Cancelled, None)
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
                            stopped_by: stopped_by.clone(),
                        }),
                    )?;
                }
                initial_done = true;
                last_result = result;
                last_stopped_by = stopped_by;
            }
        }
    }
}

fn record_pending(
    paths: &AgentPaths,
    meta: &mut TaskMeta,
    completion: PendingTerminal,
    stopped_by: Option<String>,
) -> Result<(), String> {
    if matches!(completion, PendingTerminal::Cancelled) {
        meta.result_truncated = false;
        meta.stopped_by = None;
    } else {
        meta.stopped_by = stopped_by;
    }
    meta.pending_terminal = Some(completion);
    meta.updated_ms = now_ms();
    save_meta(paths, meta)
}

fn record_failure(
    paths: &AgentPaths,
    meta: &mut TaskMeta,
    message: String,
    stopped_by: Option<String>,
) -> Result<(), String> {
    let (error, _) = bounded_text(message, MAX_RESULT_BYTES);
    meta.result_truncated = false;
    record_pending(paths, meta, PendingTerminal::Failed { error }, stopped_by)
}

/// The settle pass: parent scope as one ordering constraint, then the
/// terminal record. The terminal write happens under the inbox lock so a
/// concurrent enqueue either lands before the unanswered sweep or is refused
/// by the terminal record it would otherwise miss.
async fn settle(data: &DataDir, paths: &AgentPaths, meta: &mut TaskMeta) -> Result<Value, String> {
    reconsider_cancel(paths, meta)?;
    let cancel_children = !matches!(
        meta.pending_terminal,
        Some(PendingTerminal::Succeeded { .. })
    );
    settle_children(data, meta, cancel_children).await?;
    // A cancel that arrived while children settled still lands before the
    // terminal record, exactly as the daemon replaced a pending completion.
    reconsider_cancel(paths, meta)?;

    let payload = meta
        .terminal_payload()
        .expect("a completion was recorded before settling");
    inbox::update(paths, |messages| {
        inbox::finish_unanswered(messages);
        write_terminal(paths, &payload)
    })?;
    Ok(payload)
}

fn reconsider_cancel(paths: &AgentPaths, meta: &mut TaskMeta) -> Result<(), String> {
    if cancel_requested(paths) && !matches!(meta.pending_terminal, Some(PendingTerminal::Cancelled))
    {
        record_pending(paths, meta, PendingTerminal::Cancelled, None)?;
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
                    // showing it to whoever asked about the parent.
                    Box::pin(drive(data, &child, guard, &Live::hidden())).await?;
                }
                None => remaining = true,
            }
        }
        if remaining {
            time::sleep(POLL).await;
        }
    }
}

/// The per-run and per-workspace half of the recorded options.
///
/// The provider and the base URL are the other half — process facts since
/// ADR-0018 — and are stated on [`task_runtime`]'s recipe instead. Saying them
/// here as well would build a value that
/// [`with_runtime_builder`](basis::WorkspaceBuilder::with_runtime_builder)
/// then replaces.
fn run_config(meta: &TaskMeta) -> Result<RunConfig, String> {
    let options = &meta.options;
    let mut config = RunConfig::new(Path::new(&meta.workspace), meta.prompt.clone())
        .with_shell(ShellAccess::from_flag(!options.no_shell));
    if let Some(model) = &options.model {
        config = config.with_model(ModelSelector::Id(model.clone()));
    }
    if let Some(effort) = options.effort.as_deref() {
        config = config.with_effort(parse_effort(effort)?);
    }
    if let Some(remaining) = remaining_deadline(meta.deadline_at_ms) {
        config = config.with_deadline(remaining.max(Duration::from_millis(1)));
    }
    if let Some(tool_budget) = options.tool_budget {
        config = config.with_tool_budget(tool_budget);
    }
    if let Some(token_budget) = options.token_budget {
        config = config.with_token_budget(token_budget);
    }
    Ok(config)
}

/// The recipe for this task's own runtime: the process half of the recorded
/// options, plus the identity a spawned command needs to find the same data
/// directory and name its own children.
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
        .with_command_environment("BASIS_TASK_ID", task)
        .with_command_environment("BASIS_DATA_DIR", data.root().to_string_lossy());
    if let Some(name) = &meta.options.provider {
        runtime = runtime.with_provider(provider::parse(name).map_err(|error| error.to_string())?);
    }
    if let Some(base_url) = &meta.options.base_url {
        runtime = runtime.with_base_url(base_url);
    }
    if let Some(parent) = &meta.parent {
        runtime = runtime.with_command_environment("BASIS_PARENT_TASK_ID", parent);
    }
    Ok(runtime)
}

fn parse_effort(value: &str) -> Result<Effort, String> {
    match value {
        "low" => Ok(Effort::Low),
        "medium" => Ok(Effort::Medium),
        "high" => Ok(Effort::High),
        "xhigh" => Ok(Effort::XHigh),
        "max" => Ok(Effort::Max),
        value => Err(format!("unsupported effort `{value}`")),
    }
}

/// Whether this process can put an approval question to a person.
///
/// Under ADR-0019 the executor is whichever process holds the attach lock, so
/// this is a property of the attacher rather than of the agent: the terminal
/// that ran `basis "…"` or `basis wait <ID>` is the one that gets asked.
pub(crate) fn can_ask() -> bool {
    std::io::stdin().is_terminal()
}

/// `interactive` is whether the caller will be driving the agent *and* has a
/// terminal to ask at. Both halves matter: a `--resumable` agent has no
/// attacher yet, and an attacher reading from a pipe has nobody to ask.
pub(crate) fn validate_approval(value: &str, interactive: bool) -> Result<(), String> {
    match value {
        "always" | "never" => Ok(()),
        "prompt" if interactive => Ok(()),
        "prompt" => Err(
            "`--approve prompt` needs a terminal on the process driving the agent; use `always` or `never` for work nobody is attached to"
                .to_string(),
        ),
        value => Err(format!("unsupported approval mode `{value}`")),
    }
}

fn approver(value: &str) -> Result<Box<dyn Approver>, String> {
    validate_approval(value, can_ask())?;
    match value {
        "always" => Ok(Box::new(AllowAll)),
        "never" => Ok(Box::new(DenyAll)),
        // `TerminalApprover` refuses on its own if the terminal disappears
        // between this check and the question, so the fallback stays safe.
        "prompt" => Ok(Box::new(TerminalApprover::new())),
        _ => unreachable!("validated above"),
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

fn bound_name(bound: Bound) -> String {
    match bound {
        Bound::Deadline => "deadline",
        Bound::ToolBudget => "tool_budget",
        Bound::TokenBudget => "token_budget",
        _ => "unknown",
    }
    .to_string()
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
    live: Live,
}

impl EventSink for FileSink {
    fn emit(&mut self, event: Event) -> io::Result<()> {
        if let Ok(value) = serde_json::to_value(event) {
            let _ = self.live.show(&value);
            if let Ok(mut log) = self.log.lock() {
                let _ = log.append(value);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
