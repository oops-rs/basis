//! Durable task state transitions and structured-concurrency controls.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use lan_core::CancellationToken;
use serde_json::{Value, json};
use tokio::time::{self, Instant};

use super::{Shared, notify_changed};
use crate::local::store::{self, DurableState, Journal, MessageReply, PendingTerminal};

const DEFAULT_WAIT: Duration = Duration::from_secs(30 * 60);
const MAX_WAIT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

enum SettleAction {
    Next((String, String)),
    Complete(PendingTerminal),
}

#[derive(Default)]
struct TransitionEffects {
    cancel: Vec<String>,
    finalized: Vec<String>,
}

/// In-memory edges represent waits held by live request handlers. They are
/// deliberately not journaled: a process restart drops every lease, and a
/// persisted task can never inherit a wait edge whose owner no longer exists.
#[derive(Debug, Default)]
pub(super) struct WaitGraph {
    edges: HashMap<String, HashMap<String, usize>>,
}

/// A counted edge in [`WaitGraph`]. Dropping it releases exactly one edge,
/// including when a request returns an error, times out, or is cancelled.
pub(super) struct WaitLease {
    graph: Option<Arc<Mutex<WaitGraph>>>,
    caller: Option<String>,
    target: String,
}

impl WaitLease {
    pub(super) fn detached() -> Self {
        Self {
            graph: None,
            caller: None,
            target: String::new(),
        }
    }
}

impl Drop for WaitLease {
    fn drop(&mut self) {
        let (Some(graph), Some(caller)) = (&self.graph, &self.caller) else {
            return;
        };
        let Ok(mut graph) = graph.lock() else {
            return;
        };
        let Some(targets) = graph.edges.get_mut(caller) else {
            return;
        };
        let Some(count) = targets.get_mut(&self.target) else {
            return;
        };
        if *count > 1 {
            *count -= 1;
        } else {
            targets.remove(&self.target);
            if targets.is_empty() {
                graph.edges.remove(caller);
            }
        }
    }
}

impl WaitGraph {
    fn reaches(&self, start: &str, goal: &str) -> bool {
        let mut pending = vec![start];
        let mut visited = HashSet::new();
        while let Some(current) = pending.pop() {
            if current == goal {
                return true;
            }
            if !visited.insert(current) {
                continue;
            }
            if let Some(next) = self.edges.get(current) {
                pending.extend(next.keys().map(String::as_str));
            }
        }
        false
    }

    fn try_acquire(&mut self, caller: String, target: String) -> Result<(), String> {
        if self.reaches(&target, &caller) {
            return Err(format!(
                "wait edge {caller} -> {target} would create a cycle"
            ));
        }
        let count = self
            .edges
            .entry(caller)
            .or_default()
            .entry(target)
            .or_default();
        *count = count
            .checked_add(1)
            .ok_or_else(|| "wait edge count exhausted".to_string())?;
        Ok(())
    }
}

/// Validate the static ownership policy and acquire one dynamic wait edge.
/// The journal lock is always taken before the graph lock; no lock is held by
/// the returned lease across an await.
pub(super) fn begin_wait(
    shared: &Shared,
    caller: Option<&str>,
    target: &str,
) -> Result<WaitLease, String> {
    validate_wait_edge(shared, caller, target)?;
    let Some(caller) = caller else {
        return Ok(WaitLease::detached());
    };
    let mut graph = shared.waits.lock().expect("wait graph poisoned");
    graph.try_acquire(caller.to_string(), target.to_string())?;
    Ok(WaitLease {
        graph: Some(shared.waits.clone()),
        caller: Some(caller.to_string()),
        target: target.to_string(),
    })
}

pub(super) async fn settle_or_take_message(
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
    persist(shared).await?;
    for token in tokens {
        token.cancel();
    }
    notify_changed(shared);
    Ok(next)
}

