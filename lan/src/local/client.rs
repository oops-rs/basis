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
    cli::{
        ApproveMode, AskArgs, CancelArgs, EffortArg, InboxArgs, RunArgs, SendArgs, WaitArgs,
        WatchArgs,
    },
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

/// An error returned by a local lifecycle command.
///
/// The daemon's wire contract has historically carried a human-readable
/// `error` string. Keeping the error as a value at the client boundary lets us
/// add machine-readable timeout/retry information without changing that
/// string (or making `main` guess from stderr). In particular, a wait timeout
/// is a bounded observation, not a failed task, and therefore exits with 3.
#[derive(Debug)]
pub(crate) struct ClientError {
    message: String,
    payload: Option<Value>,
    code: u8,
}

impl ClientError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            payload: None,
            code: EXIT_FAILED,
        }
    }

    fn from_response(mut payload: Value) -> Self {
        let message = payload["error"]
            .as_str()
            .unwrap_or("lan service rejected the request")
            .to_string();

        // Older daemons only sent prose. Recognize their stable timeout
        // sentences here so a client can still preserve the durable handles
        // and return the bounded exit code. A newer daemon may send `code` and
        // `next` directly; those fields are accepted without interpretation.
        let timeout = payload["code"] == "timeout" || parse_timeout(&message).is_some();
        if timeout {
            let parsed = parse_timeout(&message);
            if let Some(parsed) = parsed
                && let Some(object) = payload.as_object_mut()
            {
                for (key, value) in parsed {
                    object.entry(key).or_insert(value);
                }
            }
            payload["code"] = Value::String("timeout".to_string());
            payload["timed_out"] = Value::Bool(true);
        }

        Self {
            message,
            payload: Some(payload),
            code: if timeout { EXIT_BOUNDED } else { EXIT_FAILED },
        }
    }

    fn timeout(message: impl Into<String>, payload: Value) -> Self {
        Self {
            message: message.into(),
            payload: Some(payload),
            code: EXIT_BOUNDED,
        }
    }

    /// Render the error using the command's requested output mode.
    pub(crate) fn render(self, structured: bool, command: &str) -> ExitCode {
        if structured {
            println!("{}", self.json_payload(command));
        } else {
            eprintln!("lan: {}", self.message);
            if let Some(next) = self.next_action() {
                eprintln!("next: use `{next}`");
            } else {
                eprintln!("next: retry with `{command}` or inspect `lan --help`");
            }
        }
        ExitCode::from(self.code)
    }

    fn next_action(&self) -> Option<String> {
        self.payload
            .as_ref()
            .and_then(|payload| payload["next"].as_str())
            .map(str::to_string)
    }

    fn json_payload(&self, command: &str) -> Value {
        let mut payload = self
            .payload
            .clone()
            .unwrap_or_else(|| serde_json::json!({"error": self.message}));
        let object = payload
            .as_object_mut()
            .expect("client error payload must be a JSON object");
        object
            .entry("error".to_string())
            .or_insert_with(|| Value::String(self.message.clone()));
        object
            .entry("code".to_string())
            .or_insert_with(|| Value::String("failed".to_string()));
        object
            .entry("next".to_string())
            .or_insert_with(|| Value::String(command.to_string()));
        payload
    }
}

impl From<String> for ClientError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ClientError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Parse timeout messages emitted by the current and previous local daemons.
/// The parser intentionally only recognizes the fixed prefixes owned by this
/// client; arbitrary service failures remain ordinary exit-1 errors.
fn parse_timeout(message: &str) -> Option<serde_json::Map<String, Value>> {
    let mut payload = serde_json::Map::new();
    if let Some(rest) = message.strip_prefix("wait for ")
        && let Some((task, _)) = rest.split_once(" timed out")
        && !task.is_empty()
    {
        payload.insert("task".to_string(), Value::String(task.to_string()));
        payload.insert("state".to_string(), Value::String("running".to_string()));
        payload.insert(
            "next".to_string(),
            Value::String(format!("lan wait {task}")),
        );
        return Some(payload);
    }

    if let Some(rest) = message.strip_prefix("message ")
        && let Some((message_id, rest)) = rest.split_once(" on ")
        && let Some((task, _)) = rest.split_once(" timed out")
        && !message_id.is_empty()
        && !task.is_empty()
    {
        payload.insert("task".to_string(), Value::String(task.to_string()));
        payload.insert("message".to_string(), Value::String(message_id.to_string()));
        payload.insert("state".to_string(), Value::String("waiting".to_string()));
        payload.insert(
            "next".to_string(),
            Value::String(format!("lan wait {task} --message {message_id}")),
        );
        return Some(payload);
    }

    if let Some(rest) = message.strip_prefix("watch for ")
        && let Some((task, _)) = rest.split_once(" timed out")
        && !task.is_empty()
    {
        payload.insert("task".to_string(), Value::String(task.to_string()));
        payload.insert("state".to_string(), Value::String("running".to_string()));
        payload.insert(
            "next".to_string(),
            Value::String(format!("lan watch {task}")),
        );
        return Some(payload);
    }

    None
}

