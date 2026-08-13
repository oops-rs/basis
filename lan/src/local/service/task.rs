//! Task creation and Mentra execution.

use std::{io, path::Path, time::Duration};

use lan_core::{
    AllowAll, Approver, Bound, CancellationToken, DenyAll, Effort, Event, EventSink, ModelSelector,
    RunConfig, RunOutcome, ShellAccess, TurnOptions, provider,
};
use tokio::time;
use uuid::Uuid;

use super::{Shared, notify_changed};
use crate::local::{
    protocol::{MAX_PROMPT, RunOptions as WireRunOptions},
    registry::canonical_workspace,
    store::{self, TaskRecord},
};

use super::lifecycle::{
    WaitLease, begin_wait, deadline_of, finish_cancelled, finish_failed, is_cancel_requested,
    persist, settle_or_take_message,
};

pub(super) struct SpawnRequest {
    pub(super) workspace: String,
    pub(super) prompt: String,
    pub(super) parent: Option<String>,
    pub(super) caller: Option<String>,
    pub(super) detached: bool,
    pub(super) await_result: bool,
    pub(super) options: WireRunOptions,
}

const DEFAULT_TASK_DEADLINE_MS: u64 = 30 * 60 * 1000;

pub(super) async fn spawn_task(
    shared: &Shared,
    request: SpawnRequest,
) -> Result<(String, WaitLease), String> {
    let SpawnRequest {
        workspace,
        prompt,
        parent,
        caller,
        detached,
        await_result,
        options,
    } = request;
    if prompt.trim().is_empty() {
        return Err("prompt is empty".to_string());
    }
    if prompt.len() > MAX_PROMPT {
        return Err(format!(
            "prompt is {} bytes; the limit is {MAX_PROMPT}",
            prompt.len()
        ));
    }
    validate_approval(&options.approve)?;
    let requested_workspace = canonical_workspace(Path::new(&workspace))
        .map_err(|error| format!("resolve workspace {workspace}: {error}"))?;
    if requested_workspace != shared.workspace {
        return Err("the request does not belong to this workspace service".to_string());
    }
    if detached && parent.is_some() {
        return Err("a detached task cannot also name a parent".to_string());
    }
    if !detached && parent.as_deref() != caller.as_deref() {
        return Err("an attached task's parent must be its submitting caller".to_string());
    }

    let task = format!("{}/{}", shared.descriptor.instance, Uuid::new_v4().simple());
    let deadline_after = options.deadline_ms.unwrap_or(DEFAULT_TASK_DEADLINE_MS);
    let requested_deadline = Some(
        store::now_ms()
            .checked_add(deadline_after)
            .ok_or_else(|| "task deadline exceeds the system clock range".to_string())?,
    );
    let deadline_at = {
        let journal = shared.journal.lock().expect("task journal poisoned");
        match parent.as_deref() {
            Some(parent_id) => {
                let owner = journal
                    .get(parent_id)
                    .ok_or_else(|| format!("parent task {parent_id} does not exist"))?;
                if !owner.accepts_work()
                    || owner
                        .deadline_at_ms
                        .is_some_and(|deadline| deadline <= store::now_ms())
                {
                    return Err(format!("parent task {parent_id} is no longer running"));
                }
                earlier_deadline(requested_deadline, owner.deadline_at_ms)
            }
            None => requested_deadline,
        }
    };

    let record = TaskRecord::new(
        task.clone(),
        parent.clone(),
        detached,
        shared.workspace.to_string_lossy().into_owned(),
        String::new(),
        deadline_at,
    );
    {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        if journal.len() >= store::MAX_TASKS {
            return Err(format!(
                "task journal is full (limit {}); archive an old workspace registry",
                store::MAX_TASKS
            ));
        }
        if let Some(parent_id) = parent.as_deref()
            && journal.get(parent_id).is_none_or(|owner| {
                !owner.accepts_work()
                    || owner
                        .deadline_at_ms
                        .is_some_and(|deadline| deadline <= store::now_ms())
            })
        {
            return Err(format!("parent task {parent_id} is no longer running"));
        }
        journal.insert(task.clone(), record);
    }
    // Acquire the dynamic wait edge before exposing or starting the worker.
    // If cycle detection rejects it, remove the just-created durable record so
    // no task can continue after the caller receives an error.
    let lease = if await_result {
        match begin_wait(shared, caller.as_deref(), &task) {
            Ok(lease) => lease,
            Err(error) => {
                shared
                    .journal
                    .lock()
                    .expect("task journal poisoned")
                    .remove(&task);
                let _ = persist(shared).await;
                return Err(error);
            }
        }
    } else {
        WaitLease::detached()
    };

    if let Err(error) = persist(shared).await {
        shared
            .journal
            .lock()
            .expect("task journal poisoned")
            .remove(&task);
        drop(lease);
        return Err(error);
    }

    let cancellation = CancellationToken::default();
    // Cancellation and control registration use the same journal -> controls
    // lock order as `request_cancel_tree`.  A cancel request can therefore not
    // slip between the journal mutation and the worker's token registration.
    let cancel_immediately = {
        let journal = shared.journal.lock().expect("task journal poisoned");
        let mut controls = shared.controls.lock().expect("task controls poisoned");
        let requested = journal
            .get(&task)
            .is_none_or(|record| record.cancel_requested || record.state.is_terminal());
        controls.insert(task.clone(), cancellation.clone());
        requested
    };
    if cancel_immediately {
        cancellation.cancel();
    }
    let worker = shared.clone();
    let task_for_worker = task.clone();
    tokio::spawn(async move {
        run_task(worker, task_for_worker, prompt, options, cancellation).await;
    });
    notify_changed(shared);
    Ok((task, lease))
}