pub(super) async fn enqueue_message(
    shared: &Shared,
    task: &str,
    message: String,
) -> Result<String, String> {
    if message.trim().is_empty() {
        return Err("message is empty".to_string());
    }
    let id = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let record = journal
            .get_mut(task)
            .ok_or_else(|| format!("task {task} does not exist"))?;
        if !record.accepts_work() {
            return Err(format!("task {task} no longer accepts messages"));
        }
        record.add_message(message)?
    };
    if let Err(error) = persist(shared).await {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        if let Some(record) = journal.get_mut(task)
            && let Some(index) = record
                .messages
                .iter()
                .position(|entry| entry.id == id && entry.state == store::MessageState::Pending)
        {
            record.messages.remove(index);
            record.updated_ms = store::now_ms();
            return Err(error);
        }
        return Err(format!(
            "persist task journal: {error}; message {id} was accepted in memory, inspect `lan inbox {task}`"
        ));
    }
    notify_changed(shared);
    Ok(id)
}

pub(super) async fn await_task(
    shared: &Shared,
    task: &str,
    timeout: Duration,
    _lease: WaitLease,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    let mut updates = shared.changed.subscribe();
    loop {
        if let Some(payload) = terminal_payload(shared, task)? {
            return Ok(payload);
        }
        if time::timeout_at(deadline, updates.changed()).await.is_err() {
            return Err(format!(
                "wait for {task} timed out after {}; the task is still running",
                human_duration(timeout)
            ));
        }
    }
}

pub(super) async fn await_message(
    shared: &Shared,
    task: &str,
    message: &str,
    timeout: Duration,
    _lease: WaitLease,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    let mut updates = shared.changed.subscribe();
    loop {
        if let Some(payload) = message_payload_for_dispatch(shared, task, message)? {
            return Ok(payload);
        }
        if time::timeout_at(deadline, updates.changed()).await.is_err() {
            return Err(format!(
                "message {message} on {task} timed out after {}; retry with `lan wait {task} --message {message}` or inspect `lan inbox {task}`",
                human_duration(timeout)
            ));
        }
    }
}

pub(super) async fn watch_task(
    shared: &Shared,
    caller: Option<&str>,
    task: &str,
    since: u64,
    timeout: Duration,
) -> Result<Value, String> {
    let _lease = begin_wait(shared, caller, task)?;
    let deadline = Instant::now() + timeout;
    let mut updates = shared.changed.subscribe();
    loop {
        let snapshot = watch_snapshot(shared, task, since)?;
        let has_events = snapshot["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty());
        let terminal = snapshot["terminal"].as_bool().unwrap_or(false);
        if has_events || terminal {
            return Ok(snapshot);
        }
        if time::timeout_at(deadline, updates.changed()).await.is_err() {
            return Ok(snapshot);
        }
    }
}

fn watch_snapshot(shared: &Shared, task: &str, since: u64) -> Result<Value, String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    let record = journal
        .get(task)
        .ok_or_else(|| format!("task {task} does not exist"))?;
    let events: Vec<_> = record
        .events
        .iter()
        .filter(|event| event.seq > since)
        .cloned()
        .collect();
    let result = record
        .terminal_result()
        .map(|payload| decorate_terminal(task, payload));
    Ok(json!({
        "task": task,
        "events": events,
        "next_seq": record.next_event.saturating_sub(1),
        "terminal": record.state.is_terminal(),
        "state": record.state,
        "result": result,
    }))
}

pub(super) fn inbox(shared: &Shared, task: &str) -> Result<Value, String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    let record = journal
        .get(task)
        .ok_or_else(|| format!("task {task} does not exist"))?;
    let messages: Vec<Value> = record
        .messages
        .iter()
        .map(|message| {
            let (body, body_truncated) = store::bounded_text(message.body.clone(), 4 * 1024);
            let reply = message.reply.as_ref().map(|reply| {
                let (result, result_truncated) =
                    store::bounded_text(reply.result.clone(), 4 * 1024);
                json!({
                    "result": result,
                    "result_truncated": reply.result_truncated || result_truncated,
                    "stopped_by": reply.stopped_by,
                })
            });
            json!({
                "id": message.id,
                "state": message.state,
                "body": body,
                "body_truncated": body_truncated,
                "reply": reply,
            })
        })
        .collect();
    Ok(json!({
        "task": task,
        "messages": messages,
        "next": format!("lan watch {task}"),
    }))
}

