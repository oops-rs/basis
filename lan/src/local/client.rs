//! Command-line clients for the local lifecycle service.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use serde_json::Value;
use tokio::time::Instant;

use crate::{
    cli::{ApproveMode, CancelArgs, EffortArg, InboxArgs, RunArgs, SendArgs, WaitArgs, WatchArgs},
    duration_arg::DurationArg,
    exit::{EXIT_BOUNDED, EXIT_FAILED, EXIT_OK},
    run::prompt_from,
};

use super::{
    protocol::{Operation, Response, ResponseKind, RunOptions},
    registry::{Descriptor, Registry, probe, request},
};

const DEFAULT_WAIT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_TASK_DEADLINE: Duration = Duration::from_secs(30 * 60);
const WATCH_POLL: Duration = Duration::from_secs(30);
const CURRENT_TASK: &str = "LAN_TASK_ID";

pub(crate) async fn spawn(args: RunArgs) -> Result<ExitCode, String> {
    let workspace = workspace_or_current(args.workspace.clone())?;
    let prompt = prompt_from(args.prompt.clone())?;
    if prompt.len() > super::protocol::MAX_PROMPT {
        return Err(format!(
            "prompt is {} bytes; the limit is {}",
            prompt.len(),
            super::protocol::MAX_PROMPT
        ));
    }
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = registry.ensure_daemon(&workspace).await?;
    let caller = current_task();
    let parent = if args.detached { None } else { caller.clone() };
    let operation = Operation::Spawn {
        workspace: workspace.to_string_lossy().into_owned(),
        prompt,
        parent,
        caller,
        detached: args.detached,
        await_result: args.await_result,
        timeout_ms: args.timeout.map(duration_ms),
        options: Box::new(run_options(&args)),
    };
    let payload = checked(request(&descriptor, operation).await?)?;
    render_result(&payload, args.json)
}

pub(crate) fn has_current_task() -> bool {
    current_task().is_some()
}

pub(crate) async fn send(args: SendArgs) -> Result<ExitCode, String> {
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = live_descriptor(&registry, &args.task).await?;
    let message = prompt_from(args.message)?;
    if message.len() > super::protocol::MAX_MESSAGE {
        return Err(format!(
            "message is {} bytes; the limit is {}",
            message.len(),
            super::protocol::MAX_MESSAGE
        ));
    }
    let operation = Operation::Send {
        task: args.task,
        message,
        caller: current_task(),
        await_result: args.await_result,
        timeout_ms: args.timeout.map(duration_ms),
    };
    let payload = checked(request(&descriptor, operation).await?)?;
    render_result(&payload, args.json)
}

pub(crate) async fn wait(args: WaitArgs) -> Result<ExitCode, String> {
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = live_descriptor(&registry, &args.task).await?;
    let operation = Operation::Wait {
        task: args.task,
        caller: current_task(),
        timeout_ms: duration_ms(args.timeout.unwrap_or_else(default_wait_arg)),
    };
    let payload = checked(request(&descriptor, operation).await?)?;
    render_result(&payload, args.json)
}

pub(crate) async fn cancel(args: CancelArgs) -> Result<ExitCode, String> {
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = live_descriptor(&registry, &args.task).await?;
    let task = args.task;
    let payload = checked(
        request(
            &descriptor,
            Operation::Cancel {
                task,
                caller: current_task(),
            },
        )
        .await?,
    )?;
    render_result(&payload, args.json)
}

pub(crate) async fn inbox(args: InboxArgs) -> Result<ExitCode, String> {
    let task = args.task.or_else(current_task).ok_or_else(|| {
        "`lan inbox` needs a task id outside a LAN task: use `lan inbox <ID>`".to_string()
    })?;
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = live_descriptor(&registry, &task).await?;
    let payload = checked(request(&descriptor, Operation::Inbox { task }).await?)?;
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
            let body = message["body"].as_str().unwrap_or_default();
            println!("[{state}] {body}");
        }
    }
    print_hint(&payload);
    Ok(ExitCode::from(EXIT_OK))
}

pub(crate) async fn watch(args: WatchArgs) -> Result<ExitCode, String> {
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = live_descriptor(&registry, &args.task).await?;
    let timeout = args
        .timeout
        .map(DurationArg::duration)
        .unwrap_or(DEFAULT_WAIT);
    let deadline = Instant::now() + timeout;
    let mut since = 0_u64;
    let mut streamed_answer = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(format!(
                "watch for {} timed out; the task is still running",
                args.task
            ));
        }
        let poll = WATCH_POLL.min(deadline.saturating_duration_since(now));
        let payload = checked(
            request(
                &descriptor,
                Operation::Watch {
                    task: args.task.clone(),
                    caller: current_task(),
                    since,
                    timeout_ms: millis(poll),
                },
            )
            .await?,
        )?;
        if let Some(events) = payload["events"].as_array() {
            for event in events {
                since = since.max(event["seq"].as_u64().unwrap_or(since));
                streamed_answer |= event["event"]["type"] == "assistant_delta";
                render_event(event, args.json)?;
            }
        }
        if payload["terminal"].as_bool().unwrap_or(false) {
            if let Some(result) = payload.get("result").and_then(Value::as_object) {
                let result = Value::Object(result.clone());
                if streamed_answer && !args.json && result["state"] == "succeeded" {
                    print_hint(&result);
                    return Ok(ExitCode::from(result_code(&result)));
                }
                return render_result(&result, args.json);
            }
            return Ok(ExitCode::from(EXIT_FAILED));
        }
    }
}

