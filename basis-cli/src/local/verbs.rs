//! The lifecycle verbs: spawn, send, ask, wait, cancel, watch, inbox.
//!
//! Every verb resolves a `<workspace-key>/<task>` handle directly to its agent
//! directory and operates on files (ADR-0019). No verb leaves a resident
//! process behind: an agent advances only while something is attached, and
//! `spawn` without `--await` prints the handle of a **resumable** agent —
//! progress happens when `basis wait` (or any attacher, backgrounded however
//! the OS backgrounds things) picks it up.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use serde_json::json;
use tokio::time::{self, Instant};
use uuid::Uuid;

use crate::{
    cli::{
        ApproveMode, AskArgs, CancelArgs, EffortArg, InboxArgs, RunArgs, SendArgs, WaitArgs,
        WatchArgs,
    },
    duration_arg::DurationArg,
    exit::EXIT_OK,
    run::prompt_from,
};

use super::{
    attach::{
        POLL, WaitOutcome, can_ask, cancel_tree, earlier_deadline, resolve, validate_approval,
        wait_for_message, wait_for_terminal,
    },
    data_dir::{DataDir, canonical_workspace, restrict_directory},
    error::{ClientError, message_timeout, wait_timeout, watch_timeout},
    events::EventTail,
    inbox as inbox_file, lock, policy,
    render::{Live, decorate_terminal, print_hint, render_result},
    state::{
        MAX_MESSAGE, MAX_PROMPT, MAX_TASKS, RunOptions, TaskMeta, cancel_requested, load_meta,
        now_ms, read_terminal, save_meta,
    },
    tasks,
};

const DEFAULT_WAIT: Duration = Duration::from_secs(30 * 60);
const MAX_WAIT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_TASK_DEADLINE_MS: u64 = 30 * 60 * 1000;
const CURRENT_TASK: &str = "BASIS_TASK_ID";

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
    if prompt.trim().is_empty() {
        return Err("prompt is empty".into());
    }
    if prompt.len() > MAX_PROMPT {
        return Err(format!(
            "prompt is {} bytes; the limit is {MAX_PROMPT}",
            prompt.len()
        )
        .into());
    }
    let options = run_options(&args);
    // Only a process that stays to drive the agent can put a question to
    // anyone, so the approval mode is validated against this route rather than
    // against the mode alone.
    validate_approval(&options.approve, attach && can_ask())?;

    let data = discover()?;
    let canonical = canonical_workspace(&workspace)
        .map_err(|error| format!("resolve workspace {}: {error}", workspace.display()))?;
    let key = data.ensure_workspace(&canonical)?;
    let caller = current_task();
    if let Some(caller) = caller.as_deref()
        && !args.detached
        && !caller.starts_with(&format!("{key}/"))
    {
        return Err(format!(
            "current task {caller} belongs to another workspace; use `basis spawn --detached ...` to start work here"
        )
        .into());
    }
    let parent = if args.detached { None } else { caller };
    // Resolved before the directory is claimed, so a refusal leaves nothing
    // behind: a task that never mints is a task nobody has to clean up.
    let continues = continuation(&data, &canonical, &key, &args)?;

    let deadline_after = options.deadline_ms.unwrap_or(DEFAULT_TASK_DEADLINE_MS);
    let requested_deadline = Some(
        now_ms()
            .checked_add(deadline_after)
            .ok_or_else(|| "task deadline exceeds the system clock range".to_string())?,
    );
    let deadline_at = match parent.as_deref() {
        Some(parent_id) => {
            let paths = resolve(&data, parent_id)
                .map_err(|_| format!("parent task {parent_id} does not exist"))?;
            let owner = load_meta(&paths)?;
            let accepts = read_terminal(&paths)?.is_none()
                && owner.pending_terminal.is_none()
                && !cancel_requested(&paths)
                && !owner.deadline_passed();
            if !accepts {
                return Err(format!("parent task {parent_id} is no longer running").into());
            }
            earlier_deadline(requested_deadline, owner.deadline_at_ms)
        }
        None => requested_deadline,
    };

    let agents = data.agents_dir(&key);
    let existing = std::fs::read_dir(&agents)
        .map_err(|error| format!("scan workspace agents: {error}"))?
        .count();
    if existing >= MAX_TASKS {
        return Err(format!(
            "workspace has {MAX_TASKS} tasks (the limit); archive old agent directories under {}",
            agents.display()
        )
        .into());
    }

    // `create_dir` is the atomic claim on the handle; a uuid collision retries.
    let (task, paths) = loop {
        let task = format!("{key}/{}", Uuid::new_v4().simple());
        let paths = data
            .agent_dir(&task)
            .expect("a minted handle is well-formed");
        match std::fs::create_dir(paths.dir()) {
            Ok(()) => {
                restrict_directory(paths.dir())
                    .map_err(|error| format!("restrict task directory: {error}"))?;
                break (task, paths);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create task directory: {error}").into()),
        }
    };
    let meta = TaskMeta::new(
        task.clone(),
        parent,
        args.detached,
        canonical.to_string_lossy().into_owned(),
        prompt,
        options,
        deadline_at,
    )
    .continuing(continues);
    save_meta(&paths, &meta)?;

    if !attach {
        let payload = json!({
            "task": task,
            "state": "resumable",
            "next": format!("basis wait {task}"),
        });
        return render_result(&payload, args.json);
    }
    let timeout = bounded_wait(args.timeout);
    let live = Live::when(!args.json);
    match wait_for_terminal(&data, &task, timeout, &live).await? {
        WaitOutcome::Terminal(terminal) => {
            live.settled(&decorate_terminal(&task, terminal), args.json)
        }
        WaitOutcome::TimedOut { attached } => Err(wait_timeout(&task, timeout, attached)),
    }
}

