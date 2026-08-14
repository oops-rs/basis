//! Durable state transitions: settling work, cancelling trees, finalizing.

use std::collections::{HashSet, VecDeque};

use lan_core::CancellationToken;
use serde_json::{Value, json};

use super::{payload::terminal_payload, policy::validate_cancel_target};
use crate::local::{
    service::{Shared, notify_changed},
    store::{self, DurableState, Journal, MessageReply, PendingTerminal},
};

enum SettleAction {
    Next((String, String)),
    Complete(PendingTerminal),
}

#[derive(Default)]
pub(super) struct TransitionEffects {
    pub(super) cancel: Vec<String>,
    pub(super) finalized: Vec<String>,
}

pub(in crate::local::service) async fn settle_or_take_message(
    shared: &Shared,
    task: &str,
    completed_message: Option<&str>,
    result: String,
    stopped_by: Option<String>,
) -> Result<Option<(String, String)>, String> {
    let (next, effects) = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let action = {
            let record = journal
                .get_mut(task)
                .ok_or_else(|| format!("task {task} does not exist"))?;
            if record.state.is_terminal() || record.pending_terminal.is_some() {
                return Err(format!("task {task} no longer has active work"));
            }
            let completed_reply = completed_message.map(|_| {
                let (reply, result_truncated) =
                    store::bounded_text(result.clone(), store::MAX_RESULT_BYTES);
                MessageReply {
                    result: reply,
                    result_truncated,
                    stopped_by: stopped_by.clone(),
                }
            });
            if let Some(message) = completed_message {
                record.finish_message(message, completed_reply);
            }
            if record.cancel_requested {
                record.finish_unanswered_messages();
                SettleAction::Complete(PendingTerminal::Cancelled)
            } else if let Some(message) = record.start_next_message() {
                SettleAction::Next(message)
            } else {
                let (result, truncated) = store::bounded_text(result, store::MAX_RESULT_BYTES);
                record.result_truncated = truncated;
                record.stopped_by = stopped_by;
                SettleAction::Complete(PendingTerminal::Succeeded { result })
            }
        };
        match action {
            SettleAction::Next(message) => (Some(message), TransitionEffects::default()),
            SettleAction::Complete(completion) => {
                (None, apply_completion(&mut journal, task, completion)?)
            }
        }
    };
    let completed_task = next.is_none().then_some(task);
    let tokens = transition_controls(shared, completed_task, &effects);
    // The in-memory transition has already detached its controls.  Always
    // release those controls and wake waiters, even when the durable snapshot
    // cannot be written; otherwise a failed write leaves live workers and
    // waiters observing a state transition that never completes.
    let persist_result = persist(shared).await;
    for token in tokens {
        token.cancel();
    }
    notify_changed(shared);
    persist_result?;
    Ok(next)
}

pub(in crate::local::service) async fn cancel_task(
    shared: &Shared,
    caller: Option<&str>,
    task: &str,
) -> Result<Value, String> {
    validate_cancel_target(shared, caller, task)?;
    if let Some(payload) = terminal_payload(shared, task)? {
        return Ok(payload);
    }
    let cancelled = request_cancel_tree(shared, task, true)?;
    // Cancellation is an in-memory control-plane transition first.  Do not
    // strand the collected cancellation tokens or waiters if its journal
    // write fails; report the persistence error only after cleanup.
    let persist_result = persist(shared).await;
    for token in cancelled {
        token.cancel();
    }
    notify_changed(shared);
    persist_result?;
    Ok(json!({
        "task": task,
        "state": "cancel_requested",
        "next": format!("lan wait {task}"),
    }))
}

pub(super) fn request_cancel_tree(
    shared: &Shared,
    task: &str,
    include_root: bool,
) -> Result<Vec<CancellationToken>, String> {
    let effects = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let cancel = request_cancel_tree_locked(&mut journal, task, include_root)?;
        let mut effects = TransitionEffects {
            cancel,
            finalized: Vec::new(),
        };
        for candidate in effects.cancel.clone().into_iter().rev() {
            finalize_ready_chain(&mut journal, &candidate, &mut effects.finalized);
        }
        effects
    };
    Ok(transition_controls(shared, None, &effects))
}

pub(super) fn request_cancel_tree_locked(
    journal: &mut Journal,
    task: &str,
    include_root: bool,
) -> Result<Vec<String>, String> {
    if !journal.contains_key(task) {
        return Err(format!("task {task} does not exist"));
    }
    let mut queue = VecDeque::from([task.to_string()]);
    let mut visited = HashSet::new();
    let mut affected = Vec::new();
    while let Some(parent) = queue.pop_front() {
        if !visited.insert(parent.clone()) {
            continue;
        }
        let children: Vec<String> = journal
            .values()
            .filter(|record| !record.detached && record.parent.as_deref() == Some(&parent))
            .map(|record| record.id.clone())
            .collect();
        queue.extend(children);
        if parent == task && !include_root {
            continue;
        }
        if let Some(record) = journal.get_mut(&parent)
            && !record.state.is_terminal()
        {
            record.cancel_requested = true;
            if record.pending_terminal.is_some() {
                record.pending_terminal = Some(PendingTerminal::Cancelled);
                record.result_truncated = false;
                record.stopped_by = None;
            }
            record.updated_ms = store::now_ms();
            affected.push(parent);
        }
    }
    Ok(affected)
}