async fn run_task(
    shared: Shared,
    task: String,
    prompt: String,
    options: WireRunOptions,
    cancellation: CancellationToken,
) {
    // The token check above is the fast path; the journal check covers a
    // cancel that won the registration race before the worker was spawned.
    if cancellation.is_cancelled() || is_cancel_requested(&shared, &task) {
        finish_cancelled(&shared, &task).await;
        return;
    }
    let config = match run_config(
        &shared.workspace,
        prompt,
        &options,
        deadline_of(&shared, &task),
    ) {
        Ok(config) => config,
        Err(error) => {
            finish_failed(&shared, &task, error, None).await;
            return;
        }
    };
    let (builder, spec) = config.split();
    let prepared = match builder
        .with_store_dir(
            shared
                .registry
                .history_directory(&shared.descriptor.instance),
        )
        .with_command_environment("LAN_TASK_ID", &task)
        .with_command_environment("LAN_REGISTRY_DIR", shared.registry.root().to_string_lossy())
        .open()
        .await
        .and_then(|workspace| workspace.prepare(spec))
    {
        Ok(run) => run,
        Err(error) => {
            finish_failed(&shared, &task, error.to_string(), None).await;
            return;
        }
    };
    let mut run = prepared;
    {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let Some(record) = journal.get_mut(&task) else {
            return;
        };
        record.agent_id = run.agent_id().to_string();
        record.updated_ms = store::now_ms();
    }
    if persist(&shared).await.is_err() {
        finish_failed(
            &shared,
            &task,
            "could not persist the prepared agent".to_string(),
            None,
        )
        .await;
        return;
    }

    let mut next_message: Option<(String, String)> = None;
    loop {
        if cancellation.is_cancelled() {
            finish_cancelled(&shared, &task).await;
            return;
        }
        let mut turn = TurnOptions::default().with_cancel(cancellation.clone());
        if let Some(remaining) = remaining_deadline(deadline_of(&shared, &task)) {
            if remaining.is_zero() {
                finish_failed(
                    &shared,
                    &task,
                    "task deadline elapsed before the next turn".to_string(),
                    Some("deadline".to_string()),
                )
                .await;
                return;
            }
            turn = turn.with_deadline(remaining);
        }
        let sink = TaskSink {
            shared: shared.clone(),
            task: task.clone(),
        };
        let approver = match approver(&options.approve) {
            Ok(approver) => approver,
            Err(error) => {
                finish_failed(&shared, &task, error, None).await;
                return;
            }
        };
        let message = next_message.take();
        let completed_message = message.as_ref().map(|(id, _)| id.clone());
        let execution = async {
            match message {
                Some((_, message)) => run.send_with_options(message, sink, approver, turn).await,
                None => {
                    run.execute_with_approver_and_options(sink, approver, turn)
                        .await
                }
            }
        };
        let report = match remaining_deadline(deadline_of(&shared, &task)) {
            Some(remaining) => match time::timeout(remaining, execution).await {
                Ok(report) => report,
                Err(_) => {
                    cancellation.cancel();
                    finish_failed(
                        &shared,
                        &task,
                        "task deadline elapsed during the turn".to_string(),
                        Some("deadline".to_string()),
                    )
                    .await;
                    return;
                }
            },
            None => execution.await,
        };
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                finish_failed(&shared, &task, error.to_string(), None).await;
                return;
            }
        };
        let stopped_by = report.stopped_by.map(bound_name);
        match report.outcome {
            RunOutcome::Error { message } => {
                if cancellation.is_cancelled() || is_cancel_requested(&shared, &task) {
                    finish_cancelled(&shared, &task).await;
                } else {
                    finish_failed(&shared, &task, message, stopped_by).await;
                }
                return;
            }
            RunOutcome::Ok => {
                let result = report.final_message.unwrap_or_default();
                match settle_or_take_message(
                    &shared,
                    &task,
                    completed_message.as_deref(),
                    result,
                    stopped_by,
                )
                .await
                {
                    Ok(Some(message)) => next_message = Some(message),
                    Ok(None) => return,
                    Err(error) => {
                        finish_failed(&shared, &task, error, None).await;
                        return;
                    }
                }
            }
        }
    }
}