pub(crate) fn has_current_task() -> bool {
    current_task().is_some()
}

/// The conversation this spawn continues, when it was told to continue one.
///
/// Continuing is a *new task on an old conversation*, never a message to the
/// old one: `inbox::enqueue` refuses the moment a terminal record exists, so
/// a settled dialogue has no inbox left to reach (ADR-0019). The new task
/// therefore carries this invocation's bounds, model, and approval mode —
/// they belong to a run, and this is a new run.
///
/// Two things are refused rather than resolved. A task something is currently
/// driving keeps its conversation to itself: one executor at a time is the
/// whole point of the attach lock, and a second one resuming the same agent
/// would interleave two dialogues into one transcript. And a handle from
/// another workspace is refused by [`tasks::named`], because a conversation
/// belongs to the workspace whose context and tools produced it.
fn continuation(
    data: &DataDir,
    workspace: &Path,
    key: &str,
    args: &RunArgs,
) -> Result<Option<String>, ClientError> {
    if !args.continues_a_conversation() {
        return Ok(None);
    }
    let summaries = tasks::workspace_tasks(data, workspace)?.unwrap_or_default();
    let chosen = match args.session.as_deref() {
        Some(handle) => tasks::named(&summaries, key, handle)?,
        None => tasks::latest_conversation(&summaries).ok_or_else(|| {
            ClientError::new("no task in this workspace has a conversation to continue")
                .pointing_at("basis spawn <PROMPT>")
        })?,
    };
    if chosen.state == "running" {
        return Err(ClientError::new(format!(
            "task {} is running; its attach lock is what keeps one conversation to one executor",
            chosen.task
        ))
        .pointing_at(format!("basis wait {}", chosen.task)));
    }
    if chosen.agent_id.is_empty() {
        return Err(ClientError::new(format!(
            "task {} has no conversation yet: nothing has attached to it",
            chosen.task
        ))
        .pointing_at(format!("basis wait {}", chosen.task)));
    }
    Ok(Some(chosen.agent_id.clone()))
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
    let data = discover()?;
    let paths = resolve(&data, &task)?;
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
        let workspace = load_meta(&paths)?.workspace;
        crate::templates::resolve(&message, Path::new(&workspace))?.unwrap_or(message)
    };
    if message.len() > MAX_MESSAGE {
        return Err(format!(
            "message is {} bytes; the limit is {MAX_MESSAGE}",
            message.len()
        )
        .into());
    }
    let caller = current_task();
    // The static ownership rule is checked before the enqueue for a blocking
    // send, so a rejected wait cannot leave a message behind.
    if await_result {
        policy::validate_wait_edge(&data, caller.as_deref(), &task)?;
    }
    let message_id = inbox_file::enqueue(&paths, &task, message)?;
    if !await_result {
        let payload = json!({
            "task": task,
            "message": message_id,
            "state": "accepted",
            "next": policy::send_next_hint(&data, caller.as_deref(), &task, &message_id),
        });
        return render_result(&payload, json);
    }
    let timeout = bounded_wait(timeout);
    match wait_for_message(&data, &task, &message_id, timeout).await? {
        WaitOutcome::Terminal(payload) => render_result(&payload, json),
        WaitOutcome::TimedOut { .. } => Err(message_timeout(&task, &message_id, timeout)),
    }
}

