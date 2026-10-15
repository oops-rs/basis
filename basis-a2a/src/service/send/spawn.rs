//! Starting a `basis-tasks` task: one detached spawn in the workspace.

use std::time::Duration;

use a2a::A2AError;
use basis_tasks::{Continuation, Hint, MAX_TASKS, RunSpec};

use super::Started;
use crate::{
    index::{Key, Record, task_id_of},
    project::{handle_of, internal},
    service::{TasksBackend, hops::Inbound, hops::effective_deadline, still_working},
};

/// Spawns the task that answers `prompt`, continuing `latest`'s
/// conversation when there is one.
pub(super) async fn spawn(
    backend: &TasksBackend,
    prompt: &str,
    latest: Option<&Record>,
    inbound: Inbound,
    accepted_ms: u64,
    task_deadline: Duration,
) -> Result<Started, A2AError> {
    let continuation = match latest {
        Some(latest) => Continuation::Named(handle_of(latest)?.clone()),
        None => Continuation::New,
    };
    // Clamped here, at spawn, because `basis-tasks` counts a task's deadline
    // from the moment it is minted.
    let deadline = effective_deadline(
        backend.run.deadline.unwrap_or(task_deadline),
        inbound,
        accepted_ms,
    )?;
    let mut spec = run_spec(backend, prompt.to_string(), continuation)
        .with_deadline(deadline)
        .with_hop(inbound.hop);
    // The caller's instant as well as the duration it implies, so the task
    // is never given the few milliseconds spawning takes on top.
    if let Some(caller_ms) = inbound.deadline_ms {
        spec = spec.with_not_after_ms(caller_ms);
    }
    let tasks = backend.tasks.clone();
    let handle = tokio::task::spawn_blocking(move || tasks.spawn(spec))
        .await
        .map_err(internal)?
        .map_err(|error| match error.hint() {
            // `basis-tasks` keeps one executor per conversation; a second
            // message into a context whose last task is still running is the
            // caller's to wait out, not a fault of ours.
            Some(Hint::Wait(_)) => still_working(),
            // The operator's to clear, and said so to them in full; the
            // caller is told what happened and not where on disk.
            _ if error.is_task_limit() => {
                eprintln!("basis-a2a: {error}");
                A2AError::unsupported_operation(format!(
                    "this agent holds the most tasks its workspace can ({MAX_TASKS}); its \
                     operator must archive settled ones before it takes more"
                ))
            }
            _ => internal(error),
        })?;

    // The instant the task actually runs under, read back rather than
    // recomputed: `basis-tasks` anchored the relative deadline when it
    // minted the task, a moment after `accepted_ms`, and what this task
    // tells anyone it delegates to must never be later than that.
    let deadline_ms = match backend.tasks.deadline_at_ms(&handle) {
        Ok(Some(deadline_ms)) => deadline_ms,
        _ => after(accepted_ms, deadline),
    };
    Ok(Started {
        id: task_id_of(&handle).to_string(),
        key: Key::Tasks(handle),
        deadline_ms,
    })
}

/// `deadline` from `start_ms`, as an instant.
pub(super) fn after(start_ms: u64, deadline: Duration) -> u64 {
    start_ms.saturating_add(u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX))
}

fn run_spec(backend: &TasksBackend, prompt: String, continuation: Continuation) -> RunSpec {
    let run = &backend.run;
    // A caller can be handed files, so the task may attach some (ADR-0041 §7).
    let mut spec = RunSpec::new(prompt)
        .with_approve(backend.approval.into())
        .detached()
        .answering()
        .continuing(continuation);
    if !backend.shell {
        spec = spec.without_shell();
    }
    if let Some(provider) = &run.provider {
        spec = spec.with_provider(provider.clone());
    }
    if let Some(base_url) = &run.base_url {
        spec = spec.with_base_url(base_url.clone());
    }
    if let Some(model) = &run.model {
        spec = spec.with_model(model.clone());
    }
    if let Some(effort) = run.effort {
        spec = spec.with_effort(effort);
    }
    if let Some(deadline) = run.deadline {
        spec = spec.with_deadline(deadline);
    }
    if let Some(tool_budget) = run.tool_budget {
        spec = spec.with_tool_budget(tool_budget);
    }
    if let Some(token_budget) = run.token_budget {
        spec = spec.with_token_budget(token_budget);
    }
    if let Some(system_prompt) = &run.system_prompt {
        spec = spec.with_system_prompt(system_prompt.clone());
    }
    spec
}
