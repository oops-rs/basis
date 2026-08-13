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
use crate::local::store::{self, DurableState, Journal};

const DEFAULT_WAIT: Duration = Duration::from_secs(30 * 60);

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
    let (next, terminal) = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let record = journal
            .get_mut(task)
            .ok_or_else(|| format!("task {task} does not exist"))?;
        if let Some(message) = completed_message {
            record.finish_message(message);
        }
        if record.cancel_requested {
            record.state = DurableState::Cancelled;
            record.updated_ms = store::now_ms();
            (None, true)
        } else if let Some(message) = record.start_next_message() {
            (Some(message), false)
        } else {
            let (result, truncated) = store::bounded_text(result, store::MAX_RESULT_BYTES);
            record.state = DurableState::Succeeded { result };
            record.result_truncated = truncated;
            record.stopped_by = stopped_by;
            record.updated_ms = store::now_ms();
            (None, true)
        }
    };
    persist(shared).await?;
    notify_changed(shared);
    if terminal {
        terminal_cleanup(shared, task).await;
    }
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
        if record.state.is_terminal() {
            return Err(format!("task {task} is already terminal"));
        }
        record.add_message(message)?
    };
    persist(shared).await?;
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
    Ok(json!({
        "task": task,
        "messages": record.messages,
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
    let mut journal = shared.journal.lock().expect("task journal poisoned");
    if !journal.contains_key(task) {
        return Err(format!("task {task} does not exist"));
    }
    let mut queue = VecDeque::from([task.to_string()]);
    let mut affected = Vec::new();
    while let Some(parent) = queue.pop_front() {
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
            record.updated_ms = store::now_ms();
            affected.push(parent);
        }
    }
    let controls = shared.controls.lock().expect("task controls poisoned");
    Ok(affected
        .iter()
        .filter_map(|id| controls.get(id).cloned())
        .collect())
}

pub(super) async fn finish_failed(
    shared: &Shared,
    task: &str,
    message: String,
    stopped_by: Option<String>,
) {
    let terminal = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let Some(record) = journal.get_mut(task) else {
            return;
        };
        if record.state.is_terminal() {
            false
        } else if record.cancel_requested {
            record.finish_in_flight_messages();
            record.state = DurableState::Cancelled;
            record.updated_ms = store::now_ms();
            true
        } else {
            record.finish_in_flight_messages();
            let (error, _) = store::bounded_text(message, store::MAX_RESULT_BYTES);
            record.state = DurableState::Failed { error };
            record.stopped_by = stopped_by;
            record.updated_ms = store::now_ms();
            true
        }
    };
    if terminal {
        let _ = persist(shared).await;
        terminal_cleanup(shared, task).await;
    }
}

pub(super) async fn finish_cancelled(shared: &Shared, task: &str) {
    let terminal = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let Some(record) = journal.get_mut(task) else {
            return;
        };
        if record.state.is_terminal() {
            false
        } else {
            record.cancel_requested = true;
            record.finish_in_flight_messages();
            record.state = DurableState::Cancelled;
            record.updated_ms = store::now_ms();
            true
        }
    };
    if terminal {
        let _ = persist(shared).await;
        terminal_cleanup(shared, task).await;
    }
}

async fn terminal_cleanup(shared: &Shared, task: &str) {
    shared
        .controls
        .lock()
        .expect("task controls poisoned")
        .remove(task);
    if let Ok(tokens) = request_cancel_tree(shared, task, false) {
        let _ = persist(shared).await;
        for token in tokens {
            token.cancel();
        }
    }
    notify_changed(shared);
}

pub(super) async fn orphan_running(shared: &Shared) {
    let tokens = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        for record in journal.values_mut() {
            if !record.state.is_terminal() {
                record.state = DurableState::Orphaned;
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
/// ancestor, peer, or self, suggest inspecting the caller's inbox instead of
/// suggesting an impossible `lan wait` edge.
pub(super) fn send_next_hint(shared: &Shared, caller: Option<&str>, target: &str) -> String {
    if let Some(caller) = caller
        && validate_wait_edge(shared, Some(caller), target).is_err()
    {
        return format!("lan inbox {caller}");
    }
    format!("lan wait {target}")
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
    value.map(Duration::from_millis).unwrap_or(DEFAULT_WAIT)
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
}