pub(crate) async fn wait(args: WaitArgs) -> Result<ExitCode, ClientError> {
    let data = discover()?;
    let paths = resolve(&data, &args.task)?;
    let caller = current_task();
    let timeout = bounded_wait(args.timeout);

    if let Some(message_id) = args.message {
        // A reply that already exists is repeatable without any policy edge.
        let messages = inbox_file::load(&paths)?;
        let terminal = read_terminal(&paths)?;
        if let Some(payload) = inbox_file::message_payload_for_dispatch(
            &args.task,
            &messages,
            &message_id,
            terminal.as_ref(),
        )? {
            return render_result(&payload, args.json);
        }
        policy::validate_wait_edge(&data, caller.as_deref(), &args.task)?;
        return match wait_for_message(&data, &args.task, &message_id, timeout).await? {
            WaitOutcome::Terminal(payload) => render_result(&payload, args.json),
            WaitOutcome::TimedOut { .. } => Err(message_timeout(&args.task, &message_id, timeout)),
        };
    }

    // A terminal result is repeatably observable before any policy question.
    if let Some(terminal) = read_terminal(&paths)? {
        return render_result(&decorate_terminal(&args.task, terminal), args.json);
    }
    policy::validate_wait_edge(&data, caller.as_deref(), &args.task)?;
    // Waiting on an unattached agent means driving it, which puts this
    // process in exactly the seat `spawn` is in: the run is happening here,
    // so it is shown here.
    let live = Live::when(!args.json);
    match wait_for_terminal(&data, &args.task, timeout, &live).await? {
        WaitOutcome::Terminal(terminal) => {
            live.settled(&decorate_terminal(&args.task, terminal), args.json)
        }
        WaitOutcome::TimedOut { attached } => Err(wait_timeout(&args.task, timeout, attached)),
    }
}

pub(crate) async fn cancel(args: CancelArgs) -> Result<ExitCode, ClientError> {
    let data = discover()?;
    let paths = resolve(&data, &args.task)?;
    policy::validate_cancel_target(&data, current_task().as_deref(), &args.task)?;
    // Cancelling a settled task is an idempotent observation.
    if let Some(terminal) = read_terminal(&paths)? {
        return render_result(&decorate_terminal(&args.task, terminal), args.json);
    }
    cancel_tree(&data, &args.task)?;
    let payload = json!({
        "task": args.task,
        "state": "cancel_requested",
        "next": format!("basis wait {}", args.task),
    });
    render_result(&payload, args.json)
}

pub(crate) async fn watch(args: WatchArgs) -> Result<ExitCode, ClientError> {
    let data = discover()?;
    let paths = resolve(&data, &args.task)?;
    if read_terminal(&paths)?.is_none() {
        policy::validate_wait_edge(&data, current_task().as_deref(), &args.task)?;
    }
    let timeout = args
        .timeout
        .map(DurationArg::duration)
        .unwrap_or(DEFAULT_WAIT);
    let deadline = Instant::now() + timeout;
    // Replay from the start is the default: the journal is the whole story.
    let mut tail = EventTail::new(&paths, 0);
    // The same renderer the executor uses, over the same events: watching a
    // run from outside it should not look different from being the process
    // that ran it.
    let live = Live::when(!args.json);
    loop {
        let terminal = read_terminal(&paths)?;
        for record in tail
            .poll()
            .map_err(|error| format!("read task events: {error}"))?
        {
            // One schema wherever the stream surfaces: the record is already
            // the flat `EventLine` shape `basis --json` writes, whichever
            // vintage of journal it came off disk from.
            if args.json {
                println!("{record}");
            } else {
                live.show(&record)
                    .map_err(|error| format!("render task progress: {error}"))?;
            }
        }
        if let Some(terminal) = terminal {
            return live.settled(&decorate_terminal(&args.task, terminal), args.json);
        }
        if Instant::now() >= deadline {
            return Err(watch_timeout(
                &args.task,
                lock::is_held(&paths.attach_lock()),
            ));
        }
        time::sleep(POLL).await;
    }
}

