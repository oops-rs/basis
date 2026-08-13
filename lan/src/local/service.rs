//! The long-lived owner behind asynchronous lifecycle commands.
//!
//! One service owns one canonical workspace. Clients submit bounded JSON
//! requests over loopback TCP, but neither sockets nor serialization leak into
//! `lan-core`: this is an adapter owned by the binary. Task graph mutations are
//! short synchronous critical sections; model work, disk writes, and client
//! waits happen outside them, so the control plane remains responsive.

use std::{
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use lan_core::{
    AllowAll, Approver, Bound, CancellationToken, DenyAll, Effort, Event, EventSink, ModelSelector,
    RunConfig, RunOutcome, ShellAccess, TurnOptions, provider,
};
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, watch},
    time::{self, Instant},
};
use uuid::Uuid;

use super::{
    protocol::{
        MAX_PROMPT, Operation, Request, RunOptions as WireRunOptions, VERSION, error, ok,
        read_frame, write_frame,
    },
    registry::{
        Descriptor, Registry, canonical_workspace, new_token, workspace_key, write_descriptor,
    },
    store::{self, DurableState, Journal, TaskRecord},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_WAIT: Duration = Duration::from_secs(30 * 60);

fn notify_changed(shared: &Shared) {
    shared
        .changed
        .send_modify(|version| *version = version.saturating_add(1));
}

#[derive(Clone)]
struct Shared {
    registry: Registry,
    descriptor: Descriptor,
    workspace: PathBuf,
    journal: Arc<Mutex<Journal>>,
    controls: Arc<Mutex<HashMap<String, CancellationToken>>>,
    persist_gate: Arc<AsyncMutex<()>>,
    changed: watch::Sender<u64>,
}

/// Runs the hidden per-workspace service. A filesystem lease selects exactly
/// one owner, while the actual operations remain capability-scoped.
pub(crate) async fn run_daemon(workspace: PathBuf, registry: PathBuf) -> Result<(), String> {
    let workspace = canonical_workspace(&workspace)
        .map_err(|error| format!("resolve workspace {}: {error}", workspace.display()))?;
    let registry = Registry::from_path(registry)
        .map_err(|error| format!("open lan service registry: {error}"))?;
    let _reservation = match registry.acquire(&workspace) {
        Ok(reservation) => reservation,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(format!("reserve lan service: {error}")),
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("bind lan service: {error}"))?;
    let instance = workspace_key(&workspace);
    let descriptor = Descriptor {
        version: VERSION,
        instance: instance.clone(),
        workspace: workspace.to_string_lossy().into_owned(),
        endpoint: listener
            .local_addr()
            .map_err(|error| format!("read lan service address: {error}"))?
            .to_string(),
        token: new_token(),
        pid: std::process::id(),
    };
    let journal =
        store::load(&registry, &instance).map_err(|error| format!("load task journal: {error}"))?;
    let (changed, _) = watch::channel(0_u64);
    let shared = Shared {
        registry: registry.clone(),
        descriptor: descriptor.clone(),
        workspace: workspace.clone(),
        journal: Arc::new(Mutex::new(journal)),
        controls: Arc::new(Mutex::new(HashMap::new())),
        persist_gate: Arc::new(AsyncMutex::new(())),
        changed,
    };

    write_descriptor(&registry.workspace_descriptor(&workspace), &descriptor)
        .map_err(|error| format!("publish lan service: {error}"))?;
    write_descriptor(&registry.instance_descriptor(&instance), &descriptor)
        .map_err(|error| format!("publish lan service handle: {error}"))?;

    let result = accept_loop(listener, shared.clone()).await;
    orphan_running(&shared).await;
    let _ = registry.remove_descriptor(&descriptor);
    result
}

async fn accept_loop(listener: TcpListener, shared: Shared) -> Result<(), String> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|error| format!("accept lan client: {error}"))?;
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, shared).await;
                });
            }
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| format!("listen for shutdown: {error}"))?;
                return Ok(());
            }
        }
    }
}

async fn handle_connection(mut stream: TcpStream, shared: Shared) -> Result<(), String> {
    let request = time::timeout(HANDSHAKE_TIMEOUT, read_frame::<_, Request>(&mut stream))
        .await
        .map_err(|_| "lan client did not finish its request in time".to_string())?
        .map_err(|error| format!("read lan request: {error}"))?;
    let id = request.id;
    let response = if request.version != VERSION {
        error(
            id,
            format!("unsupported local protocol version {}", request.version),
        )
    } else if request.token != shared.descriptor.token {
        error(id, "invalid lan service capability")
    } else {
        match dispatch(request.operation, &shared).await {
            Ok(payload) => ok(id, payload),
            Err(message) => error(id, message),
        }
    };
    write_frame(&mut stream, &response)
        .await
        .map_err(|error| format!("write lan response: {error}"))
}