fn run_config(
    workspace: &Path,
    prompt: String,
    options: &WireRunOptions,
    deadline_at: Option<u64>,
) -> Result<RunConfig, String> {
    let mut config =
        RunConfig::new(workspace, prompt).with_shell(ShellAccess::from_flag(!options.no_shell));
    if let Some(name) = &options.provider {
        config = config.with_provider(provider::parse(name).map_err(|error| error.to_string())?);
    }
    if let Some(base_url) = &options.base_url {
        config = config.with_base_url(base_url);
    }
    if let Some(model) = &options.model {
        config = config.with_model(ModelSelector::Id(model.clone()));
    }
    if let Some(effort) = options.effort.as_deref() {
        config = config.with_effort(parse_effort(effort)?);
    }
    if let Some(remaining) = remaining_deadline(deadline_at) {
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

fn validate_approval(value: &str) -> Result<(), String> {
    match value {
        "always" | "never" => Ok(()),
        "prompt" => Err(
            "`--approve prompt` needs an interactive transport; use `always` or `never` for asynchronous work"
                .to_string(),
        ),
        value => Err(format!("unsupported approval mode `{value}`")),
    }
}

fn approver(value: &str) -> Result<Box<dyn Approver>, String> {
    validate_approval(value)?;
    match value {
        "always" => Ok(Box::new(AllowAll)),
        "never" => Ok(Box::new(DenyAll)),
        _ => unreachable!("validated above"),
    }
}

fn earlier_deadline(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn remaining_deadline(deadline_at: Option<u64>) -> Option<Duration> {
    deadline_at.map(|deadline| Duration::from_millis(deadline.saturating_sub(store::now_ms())))
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

struct TaskSink {
    shared: Shared,
    task: String,
}

impl EventSink for TaskSink {
    fn emit(&mut self, event: Event) -> io::Result<()> {
        let value = serde_json::to_value(event)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut journal = self.shared.journal.lock().expect("task journal poisoned");
        if let Some(record) = journal.get_mut(&self.task)
            && !record.state.is_terminal()
        {
            record.record_event(value);
            notify_changed(&self.shared);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_deadlines_can_only_narrow_the_parent() {
        assert_eq!(earlier_deadline(Some(20), Some(10)), Some(10));
        assert_eq!(earlier_deadline(None, Some(10)), Some(10));
        assert_eq!(earlier_deadline(Some(20), None), Some(20));
        assert_eq!(earlier_deadline(None, None), None);
    }
}