pub(crate) async fn inbox(args: InboxArgs) -> Result<ExitCode, ClientError> {
    let task = args.task.or_else(current_task).ok_or_else(|| {
        "`basis inbox` needs a task id outside a basis task: use `basis inbox <ID>`".to_string()
    })?;
    let data = discover()?;
    let paths = resolve(&data, &task)?;
    let payload = inbox_file::inbox_payload(&task, &inbox_file::load(&paths)?);
    if args.json {
        println!("{payload}");
        return Ok(ExitCode::from(EXIT_OK));
    }

    let messages = payload["messages"].as_array().cloned().unwrap_or_default();
    if messages.is_empty() {
        println!("inbox is empty");
    } else {
        for message in messages {
            let state = message["state"].as_str().unwrap_or("unknown");
            let id = message["id"].as_str().unwrap_or("?");
            let body = message["body"].as_str().unwrap_or_default();
            println!("[{state}] {id}: {body}");
            if let Some(reply) = message["reply"]["result"].as_str()
                && !reply.is_empty()
            {
                println!("  reply: {reply}");
            }
        }
    }
    print_hint(&payload);
    Ok(ExitCode::from(EXIT_OK))
}

fn discover() -> Result<DataDir, String> {
    DataDir::discover().map_err(|error| format!("open task data directory: {error}"))
}

fn run_options(args: &RunArgs) -> RunOptions {
    RunOptions {
        provider: args.provider.clone(),
        base_url: args.base_url.clone(),
        model: args.model.clone(),
        no_shell: args.no_shell,
        system_prompt: args.system_prompt.clone(),
        append_system_prompt: args.append_system_prompt.clone(),
        effort: args.effort.map(effort_name).map(str::to_string),
        approve: approval_name(args.approve).to_string(),
        // An unattended owner has no human watching it. Give it a finite
        // service bound even when the attended one-shot spelling omitted one;
        // `--deadline` still narrows this explicitly.
        deadline_ms: Some(duration_ms(args.deadline.unwrap_or_else(|| {
            duration_arg(Duration::from_millis(DEFAULT_TASK_DEADLINE_MS))
        }))),
        tool_budget: args.tool_budget,
        token_budget: args.token_budget,
    }
}

fn duration_arg(duration: Duration) -> DurationArg {
    let seconds = duration.as_secs();
    let text = if seconds.is_multiple_of(86_400) {
        format!("{}d", seconds / 86_400)
    } else if seconds.is_multiple_of(3_600) {
        format!("{}h", seconds / 3_600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    };
    text.parse().expect("service default is a valid duration")
}

fn effort_name(effort: EffortArg) -> &'static str {
    match effort {
        EffortArg::Low => "low",
        EffortArg::Medium => "medium",
        EffortArg::High => "high",
        EffortArg::XHigh => "xhigh",
        EffortArg::Max => "max",
    }
}

fn approval_name(mode: ApproveMode) -> &'static str {
    match mode {
        ApproveMode::Always => "always",
        ApproveMode::Prompt => "prompt",
        ApproveMode::Never => "never",
    }
}

fn workspace_or_current(workspace: Option<PathBuf>) -> Result<PathBuf, String> {
    workspace.map_or_else(
        || std::env::current_dir().map_err(|error| format!("no working directory: {error}")),
        Ok,
    )
}

fn current_task() -> Option<String> {
    std::env::var(CURRENT_TASK)
        .ok()
        .filter(|task| !task.trim().is_empty())
}

fn duration_ms(duration: DurationArg) -> u64 {
    duration.duration().as_millis().min(u128::from(u64::MAX)) as u64
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

    #[test]
    fn spawn_records_a_finite_default_deadline() {
        // attach::drive enforces deadlines even for agents nobody attached to
        // in time; the default has to exist for that to bound anything.
        let args = <crate::cli::Cli as clap::Parser>::try_parse_from(["basis", "spawn", "p"])
            .expect("parses");
        let Some(crate::cli::Command::Spawn(args)) = args.command else {
            panic!("spawn parses");
        };
        assert_eq!(
            run_options(&args).deadline_ms,
            Some(u64::from(30_u32 * 60 * 1000))
        );
    }
}