pub(super) async fn cancel_task(
    shared: &Shared,
    caller: Option<&str>,
    task: &str,
) -> Result<Value, String> {
    validate_cancel_target(shared, caller, task)?;
    if let Some(payload) = terminal_payload(shared, task)? {
        return Ok(payload);
    }
    let cancelled = request_cancel_tree(shared, task, true)?;
    persist(shared).await?;
    for token in cancelled {
        token.cancel();
    }
    notify_changed(shared);
    Ok(json!({
        "task": task,
        "state": "cancel_requested",
        "next": format!("lan wait {task}"),
    }))
}

fn validate_cancel_target(
    shared: &Shared,
    caller: Option<&str>,
    target: &str,
) -> Result<(), String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    if !journal.contains_key(target) {
        return Err(format!("task {target} does not exist"));
    }
    let Some(caller) = caller else {
        return Ok(());
    };
    if !journal.contains_key(caller) {
        return Err(format!("caller task {caller} does not exist"));
    }
    if caller == target || is_ancestor(&journal, caller, target) {
        return Ok(());
    }
    if is_ancestor(&journal, target, caller) {
        return Err(format!("task {caller} cannot cancel its ancestor {target}"));
    }
    Err(format!(
        "task {caller} cannot cancel peer {target}; only itself or descendants are allowed"
    ))
}