pub(crate) async fn spawn(args: RunArgs) -> Result<ExitCode, ClientError> {
    let workspace = workspace_or_current(args.workspace.clone())?;
    let prompt = prompt_from(args.prompt.clone())?;
    if prompt.len() > super::protocol::MAX_PROMPT {
        return Err(format!(
            "prompt is {} bytes; the limit is {}",
            prompt.len(),
            super::protocol::MAX_PROMPT
        )
        .into());
    }
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = registry.ensure_daemon(&workspace).await?;
    let caller = current_task();
    if let Some(caller) = caller.as_deref()
        && !args.detached
        && !caller.starts_with(&format!("{}/", descriptor.instance))
    {
        return Err(format!(
            "current task {caller} belongs to another workspace service; use `lan spawn --detached ...` to start work here"
        )
        .into());
    }
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
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = live_descriptor(&registry, &task).await?;
    let message = prompt_from(raw_message)?;
    if message.len() > super::protocol::MAX_MESSAGE {
        return Err(format!(
            "message is {} bytes; the limit is {}",
            message.len(),
            super::protocol::MAX_MESSAGE
        )
        .into());
    }
    let operation = Operation::Send {
        task,
        message,
        caller: current_task(),
        await_result,
        timeout_ms: timeout.map(duration_ms),
    };
    let payload = checked(request(&descriptor, operation).await?)?;
    render_result(&payload, json)
}

pub(crate) async fn wait(args: WaitArgs) -> Result<ExitCode, ClientError> {
    let registry = Registry::discover().map_err(|error| format!("open task registry: {error}"))?;
    let descriptor = live_descriptor(&registry, &args.task).await?;
    let operation = Operation::Wait {
        task: args.task,
        caller: current_task(),
        message: args.message,
        timeout_ms: duration_ms(args.timeout.unwrap_or_else(default_wait_arg)),
    };
    let payload = checked(request(&descriptor, operation).await?)?;
    render_result(&payload, args.json)
}

pub(crate) async fn cancel(args: CancelArgs) -> Result<ExitCode, ClientError> {
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

pub(crate) async fn inbox(args: InboxArgs) -> Result<ExitCode, ClientError> {
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

pub(crate) async fn watch(args: WatchArgs) -> Result<ExitCode, ClientError> {
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
            let message = format!(
                "watch for {} timed out; the task is still running",
                args.task
            );
            let payload = serde_json::json!({
                "error": message,
                "code": "timeout",
                "timed_out": true,
                "task": args.task,
                "state": "running",
                "next": format!("lan watch {}", args.task),
            });
            let error = payload["error"].as_str().unwrap_or_default().to_string();
            return Err(ClientError::timeout(error, payload));
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

async fn live_descriptor(registry: &Registry, task: &str) -> Result<Descriptor, ClientError> {
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
        return Err(format!("task handle `{task}` belongs to a different service instance").into());
    }
    Ok(restarted)
}

fn checked(response: Response) -> Result<Value, ClientError> {
    if response.version != super::protocol::VERSION {
        return Err(ClientError::new(format!(
            "lan service replied with unsupported protocol version {}",
            response.version
        )));
    }
    match response.kind {
        ResponseKind::Ok => Ok(response.payload),
        ResponseKind::Error => Err(ClientError::from_response(response.payload)),
    }
}

fn render_result(payload: &Value, structured: bool) -> Result<ExitCode, ClientError> {
    if structured {
        println!("{payload}");
        return Ok(ExitCode::from(result_code(payload)));
    }

    match payload["state"].as_str().unwrap_or("unknown") {
        "running" | "accepted" | "cancel_requested" => {
            if let Some(task) = payload["task"].as_str() {
                let state = payload["state"].as_str().unwrap_or("unknown");
                if state == "accepted"
                    && let Some(message) = payload["message"].as_str()
                {
                    println!("task {task}: accepted message {message}");
                } else {
                    println!("task {task}: {state}");
                }
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
        .map_err(|error| ClientError::new(format!("flush task output: {error}")))?;
    Ok(ExitCode::from(result_code(payload)))
}

fn render_event(record: &Value, structured: bool) -> Result<(), ClientError> {
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
                .map_err(|error| ClientError::new(format!("flush task progress: {error}")))?;
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

    #[test]
    fn message_timeout_keeps_the_durable_retry_handle() {
        let error = ClientError::from_response(json!({
            "error": "message msg-7 on root/task timed out after 1s; retry with `lan wait root/task --message msg-7` or inspect `lan inbox root/task`"
        }));

        assert_eq!(error.code, EXIT_BOUNDED);
        let payload = error.json_payload("lan ask <ID> <MESSAGE>");
        assert_eq!(payload["code"], "timeout");
        assert_eq!(payload["timed_out"], true);
        assert_eq!(payload["task"], "root/task");
        assert_eq!(payload["message"], "msg-7");
        assert_eq!(payload["state"], "waiting");
        assert_eq!(payload["next"], "lan wait root/task --message msg-7");
    }

    #[test]
    fn task_timeout_is_bounded_without_fabricating_a_message_id() {
        let error = ClientError::from_response(json!({
            "error": "wait for root/task timed out after 30s; the task is still running"
        }));

        assert_eq!(error.code, EXIT_BOUNDED);
        let payload = error.json_payload("lan wait <ID>");
        assert_eq!(payload["task"], "root/task");
        assert_eq!(payload["state"], "running");
        assert!(payload.get("message").is_none());
        assert_eq!(payload["next"], "lan wait root/task");
    }

    #[test]
    fn ordinary_service_errors_keep_failed_exit_and_structured_details() {
        let error = ClientError::from_response(json!({
            "error": "task root/task does not exist"
        }));

        assert_eq!(error.code, EXIT_FAILED);
        let payload = error.json_payload("lan wait <ID>");
        assert_eq!(payload["error"], "task root/task does not exist");
        assert_eq!(payload["code"], "failed");
        assert_eq!(payload["next"], "lan wait <ID>");
    }
}