async fn live_descriptor(registry: &Registry, task: &str) -> Result<Descriptor, String> {
    let descriptor = registry
        .descriptor_for_task(task)
        .map_err(|error| format!("read task handle: {error}"))?
        .ok_or_else(|| format!("task handle `{task}` has no local service descriptor"))?;
    if probe(&descriptor).await {
        return Ok(descriptor);
    }
    let restarted = registry
        .ensure_daemon(Path::new(&descriptor.workspace))
        .await?;
    if restarted.instance != descriptor.instance {
        return Err(format!(
            "task handle `{task}` belongs to a different service instance"
        ));
    }
    Ok(restarted)
}

fn checked(response: Response) -> Result<Value, String> {
    if response.version != super::protocol::VERSION {
        return Err(format!(
            "lan service replied with unsupported protocol version {}",
            response.version
        ));
    }
    match response.kind {
        ResponseKind::Ok => Ok(response.payload),
        ResponseKind::Error => Err(response.payload["error"]
            .as_str()
            .unwrap_or("lan service rejected the request")
            .to_string()),
    }
}

fn render_result(payload: &Value, structured: bool) -> Result<ExitCode, String> {
    if structured {
        println!("{payload}");
        return Ok(ExitCode::from(result_code(payload)));
    }

    match payload["state"].as_str().unwrap_or("unknown") {
        "running" | "accepted" | "cancel_requested" => {
            if let Some(task) = payload["task"].as_str() {
                println!("task {task}: {}", payload["state"].as_str().unwrap());
            }
        }
        "succeeded" => {
            if let Some(result) = payload["result"].as_str()
                && !result.is_empty()
            {
                print!("{result}");
                if !result.ends_with('\n') {
                    println!();
                }
            }
        }
        "failed" => eprintln!(
            "lan: task failed: {}",
            payload["error"].as_str().unwrap_or("unknown failure")
        ),
        "cancelled" => eprintln!("lan: task was cancelled"),
        "orphaned" => eprintln!("lan: task owner exited before the task settled"),
        state => println!("task state: {state}"),
    }
    print_hint(payload);
    io::stdout()
        .flush()
        .map_err(|error| format!("flush task output: {error}"))?;
    Ok(ExitCode::from(result_code(payload)))
}

fn render_event(record: &Value, structured: bool) -> Result<(), String> {
    if structured {
        println!("{record}");
        return Ok(());
    }
    let event = &record["event"];
    match event["type"].as_str().unwrap_or_default() {
        "assistant_delta" => {
            print!("{}", event["text"].as_str().unwrap_or_default());
            io::stdout()
                .flush()
                .map_err(|error| format!("flush task progress: {error}"))?;
        }
        "tool_started" => eprintln!("  · {}", event["tool_name"].as_str().unwrap_or("tool")),
        "notice" | "error" => {
            eprintln!("lan: {}", event["message"].as_str().unwrap_or("task event"))
        }
        "run_finished" => println!(),
        _ => {}
    }
    Ok(())
}

fn result_code(payload: &Value) -> u8 {
    if !payload["stopped_by"].is_null() {
        return EXIT_BOUNDED;
    }
    match payload["state"].as_str() {
        Some("running" | "accepted" | "cancel_requested" | "succeeded") => EXIT_OK,
        _ => EXIT_FAILED,
    }
}

fn print_hint(payload: &Value) {
    if let Some(next) = payload["next"].as_str() {
        println!("next: use `{next}`");
    }
}

fn run_options(args: &RunArgs) -> RunOptions {
    RunOptions {
        provider: args.provider.clone(),
        base_url: args.base_url.clone(),
        model: args.model.clone(),
        no_shell: args.no_shell,
        effort: args.effort.map(effort_name).map(str::to_string),
        approve: approval_name(args.approve).to_string(),
        // A detached/async owner has no human watching it. Give it a finite
        // service bound even when the attended one-shot spelling omitted one;
        // `--deadline` still narrows this explicitly.
        deadline_ms: Some(duration_ms(
            args.deadline
                .unwrap_or_else(|| duration_arg(DEFAULT_TASK_DEADLINE)),
        )),
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
    millis(duration.duration())
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn default_wait_arg() -> DurationArg {
    // Parsed through the same type as a command-line value, so this default
    // cannot drift outside the grammar's range.
    "30m".parse().expect("30m is a valid duration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terminal_codes_do_not_depend_on_rendering() {
        assert_eq!(result_code(&json!({"state": "succeeded"})), EXIT_OK);
        assert_eq!(result_code(&json!({"state": "failed"})), EXIT_FAILED);
        assert_eq!(
            result_code(&json!({"state": "failed", "stopped_by": "deadline"})),
            EXIT_BOUNDED
        );
    }
}