fn request_cancel_tree(
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

fn request_cancel_tree_locked(
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

fn apply_completion(
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

fn transition_controls(
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
    tokens
}

pub(super) async fn finish_failed(
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

pub(super) async fn finish_cancelled(shared: &Shared, task: &str) {
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

pub(super) async fn orphan_running(shared: &Shared) {
    let tokens = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        for record in journal.values_mut() {
            if !record.state.is_terminal() {
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
    for token in tokens {
        token.cancel();
    }
    let _ = persist(shared).await;
    notify_changed(shared);
}

pub(super) async fn persist(shared: &Shared) -> Result<(), String> {
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

fn terminal_payload(shared: &Shared, task: &str) -> Result<Option<Value>, String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    let record = journal
        .get(task)
        .ok_or_else(|| format!("task {task} does not exist"))?;
    let Some(payload) = record.terminal_result() else {
        return Ok(None);
    };
    Ok(Some(decorate_terminal(task, payload)))
}

pub(super) fn message_payload_for_dispatch(
    shared: &Shared,
    task: &str,
    message_id: &str,
) -> Result<Option<Value>, String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    let record = journal
        .get(task)
        .ok_or_else(|| format!("task {task} does not exist"))?;
    let message = record
        .messages
        .iter()
        .find(|message| message.id == message_id)
        .ok_or_else(|| format!("message {message_id} does not exist on task {task}"))?;
    if let Some(reply) = &message.reply {
        let mut payload = json!({
            "task": task,
            "message": message_id,
            "state": "succeeded",
            "result": reply.result,
            "result_truncated": reply.result_truncated,
            "stopped_by": reply.stopped_by,
            "next": format!("lan inbox {task}"),
        });
        if !reply.result_truncated {
            payload["result_truncated"] = Value::Null;
        }
        return Ok(Some(payload));
    }
    if message.state == store::MessageState::Delivered
        && let Some(mut terminal) = record.terminal_result()
    {
        let object = terminal
            .as_object_mut()
            .expect("terminal payload is an object");
        object.insert("task".to_string(), json!(task));
        object.insert("message".to_string(), json!(message_id));
        object.insert("next".to_string(), json!(format!("lan inbox {task}")));
        return Ok(Some(terminal));
    }
    Ok(None)
}

fn decorate_terminal(task: &str, mut payload: Value) -> Value {
    let object = payload
        .as_object_mut()
        .expect("terminal payload is an object");
    object.insert("task".to_string(), json!(task));
    object.insert(
        "next".to_string(),
        json!(format!("lan watch {task} or lan inbox {task}")),
    );
    payload
}

pub(super) fn accepted_payload(task: &str) -> Value {
    json!({
        "task": task,
        "state": "running",
        "next": format!("lan wait {task}"),
    })
}

/// Return a next action that is legal for the submitting task. Enqueue-only
/// sends intentionally do not acquire a wait lease; when the target is an
/// ancestor, peer, or self, suggest inspecting the target's inbox instead of
/// suggesting an impossible `lan wait` edge.
pub(super) fn send_next_hint(
    shared: &Shared,
    caller: Option<&str>,
    target: &str,
    message: &str,
) -> String {
    if let Some(caller) = caller
        && validate_wait_edge(shared, Some(caller), target).is_err()
    {
        return format!("lan inbox {target}");
    }
    format!("lan wait {target} --message {message}")
}

pub(super) fn deadline_of(shared: &Shared, task: &str) -> Option<u64> {
    shared
        .journal
        .lock()
        .expect("task journal poisoned")
        .get(task)
        .and_then(|record| record.deadline_at_ms)
}

pub(super) fn is_cancel_requested(shared: &Shared, task: &str) -> bool {
    shared
        .journal
        .lock()
        .expect("task journal poisoned")
        .get(task)
        .is_none_or(|record| record.cancel_requested)
}

pub(super) fn validate_wait_edge(
    shared: &Shared,
    caller: Option<&str>,
    target: &str,
) -> Result<(), String> {
    let Some(caller) = caller else {
        return Ok(());
    };
    if caller == target {
        return Err("a task cannot await itself".to_string());
    }
    let journal = shared.journal.lock().expect("task journal poisoned");
    if !journal.contains_key(target) {
        return Err(format!("task {target} does not exist"));
    }
    if !journal.contains_key(caller) {
        return Err(format!("caller task {caller} does not exist"));
    }
    if is_ancestor(&journal, target, caller) {
        return Err(format!(
            "task {caller} cannot await its ancestor {target}; send without --await instead"
        ));
    }
    if is_ancestor(&journal, caller, target) {
        return Ok(());
    }
    if root_of(&journal, caller) == root_of(&journal, target) {
        return Err(format!(
            "task {caller} cannot await peer {target}; only descendants or independent roots are safe"
        ));
    }
    Ok(())
}

fn is_ancestor(journal: &Journal, ancestor: &str, descendant: &str) -> bool {
    let mut current = journal
        .get(descendant)
        .and_then(|record| record.parent.as_deref());
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = journal.get(id).and_then(|record| record.parent.as_deref());
    }
    false
}

fn root_of<'a>(journal: &'a Journal, task: &'a str) -> &'a str {
    let mut current = task;
    while let Some(parent) = journal
        .get(current)
        .and_then(|record| record.parent.as_deref())
    {
        current = parent;
    }
    current
}

pub(super) fn duration_from_ms(value: Option<u64>) -> Duration {
    value
        .map(|milliseconds| Duration::from_millis(milliseconds).min(MAX_WAIT))
        .unwrap_or(DEFAULT_WAIT)
}

fn human_duration(duration: Duration) -> String {
    if duration.as_secs().is_multiple_of(60) {
        format!("{}m", duration.as_secs() / 60)
    } else {
        format!("{}s", duration.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use tempfile::TempDir;
    use tokio::sync::{Mutex as AsyncMutex, watch};

    use super::*;
    use crate::local::{
        protocol::VERSION,
        registry::{Descriptor, Registry, canonical_workspace, workspace_key},
        store::TaskRecord,
    };

    fn record(id: &str, parent: Option<&str>) -> TaskRecord {
        TaskRecord::new(
            id.to_string(),
            parent.map(str::to_string),
            false,
            "/repo".to_string(),
            String::new(),
            None,
        )
    }

    fn test_shared() -> (TempDir, Shared) {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::from_path(dir.path().join("registry")).expect("registry");
        let workspace = canonical_workspace(dir.path()).expect("workspace");
        let (changed, _) = watch::channel(0_u64);
        let descriptor = Descriptor {
            version: VERSION,
            instance: workspace_key(&workspace),
            workspace: workspace.to_string_lossy().into_owned(),
            endpoint: "127.0.0.1:1".to_string(),
            token: "token".to_string(),
            pid: std::process::id(),
        };
        let shared = Shared {
            registry,
            descriptor,
            workspace,
            journal: Arc::new(Mutex::new(Journal::new())),
            controls: Arc::new(Mutex::new(HashMap::new())),
            persist_gate: Arc::new(AsyncMutex::new(())),
            changed,
            waits: Arc::new(Mutex::new(WaitGraph::default())),
        };
        (dir, shared)
    }

    #[test]
    fn wait_edges_allow_descendants_and_independent_roots_only() {
        let (_dir, shared) = test_shared();
        {
            let mut journal = shared.journal.lock().expect("journal");
            journal.insert("root".to_string(), record("root", None));
            journal.insert("child".to_string(), record("child", Some("root")));
            journal.insert("peer".to_string(), record("peer", Some("root")));
            journal.insert("other".to_string(), record("other", None));
        }

        assert!(validate_wait_edge(&shared, Some("root"), "child").is_ok());
        assert!(validate_wait_edge(&shared, Some("root"), "other").is_ok());
        assert!(validate_wait_edge(&shared, Some("child"), "root").is_err());
        assert!(validate_wait_edge(&shared, Some("child"), "peer").is_err());
        assert!(validate_wait_edge(&shared, Some("root"), "root").is_err());
    }

    #[test]
    fn cancellation_stays_inside_the_attached_tree() {
        let (_dir, shared) = test_shared();
        {
            let mut journal = shared.journal.lock().expect("journal");
            journal.insert("root".to_string(), record("root", None));
            journal.insert("child".to_string(), record("child", Some("root")));
            let mut independent = record("independent", None);
            independent.detached = true;
            journal.insert("independent".to_string(), independent);
        }

        request_cancel_tree(&shared, "root", true).expect("cancel tree");
        let journal = shared.journal.lock().expect("journal");
        assert!(journal["root"].cancel_requested);
        assert!(journal["child"].cancel_requested);
        assert!(!journal["independent"].cancel_requested);
    }

    #[test]
    fn terminal_watch_snapshot_carries_a_next_action() {
        let (_dir, shared) = test_shared();
        let mut terminal = record("task", None);
        terminal.state = DurableState::Succeeded {
            result: "done".to_string(),
        };
        shared
            .journal
            .lock()
            .expect("journal")
            .insert("task".to_string(), terminal);

        let snapshot = watch_snapshot(&shared, "task", 0).expect("snapshot");
        assert_eq!(
            snapshot["result"]["next"],
            "lan watch task or lan inbox task"
        );
    }

    #[test]
    fn opposite_independent_wait_edges_are_rejected() {
        let mut graph = WaitGraph::default();
        graph
            .try_acquire("left".to_string(), "right".to_string())
            .expect("first edge");
        let error = graph
            .try_acquire("right".to_string(), "left".to_string())
            .expect_err("opposite edge would deadlock");
        assert!(error.contains("cycle"), "{error}");
    }

    #[test]
    fn duplicate_wait_leases_are_counted_until_each_drops() {
        let graph = Arc::new(Mutex::new(WaitGraph::default()));
        let first = {
            let mut guard = graph.lock().expect("graph");
            guard
                .try_acquire("caller".to_string(), "target".to_string())
                .expect("first edge");
            WaitLease {
                graph: Some(graph.clone()),
                caller: Some("caller".to_string()),
                target: "target".to_string(),
            }
        };
        let second = {
            let mut guard = graph.lock().expect("graph");
            guard
                .try_acquire("caller".to_string(), "target".to_string())
                .expect("duplicate edge");
            WaitLease {
                graph: Some(graph.clone()),
                caller: Some("caller".to_string()),
                target: "target".to_string(),
            }
        };
        drop(first);
        assert_eq!(graph.lock().expect("graph").edges["caller"]["target"], 1);
        drop(second);
        assert!(graph.lock().expect("graph").edges.is_empty());
    }

    #[tokio::test]
    async fn watch_rejects_an_ancestor_target_before_waiting() {
        let (_dir, shared) = test_shared();
        {
            let mut journal = shared.journal.lock().expect("journal");
            journal.insert("root".to_string(), record("root", None));
            journal.insert("child".to_string(), record("child", Some("root")));
        }
        let error = watch_task(&shared, Some("child"), "root", 0, Duration::from_millis(1))
            .await
            .expect_err("watching an ancestor is an unsafe wait edge");
        assert!(error.contains("ancestor"), "{error}");
    }

    #[test]
    fn successful_parent_finalizes_only_after_attached_child() {
        let mut journal = Journal::new();
        journal.insert("root".to_string(), record("root", None));
        journal.insert("child".to_string(), record("child", Some("root")));

        let parent = apply_completion(
            &mut journal,
            "root",
            PendingTerminal::Succeeded {
                result: "parent".to_string(),
            },
        )
        .expect("parent completion");
        assert!(parent.finalized.is_empty());
        assert!(matches!(journal["root"].state, DurableState::Running));
        assert!(!journal["root"].accepts_work());

        let child = apply_completion(
            &mut journal,
            "child",
            PendingTerminal::Succeeded {
                result: "child".to_string(),
            },
        )
        .expect("child completion");
        assert_eq!(child.finalized, ["child", "root"]);
        assert!(matches!(
            journal["child"].state,
            DurableState::Succeeded { ref result } if result == "child"
        ));
        assert!(matches!(
            journal["root"].state,
            DurableState::Succeeded { ref result } if result == "parent"
        ));
    }

    #[test]
    fn failed_parent_cancels_children_and_waits_for_them() {
        let mut journal = Journal::new();
        journal.insert("root".to_string(), record("root", None));
        journal.insert("child".to_string(), record("child", Some("root")));

        let parent = apply_completion(
            &mut journal,
            "root",
            PendingTerminal::Failed {
                error: "boom".to_string(),
            },
        )
        .expect("parent completion");
        assert_eq!(parent.cancel, ["child"]);
        assert!(parent.finalized.is_empty());
        assert!(journal["child"].cancel_requested);
        assert!(matches!(journal["root"].state, DurableState::Running));

        let child = apply_completion(&mut journal, "child", PendingTerminal::Cancelled)
            .expect("child cancellation");
        assert_eq!(child.finalized, ["child", "root"]);
        assert!(matches!(journal["child"].state, DurableState::Cancelled));
        assert!(matches!(
            journal["root"].state,
            DurableState::Failed { ref error } if error == "boom"
        ));
    }

    #[test]
    fn detached_work_does_not_hold_or_inherit_parent_scope() {
        let mut journal = Journal::new();
        journal.insert("root".to_string(), record("root", None));
        let mut detached = record("detached", Some("root"));
        detached.detached = true;
        journal.insert("detached".to_string(), detached);

        let parent = apply_completion(
            &mut journal,
            "root",
            PendingTerminal::Succeeded {
                result: "done".to_string(),
            },
        )
        .expect("parent completion");
        assert_eq!(parent.finalized, ["root"]);
        assert!(matches!(journal["detached"].state, DurableState::Running));

        let cancelled = request_cancel_tree_locked(&mut journal, "root", true)
            .expect("cancel terminal root is harmless");
        assert!(cancelled.is_empty());
        assert!(!journal["detached"].cancel_requested);
    }

    #[tokio::test]
    async fn each_message_keeps_its_own_reply() {
        let (_dir, shared) = test_shared();
        let mut task = record("task", None);
        let first = task
            .add_message("first".to_string())
            .expect("first message");
        task.start_next_message().expect("first in flight");
        shared
            .journal
            .lock()
            .expect("journal")
            .insert("task".to_string(), task);

        settle_or_take_message(
            &shared,
            "task",
            Some(&first),
            "first reply".to_string(),
            None,
        )
        .await
        .expect("settle first reply");

        let payload = message_payload_for_dispatch(&shared, "task", &first)
            .expect("message lookup")
            .expect("reply is durable");
        assert_eq!(payload["message"], first);
        assert_eq!(payload["result"], "first reply");
        assert_eq!(payload["state"], "succeeded");
    }
}