async fn dispatch(operation: Operation, shared: &Shared) -> Result<Value, String> {
    match operation {
        Operation::Spawn {
            workspace,
            prompt,
            parent,
            detached,
            await_result,
            timeout_ms,
            options,
        } => {
            let task = spawn_task(shared, workspace, prompt, parent, detached, options).await?;
            if await_result {
                await_task(
                    shared,
                    &task,
                    task_parent(shared, &task)?,
                    duration_from_ms(timeout_ms),
                )
                .await
            } else {
                Ok(accepted_payload(&task))
            }
        }
        Operation::Send {
            task,
            message,
            caller,
            await_result,
            timeout_ms,
        } => {
            if await_result {
                validate_wait_edge(shared, caller.as_deref(), &task)?;
            }
            let message_id = enqueue_message(shared, &task, message).await?;
            if await_result {
                await_task(shared, &task, caller, duration_from_ms(timeout_ms)).await
            } else {
                Ok(json!({
                    "task": task,
                    "message": message_id,
                    "state": "accepted",
                    "next": format!("lan wait {task}"),
                }))
            }
        }
        Operation::Wait {
            task,
            caller,
            timeout_ms,
        } => {
            validate_wait_edge(shared, caller.as_deref(), &task)?;
            await_task(shared, &task, caller, Duration::from_millis(timeout_ms)).await
        }
        Operation::Cancel { task } => cancel_task(shared, &task).await,
        Operation::Watch {
            task,
            since,
            timeout_ms,
        } => watch_task(shared, &task, since, Duration::from_millis(timeout_ms)).await,
        Operation::Inbox { task } if task == "__probe__" => Ok(json!({"state": "ready"})),
        Operation::Inbox { task } => inbox(shared, &task),
    }
}

async fn spawn_task(
    shared: &Shared,
    workspace: String,
    prompt: String,
    parent: Option<String>,
    detached: bool,
    options: WireRunOptions,
) -> Result<String, String> {
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

    let task = format!("{}/{}", shared.descriptor.instance, Uuid::new_v4().simple());
    let requested_deadline = options
        .deadline_ms
        .and_then(|after| store::now_ms().checked_add(after));
    let deadline_at = {
        let journal = shared.journal.lock().expect("task journal poisoned");
        match parent.as_deref() {
            Some(parent_id) => {
                let owner = journal
                    .get(parent_id)
                    .ok_or_else(|| format!("parent task {parent_id} does not exist"))?;
                if owner.state.is_terminal() {
                    return Err(format!("parent task {parent_id} is no longer running"));
                }
                earlier_deadline(requested_deadline, owner.deadline_at_ms)
            }
            None => requested_deadline,
        }
    };

    let record = TaskRecord::new(
        task.clone(),
        parent,
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
        journal.insert(task.clone(), record);
    }
    if let Err(error) = persist(shared).await {
        shared
            .journal
            .lock()
            .expect("task journal poisoned")
            .remove(&task);
        return Err(error);
    }

    let cancellation = CancellationToken::default();
    shared
        .controls
        .lock()
        .expect("task controls poisoned")
        .insert(task.clone(), cancellation.clone());
    let worker = shared.clone();
    let task_for_worker = task.clone();
    tokio::spawn(async move {
        run_task(worker, task_for_worker, prompt, options, cancellation).await;
    });
    notify_changed(shared);
    Ok(task)
}