pub(super) fn apply_completion(
    journal: &mut Journal,
    task: &str,
    completion: PendingTerminal,
) -> Result<TransitionEffects, String> {
    let cancel_children = !matches!(completion, PendingTerminal::Succeeded { .. });
    {
        let record = journal
            .get_mut(task)
            .ok_or_else(|| format!("task {task} does not exist"))?;
        if record.state.is_terminal() || record.pending_terminal.is_some() {
            return Err(format!("task {task} no longer has active work"));
        }
        record.pending_terminal = Some(completion);
        record.updated_ms = store::now_ms();
    }

    let cancel = if cancel_children {
        request_cancel_tree_locked(journal, task, false)?
    } else {
        Vec::new()
    };
    let mut effects = TransitionEffects {
        cancel,
        finalized: Vec::new(),
    };
    for candidate in effects.cancel.clone().into_iter().rev() {
        finalize_ready_chain(journal, &candidate, &mut effects.finalized);
    }
    finalize_ready_chain(journal, task, &mut effects.finalized);
    Ok(effects)
}

fn finalize_ready_chain(journal: &mut Journal, task: &str, finalized: &mut Vec<String>) {
    let mut current = Some(task.to_string());
    while let Some(id) = current {
        let has_running_child = journal.values().any(|record| {
            !record.detached && record.parent.as_deref() == Some(&id) && !record.state.is_terminal()
        });
        let ready = journal.get(&id).is_some_and(|record| {
            !record.state.is_terminal() && record.pending_terminal.is_some() && !has_running_child
        });
        if !ready {
            break;
        }

        let record = journal
            .get_mut(&id)
            .expect("the readiness check found this task");
        let completion = record
            .pending_terminal
            .take()
            .expect("the readiness check found a completion");
        record.state = completion.into();
        record.updated_ms = store::now_ms();
        let parent = if record.detached {
            None
        } else {
            record.parent.clone()
        };
        finalized.push(id);
        current = parent;
    }
}

pub(super) fn transition_controls(
    shared: &Shared,
    completed_task: Option<&str>,
    effects: &TransitionEffects,
) -> Vec<CancellationToken> {
    let mut controls = shared.controls.lock().expect("task controls poisoned");
    let tokens = effects
        .cancel
        .iter()
        .filter_map(|id| controls.get(id).cloned())
        .collect();
    if let Some(task) = completed_task {
        controls.remove(task);
    }
    for task in &effects.finalized {
        controls.remove(task);
    }
    drop(controls);

    let mut graph = shared.waits.lock().expect("wait graph poisoned");
    for task in &effects.finalized {
        graph.edges.remove(task);
        for targets in graph.edges.values_mut() {
            targets.remove(task);
        }
    }
    graph.edges.retain(|_, targets| !targets.is_empty());
    tokens
}

pub(in crate::local::service) async fn finish_failed(
    shared: &Shared,
    task: &str,
    message: String,
    stopped_by: Option<String>,
) {
    let effects = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let Some(record) = journal.get_mut(task) else {
            return;
        };
        if record.state.is_terminal() || record.pending_terminal.is_some() {
            return;
        }
        record.finish_unanswered_messages();
        let completion = if record.cancel_requested {
            record.result_truncated = false;
            record.stopped_by = None;
            PendingTerminal::Cancelled
        } else {
            let (error, _) = store::bounded_text(message, store::MAX_RESULT_BYTES);
            record.stopped_by = stopped_by;
            PendingTerminal::Failed { error }
        };
        apply_completion(&mut journal, task, completion).ok()
    };
    if let Some(effects) = effects {
        let tokens = transition_controls(shared, Some(task), &effects);
        let _ = persist(shared).await;
        for token in tokens {
            token.cancel();
        }
        notify_changed(shared);
    }
}

pub(in crate::local::service) async fn finish_cancelled(shared: &Shared, task: &str) {
    let effects = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let Some(record) = journal.get_mut(task) else {
            return;
        };
        if record.state.is_terminal() || record.pending_terminal.is_some() {
            return;
        }
        record.cancel_requested = true;
        record.finish_unanswered_messages();
        record.result_truncated = false;
        record.stopped_by = None;
        apply_completion(&mut journal, task, PendingTerminal::Cancelled).ok()
    };
    if let Some(effects) = effects {
        let tokens = transition_controls(shared, Some(task), &effects);
        let _ = persist(shared).await;
        for token in tokens {
            token.cancel();
        }
        notify_changed(shared);
    }
}

pub(in crate::local::service) async fn orphan_running(shared: &Shared) {
    let tokens = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        for record in journal.values_mut() {
            if !record.state.is_terminal() {
                record.finish_unanswered_messages();
                record.state = DurableState::Orphaned;
                record.pending_terminal = None;
                record.updated_ms = store::now_ms();
            }
        }
        shared
            .controls
            .lock()
            .expect("task controls poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>()
    };
    // No request handler can remain a valid waiter once the daemon has
    // declared every live task orphaned. Clear the graph before waking those
    // handlers so a short-lived test/runtime restart cannot leave stale edges
    // blocking a later request in the same process.
    shared
        .waits
        .lock()
        .expect("wait graph poisoned")
        .edges
        .clear();
    for token in tokens {
        token.cancel();
    }
    let _ = persist(shared).await;
    notify_changed(shared);
}

pub(in crate::local::service) async fn persist(shared: &Shared) -> Result<(), String> {
    let _gate = shared.persist_gate.lock().await;
    let snapshot = shared
        .journal
        .lock()
        .expect("task journal poisoned")
        .clone();
    let registry = shared.registry.clone();
    let instance = shared.descriptor.instance.clone();
    tokio::task::spawn_blocking(move || store::save(&registry, &instance, &snapshot))
        .await
        .map_err(|error| format!("task journal writer failed: {error}"))?
        .map_err(|error| format!("persist task journal: {error}"))
}
