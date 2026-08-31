//! The lifecycle verbs: spawn, send, ask, wait, cancel, watch, inbox.
//!
//! Every verb adapts a [`basis_tasks::Tasks`] call to this binary's grammar
//! and JSON shapes (ADR-0015): parsing a CLI argument into the library's
//! typed request, mapping its `Error` onto [`ClientError`]'s exit codes, and
//! rendering the result. The durable rules — the inbox, the wait-edge policy,
//! the attach lock — are `basis-tasks`'s; nothing here re-derives them.

use std::{path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use basis_tasks::{Continuation, MessageState, RunSpec, TaskHandle, Tasks, WaitOutcome};
use serde_json::json;

use crate::{
    cli::{AskArgs, CancelArgs, InboxArgs, RunArgs, SendArgs, WaitArgs, WatchArgs},
    duration_arg::DurationArg,
    exit::EXIT_OK,
    run::prompt_from,
};

use super::{
    error::{ClientError, message_timeout, wait_timeout, watch_timeout},
    prompt_host::CliPromptHost,
    render::{Live, decorate_terminal, print_hint, render_result, render_terminal},
};

const DEFAULT_WAIT: Duration = Duration::from_secs(30 * 60);
const MAX_WAIT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// `attach` is [`Route::Attach`](crate::route::Route::Attach): this process
/// drives the agent it just minted and prints its terminal result. Without it
/// the handle comes straight back and the agent waits for an attacher.
///
/// An attached run is shown as it happens unless `--json` asked for the
/// settled object instead. Nothing else decides it: the person who typed a
/// prompt at a shell is blocked on this process either way, and a run that
/// renders nothing until it ends is indistinguishable from one that hung.
pub(crate) async fn spawn(args: RunArgs, attach: bool) -> Result<ExitCode, ClientError> {
    let workspace = workspace_or_current(args.workspace.clone())?;
    let prompt = prompt_from(args.prompt.clone())?;
    let approve: basis_tasks::Approve = args.approve.into();
    // `Tasks::spawn` refuses `Approve::Prompt` itself when this `Tasks` has
    // no way to answer it — the same check this used to make here, ahead of
    // it, against `attach` rather than against whether this `Tasks` can ever
    // ask at all. Nothing left to duplicate.
    let spec = run_spec(&args, prompt, approve)?;

    let tasks = open_tasks(workspace)?;

    if !attach {
        let handle = tasks.spawn(spec)?;
        let payload = json!({
            "task": handle,
            "state": "resumable",
            "next": format!("basis wait {handle}"),
        });
        return render_result(&payload, args.json);
    }
    let timeout = bounded_wait(args.timeout);
    let live = Live::when(!args.json);
    // Not `Tasks::wait`: the edge between this call and the task it just
    // minted is the one `spawn` established a moment ago, not one to
    // re-derive from `BASIS_TASK_ID` a second time — see
    // `Tasks::spawn_and_wait`.
    let (handle, outcome) = tasks
        .spawn_and_wait(spec, timeout, Some(live_sink(live.clone())))
        .await?;
    match outcome {
        WaitOutcome::Terminal(terminal) => {
            let terminal = decorate_terminal(handle.as_ref(), terminal);
            live.settled(&terminal, args.json)
        }
        WaitOutcome::TimedOut { attached } => Err(wait_timeout(handle.as_ref(), timeout, attached)),
    }
}

/// The [`RunSpec`] one `spawn` invocation asks for: the CLI flags, turned into
/// the typed builder — clap has already refused the spellings that would
/// conflict (`--continue` with `--session`, both system-prompt flags at once).
fn run_spec(
    args: &RunArgs,
    prompt: String,
    approve: basis_tasks::Approve,
) -> Result<RunSpec, ClientError> {
    let mut spec = RunSpec::new(prompt).with_approve(approve);
    if let Some(provider) = &args.provider {
        spec = spec.with_provider(provider.clone());
    }
    if let Some(base_url) = &args.base_url {
        spec = spec.with_base_url(base_url.clone());
    }
    if let Some(model) = &args.model {
        spec = spec.with_model(model.clone());
    }
    if args.no_shell {
        spec = spec.without_shell();
    }
    if let Some(system_prompt) = crate::cli::system_prompt(
        args.system_prompt.clone(),
        args.append_system_prompt.clone(),
    ) {
        spec = spec.with_system_prompt(system_prompt);
    }
    if let Some(effort) = args.effort {
        spec = spec.with_effort(effort.into());
    }
    // An unattended owner has no human watching it. `basis-tasks` gives every
    // task a finite default deadline on its own; `--deadline` only narrows it.
    if let Some(deadline) = args.deadline {
        spec = spec.with_deadline(deadline.duration());
    }
    if let Some(tool_budget) = args.tool_budget {
        spec = spec.with_tool_budget(tool_budget);
    }
    if let Some(token_budget) = args.token_budget {
        spec = spec.with_token_budget(token_budget);
    }
    if args.detached {
        spec = spec.detached();
    }
    if args.continue_latest {
        spec = spec.continuing(Continuation::Latest);
    } else if let Some(session) = &args.session {
        // A malformed `--session` is an argument no amount of waiting fixes.
        let handle = TaskHandle::parse(session.clone()).map_err(|_| {
            ClientError::usage(format!(
                "`{session}` is not a task handle; `basis list` prints them"
            ))
        })?;
        spec = spec.continuing(Continuation::Named(handle));
    }
    Ok(spec)
}

pub(crate) fn has_current_task() -> bool {
    basis_tasks::current_task().is_some()
}

pub(crate) async fn send(args: SendArgs) -> Result<ExitCode, ClientError> {
    send_message(
        args.task,
        args.message,
        args.await_result,
        args.timeout,
        args.json,
    )
    .await
}

pub(crate) async fn ask(args: AskArgs) -> Result<ExitCode, ClientError> {
    send_message(args.task, args.message, true, args.timeout, args.json).await
}

async fn send_message(
    task: String,
    raw_message: String,
    await_result: bool,
    timeout: Option<DurationArg>,
    json: bool,
) -> Result<ExitCode, ClientError> {
    let handle = TaskHandle::parse(task.clone())?;
    let tasks = open_tasks(current_dir()?)?;
    // A follow-up is a prompt, so `/name` means here what it means at spawn.
    // The workspace is the one the task recorded rather than the one this
    // shell happens to be standing in: the templates a conversation can be
    // sent are the templates it ran with. `-` is stdin, and stdin is the
    // escape for a message that begins with a literal `/`.
    let from_stdin = raw_message == "-";
    let message = prompt_from(raw_message)?;
    let message = if from_stdin {
        message
    } else {
        let workspace = tasks.workspace_of(&handle)?;
        crate::templates::resolve(&message, &workspace)?.unwrap_or(message)
    };

    let caller = basis_tasks::current_task();
    if !await_result {
        let message_id = tasks.send(&handle, message)?;
        let payload = json!({
            "task": task,
            "message": message_id,
            "state": "accepted",
            "next": send_next_hint(&tasks, caller.as_ref(), &handle, &message_id),
        });
        return render_result(&payload, json);
    }
    let timeout = bounded_wait(timeout);
    let reply = tasks
        .ask(&handle, caller.as_ref(), message, timeout)
        .await?;
    match reply.outcome {
        WaitOutcome::Terminal(record) => render_terminal(&record, json),
        WaitOutcome::TimedOut { .. } => Err(message_timeout(&task, &reply.message_id, timeout)),
    }
}

/// A next action that is legal for the submitting task. Enqueue-only sends
/// never block; when the target is an ancestor, peer, or self, this suggests
/// inspecting the target's inbox instead of an impossible `basis wait` edge.
fn send_next_hint(
    tasks: &Tasks,
    caller: Option<&TaskHandle>,
    target: &TaskHandle,
    message: &str,
) -> String {
    if tasks.validate_wait_edge(caller, target).is_ok() {
        format!("basis wait {target} --message {message}")
    } else {
        format!("basis inbox {target}")
    }
}

pub(crate) async fn wait(args: WaitArgs) -> Result<ExitCode, ClientError> {
    let handle = TaskHandle::parse(args.task.clone())?;
    let tasks = open_tasks(current_dir()?)?;
    let caller = basis_tasks::current_task();
    let timeout = bounded_wait(args.timeout);

    if let Some(message_id) = args.message {
        return match tasks
            .wait_message(&handle, caller.as_ref(), &message_id, timeout)
            .await?
        {
            WaitOutcome::Terminal(record) => render_terminal(&record, args.json),
            WaitOutcome::TimedOut { .. } => Err(message_timeout(&args.task, &message_id, timeout)),
        };
    }

    // A terminal result is repeatably observable before any policy question.
    if let Some(terminal) = tasks.terminal(&handle)? {
        let terminal = decorate_terminal(&args.task, terminal);
        return render_terminal(&terminal, args.json);
    }
    // Waiting on an unattached agent means driving it, which puts this
    // process in exactly the seat `spawn` is in: the run is happening here,
    // so it is shown here.
    let live = Live::when(!args.json);
    match tasks
        .wait(
            &handle,
            caller.as_ref(),
            timeout,
            Some(live_sink(live.clone())),
        )
        .await?
    {
        WaitOutcome::Terminal(terminal) => {
            let terminal = decorate_terminal(&args.task, terminal);
            live.settled(&terminal, args.json)
        }
        WaitOutcome::TimedOut { attached } => Err(wait_timeout(&args.task, timeout, attached)),
    }
}

pub(crate) async fn cancel(args: CancelArgs) -> Result<ExitCode, ClientError> {
    let handle = TaskHandle::parse(args.task.clone())?;
    let tasks = open_tasks(current_dir()?)?;
    let caller = basis_tasks::current_task();
    // The policy refusal wins over idempotent observation: a caller with no
    // authority over the target hears about that even when it has already
    // settled, rather than being shown the settled record as if the
    // cancellation it had no standing to ask for had happened.
    tasks.validate_cancel_target(caller.as_ref(), &handle)?;
    if let Some(terminal) = tasks.terminal(&handle)? {
        let terminal = decorate_terminal(&args.task, terminal);
        return render_terminal(&terminal, args.json);
    }
    tasks.cancel(&handle, caller.as_ref())?;
    let payload = json!({
        "task": args.task,
        "state": "cancel_requested",
        "next": format!("basis wait {}", args.task),
    });
    render_result(&payload, args.json)
}

pub(crate) async fn watch(args: WatchArgs) -> Result<ExitCode, ClientError> {
    let handle = TaskHandle::parse(args.task.clone())?;
    let tasks = open_tasks(current_dir()?)?;
    if tasks.terminal(&handle)?.is_none() {
        tasks.validate_wait_edge(basis_tasks::current_task().as_ref(), &handle)?;
    }
    let timeout = args
        .timeout
        .map(DurationArg::duration)
        .unwrap_or(DEFAULT_WAIT);
    let deadline = tokio::time::Instant::now() + timeout;
    // Replay from the start is the default: the journal is the whole story.
    let mut cursor = tasks.watch(&handle)?;
    // The same renderer the executor uses, over the same events: watching a
    // run from outside it should not look different from being the process
    // that ran it.
    let live = Live::when(!args.json);
    loop {
        let terminal = tasks.terminal(&handle)?;
        for record in cursor.poll()? {
            // One schema wherever the stream surfaces: `record.raw` is
            // already the flat `EventLine` shape `basis --json` writes,
            // whichever vintage of journal it came off disk from.
            if args.json {
                println!("{}", record.raw);
            } else {
                live.show(&record.raw)
                    .map_err(|error| format!("render task progress: {error}"))?;
            }
        }
        if let Some(terminal) = terminal {
            let terminal = decorate_terminal(&args.task, terminal);
            return live.settled(&terminal, args.json);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(watch_timeout(&args.task, tasks.is_attached(&handle)?));
        }
        tokio::time::sleep(basis_tasks::POLL).await;
    }
}

pub(crate) async fn inbox(args: InboxArgs) -> Result<ExitCode, ClientError> {
    let task = args
        .task
        .or_else(|| basis_tasks::current_task().map(|handle| handle.to_string()))
        .ok_or_else(|| {
            "`basis inbox` needs a task id outside a basis task: use `basis inbox <ID>`".to_string()
        })?;
    let handle = TaskHandle::parse(task.clone())?;
    let tasks = open_tasks(current_dir()?)?;
    let record = tasks.inbox(&handle)?;
    if args.json {
        println!("{}", record.raw);
        return Ok(ExitCode::from(EXIT_OK));
    }

    if record.messages.is_empty() {
        println!("inbox is empty");
    } else {
        for message in &record.messages {
            let state = match message.state {
                MessageState::Pending => "pending",
                MessageState::InFlight => "in_flight",
                MessageState::Delivered => "delivered",
            };
            println!("[{state}] {}: {}", message.id, message.body);
            if let Some(reply) = &message.reply
                && !reply.result.is_empty()
            {
                println!("  reply: {}", reply.result);
            }
        }
    }
    print_hint(&record.raw);
    Ok(ExitCode::from(EXIT_OK))
}

/// Wraps a [`Live`] as the [`basis_tasks::LiveSink`] `Tasks::wait` shows
/// progress through, while keeping the original to read `.answered()` and
/// call `.settled()` on afterwards — the two share the same underlying flag.
fn live_sink(live: Live) -> Arc<dyn basis_tasks::LiveSink> {
    Arc::new(live)
}

/// Opens the durable task store with this binary's own say over
/// `Approve::Prompt` — the terminal that ran `basis "…"` or `basis wait <ID>`
/// is the one that gets asked (ADR-0020). Every verb that might attach and
/// drive a task goes through this rather than `Tasks::open` directly, so none
/// of them can forget it.
fn open_tasks(workspace: PathBuf) -> Result<Tasks, ClientError> {
    Ok(Tasks::open(workspace)
        .map_err(ClientError::from)?
        .with_prompt_host(Arc::new(CliPromptHost)))
}

fn current_dir() -> Result<PathBuf, ClientError> {
    std::env::current_dir().map_err(|error| format!("no working directory: {error}").into())
}

fn workspace_or_current(workspace: Option<PathBuf>) -> Result<PathBuf, ClientError> {
    workspace.map_or_else(current_dir, Ok)
}

/// Client waits default to 30 minutes and are clamped to a week, exactly the
/// bounds the daemon enforced on untrusted timeouts.
fn bounded_wait(timeout: Option<DurationArg>) -> Duration {
    timeout
        .map(|timeout| timeout.duration().min(MAX_WAIT))
        .unwrap_or(DEFAULT_WAIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_waits_are_defaulted_and_clamped() {
        assert_eq!(bounded_wait(None), DEFAULT_WAIT);
        assert_eq!(
            bounded_wait(Some("30s".parse().unwrap())),
            Duration::from_secs(30)
        );
        assert_eq!(bounded_wait(Some("30d".parse().unwrap())), MAX_WAIT);
    }
}