async fn run_task(
    shared: Shared,
    task: String,
    prompt: String,
    options: WireRunOptions,
    cancellation: CancellationToken,
) {
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

async fn settle_or_take_message(
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

async fn enqueue_message(shared: &Shared, task: &str, message: String) -> Result<String, String> {
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

async fn await_task(
    shared: &Shared,
    task: &str,
    _caller: Option<String>,
    timeout: Duration,
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

async fn watch_task(
    shared: &Shared,
    task: &str,
    since: u64,
    timeout: Duration,
) -> Result<Value, String> {
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

fn inbox(shared: &Shared, task: &str) -> Result<Value, String> {
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

async fn cancel_task(shared: &Shared, task: &str) -> Result<Value, String> {
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

async fn finish_failed(shared: &Shared, task: &str, message: String, stopped_by: Option<String>) {
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

async fn finish_cancelled(shared: &Shared, task: &str) {
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

async fn orphan_running(shared: &Shared) {
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

async fn persist(shared: &Shared) -> Result<(), String> {
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

fn accepted_payload(task: &str) -> Value {
    json!({
        "task": task,
        "state": "running",
        "next": format!("lan wait {task}"),
    })
}

fn task_parent(shared: &Shared, task: &str) -> Result<Option<String>, String> {
    shared
        .journal
        .lock()
        .expect("task journal poisoned")
        .get(task)
        .map(|record| record.parent.clone())
        .ok_or_else(|| format!("task {task} does not exist"))
}

fn deadline_of(shared: &Shared, task: &str) -> Option<u64> {
    shared
        .journal
        .lock()
        .expect("task journal poisoned")
        .get(task)
        .and_then(|record| record.deadline_at_ms)
}

fn is_cancel_requested(shared: &Shared, task: &str) -> bool {
    shared
        .journal
        .lock()
        .expect("task journal poisoned")
        .get(task)
        .is_none_or(|record| record.cancel_requested)
}

fn validate_wait_edge(shared: &Shared, caller: Option<&str>, target: &str) -> Result<(), String> {
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

fn duration_from_ms(value: Option<u64>) -> Duration {
    value.map(Duration::from_millis).unwrap_or(DEFAULT_WAIT)
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

fn human_duration(duration: Duration) -> String {
    if duration.as_secs().is_multiple_of(60) {
        format!("{}m", duration.as_secs() / 60)
    } else {
        format!("{}s", duration.as_secs())
    }
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
    use crate::local::protocol::{Response, ResponseKind, write_frame};
    use tempfile::TempDir;

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

    #[tokio::test]
    async fn enqueue_only_send_to_an_ancestor_does_not_add_a_wait_edge() {
        let (_dir, shared) = test_shared();
        {
            let mut journal = shared.journal.lock().expect("journal");
            journal.insert("root".to_string(), record("root", None));
            journal.insert("child".to_string(), record("child", Some("root")));
        }

        let payload = dispatch(
            Operation::Send {
                task: "root".to_string(),
                message: "status update".to_string(),
                caller: Some("child".to_string()),
                await_result: false,
                timeout_ms: None,
            },
            &shared,
        )
        .await
        .expect("enqueue-only parent send is safe");
        assert_eq!(payload["state"], "accepted");

        let error = dispatch(
            Operation::Send {
                task: "root".to_string(),
                message: "blocking question".to_string(),
                caller: Some("child".to_string()),
                await_result: true,
                timeout_ms: Some(1),
            },
            &shared,
        )
        .await
        .expect_err("awaiting an ancestor would close the wait cycle");
        assert!(error.contains("ancestor"), "{error}");
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
    fn attached_deadlines_can_only_narrow_the_parent() {
        assert_eq!(earlier_deadline(Some(20), Some(10)), Some(10));
        assert_eq!(earlier_deadline(None, Some(10)), Some(10));
        assert_eq!(earlier_deadline(Some(20), None), Some(20));
        assert_eq!(earlier_deadline(None, None), None);
    }

    #[tokio::test]
    async fn capability_is_checked_before_any_operation_mutates_state() {
        let (_dir, shared) = test_shared();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let descriptor = Descriptor {
            endpoint: listener.local_addr().expect("address").to_string(),
            token: "right-token".to_string(),
            ..shared.descriptor.clone()
        };
        let shared = Shared {
            descriptor: descriptor.clone(),
            ..shared
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            handle_connection(stream, shared.clone())
                .await
                .expect("respond");
            assert!(
                shared.journal.lock().expect("journal").is_empty(),
                "an unauthenticated request must not reach dispatch"
            );
        });

        let mut stream = TcpStream::connect(&descriptor.endpoint)
            .await
            .expect("connect");
        write_frame(
            &mut stream,
            &Request {
                version: VERSION,
                id: 7,
                token: "wrong-token".to_string(),
                operation: Operation::Inbox {
                    task: "__probe__".to_string(),
                },
            },
        )
        .await
        .expect("request");
        let response: Response = read_frame(&mut stream).await.expect("response");
        assert!(matches!(response.kind, ResponseKind::Error));
        assert_eq!(response.id, 7);
        server.await.expect("server joins");
    }
}
