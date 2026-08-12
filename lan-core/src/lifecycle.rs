//! Structured task ownership for attached agent work.
//!
//! This module deliberately knows nothing about prompts, models, ACP, or
//! transports. It owns only the facts required to make a child lifecycle
//! finite and observable: an owner, a cancellation signal, a terminal result,
//! and a supervisor that remains responsive while work runs.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time,
};

type TaskFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'static>>;
type TaskFactory = Box<dyn FnOnce(TaskContext) -> TaskFuture + Send + 'static>;

/// An identifier unique within one [`Supervisor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(u64);

impl TaskId {
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task-{}", self.0)
    }
}

/// The only states a task can expose. `Running` is non-terminal; every other
/// state is terminal and is published at most once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Running,
    Succeeded(Vec<u8>),
    Failed(String),
    Cancelled,
    Orphaned,
}

impl TaskState {
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Errors returned while creating or controlling a task.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    #[error("the lifecycle supervisor is closed")]
    Closed,
    #[error("the task id space is exhausted")]
    Exhausted,
    #[error("parent task {0} does not exist")]
    ParentNotFound(TaskId),
    #[error("parent task {0} is no longer running")]
    ParentNotRunning(TaskId),
    #[error("a detached task cannot have an attached parent")]
    DetachedHasParent,
    #[error("task {0} belongs to another supervisor")]
    WrongSupervisor(TaskId),
    #[error("task {0} does not exist")]
    TaskNotFound(TaskId),
}

/// Errors returned by a bounded wait. A timed-out wait is safe to retry: the
/// task's terminal state remains in the supervisor and is never consumed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WaitError {
    #[error("waiting for {task} exceeded {timeout:?}")]
    Timeout { task: TaskId, timeout: Duration },
    #[error("the lifecycle supervisor closed while waiting")]
    Closed,
}

/// A cancellation observation passed to task work.
#[derive(Clone)]
pub struct Cancellation {
    receiver: watch::Receiver<bool>,
}

impl fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cancellation")
            .field("cancelled", &*self.receiver.borrow())
            .finish()
    }
}

impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Waits until cancellation is requested. The watch channel makes the
    /// check-and-wait race-free and safe to cancel and retry.
    pub async fn cancelled(&self) {
        let mut receiver = self.receiver.clone();
        if *receiver.borrow() {
            return;
        }
        let _ = receiver.changed().await;
    }
}

struct CancellationSource {
    sender: watch::Sender<bool>,
    token: Cancellation,
}

impl CancellationSource {
    fn new() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self {
            sender,
            token: Cancellation { receiver },
        }
    }

    fn cancel(&self) {
        self.sender.send_replace(true);
    }
}

/// Context given to one task's work function.
#[derive(Debug)]
pub struct TaskContext {
    id: TaskId,
    cancellation: Cancellation,
}

impl TaskContext {
    pub const fn id(&self) -> TaskId {
        self.id
    }

    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }
}

/// A capability for observing and cancelling one task. Cloning a handle does
/// not clone or rerun the work; all handles observe the same terminal record.
#[derive(Clone)]
pub struct TaskHandle {
    id: TaskId,
    supervisor: Arc<()>,
    commands: mpsc::Sender<Command>,
    completion: watch::Receiver<Option<TaskState>>,
}

impl fmt::Debug for TaskHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskHandle").field("id", &self.id).finish()
    }
}

impl TaskHandle {
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// Returns the latest state without waiting.
    pub fn state(&self) -> TaskState {
        self.completion
            .borrow()
            .clone()
            .unwrap_or(TaskState::Running)
    }

    /// Waits for a terminal state. The result is repeatable, and a timeout
    /// does not consume it.
    pub async fn wait(&self, timeout: Duration) -> Result<TaskState, WaitError> {
        let mut completion = self.completion.clone();
        let task = self.id;
        let wait = async move {
            loop {
                if let Some(state) = completion.borrow().clone() {
                    return Ok(state);
                }
                completion.changed().await.map_err(|_| WaitError::Closed)?;
            }
        };

        match time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err(WaitError::Timeout { task, timeout }),
        }
    }

    /// Requests cancellation of this task and all attached descendants.
    pub async fn cancel(&self) -> Result<(), LifecycleError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Cancel { id: self.id, reply })
            .await
            .map_err(|_| LifecycleError::Closed)?;
        result.await.map_err(|_| LifecycleError::Closed)?
    }
}

/// The lifecycle supervisor. It is an actor-style command loop: state changes
/// are synchronous inside the loop, while task work runs outside it. This is
/// what keeps cancellation, completion, and control messages moving while a
/// caller waits for a child.
#[derive(Clone)]
pub struct Supervisor {
    commands: mpsc::Sender<Command>,
    identity: Arc<()>,
}

impl fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Supervisor").finish_non_exhaustive()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        let (commands, receiver) = mpsc::channel(128);
        let identity = Arc::new(());
        tokio::spawn(run_supervisor(
            receiver,
            commands.clone(),
            Arc::clone(&identity),
        ));
        Self { commands, identity }
    }

    /// Starts work and returns its handle without waiting for completion.
    ///
    /// A child is attached when `parent` is present and `detached` is false.
    /// Detached work must be a new root; accepting a parent for it would make
    /// ownership ambiguous and is rejected structurally.
    pub async fn spawn<F, Fut>(
        &self,
        parent: Option<&TaskHandle>,
        detached: bool,
        work: F,
    ) -> Result<TaskHandle, LifecycleError>
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Vec<u8>, String>> + Send + 'static,
    {
        let parent_id = match parent {
            Some(parent) => {
                if !Arc::ptr_eq(&self.identity, &parent.supervisor) {
                    return Err(LifecycleError::WrongSupervisor(parent.id));
                }
                Some(parent.id)
            }
            None => None,
        };

        if detached && parent_id.is_some() {
            return Err(LifecycleError::DetachedHasParent);
        }

        let factory: TaskFactory = Box::new(move |context| Box::pin(work(context)));
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Spawn {
                parent: parent_id,
                factory,
                reply,
            })
            .await
            .map_err(|_| LifecycleError::Closed)?;
        result.await.map_err(|_| LifecycleError::Closed)?
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

enum Command {
    Spawn {
        parent: Option<TaskId>,
        factory: TaskFactory,
        reply: oneshot::Sender<Result<TaskHandle, LifecycleError>>,
    },
    Complete {
        id: TaskId,
        outcome: Result<Vec<u8>, String>,
    },
    Cancel {
        id: TaskId,
        reply: oneshot::Sender<Result<(), LifecycleError>>,
    },
}

struct TaskRecord {
    parent: Option<TaskId>,
    children: HashSet<TaskId>,
    cancellation: CancellationSource,
    completion: watch::Sender<Option<TaskState>>,
    work: Option<TaskState>,
    terminal: Option<TaskState>,
    worker: JoinHandle<()>,
}

async fn run_supervisor(
    mut commands: mpsc::Receiver<Command>,
    command_sender: mpsc::Sender<Command>,
    identity: Arc<()>,
) {
    let mut next_id = 1_u64;
    let mut records = HashMap::new();

    while let Some(command) = commands.recv().await {
        match command {
            Command::Spawn {
                parent,
                factory,
                reply,
            } => spawn_task(
                &mut next_id,
                &mut records,
                command_sender.clone(),
                identity.clone(),
                parent,
                factory,
                reply,
            ),
            Command::Complete { id, outcome } => {
                complete_task(id, outcome, &mut records);
            }
            Command::Cancel { id, reply } => {
                let result = if records.contains_key(&id) {
                    cancel_tree(id, &mut records);
                    Ok(())
                } else {
                    Err(LifecycleError::TaskNotFound(id))
                };
                let _ = reply.send(result);
            }
        }
    }

    for record in records.values_mut() {
        record.cancellation.cancel();
        record.worker.abort();
        if record.terminal.is_none() {
            record.terminal = Some(TaskState::Orphaned);
            let _ = record.completion.send(Some(TaskState::Orphaned));
        }
    }
}

fn spawn_task(
    next_id: &mut u64,
    records: &mut HashMap<TaskId, TaskRecord>,
    command_sender: mpsc::Sender<Command>,
    identity: Arc<()>,
    parent: Option<TaskId>,
    factory: TaskFactory,
    reply: oneshot::Sender<Result<TaskHandle, LifecycleError>>,
) {
    if let Some(parent_id) = parent {
        let Some(parent_record) = records.get(&parent_id) else {
            let _ = reply.send(Err(LifecycleError::ParentNotFound(parent_id)));
            return;
        };
        if parent_record.work.is_some() || parent_record.terminal.is_some() {
            let _ = reply.send(Err(LifecycleError::ParentNotRunning(parent_id)));
            return;
        }
    }

    let raw_id = *next_id;
    *next_id = match next_id.checked_add(1) {
        Some(next) => next,
        None => {
            let _ = reply.send(Err(LifecycleError::Exhausted));
            return;
        }
    };
    let id = TaskId(raw_id);
    let cancellation = CancellationSource::new();
    let task_context = TaskContext {
        id,
        cancellation: cancellation.token.clone(),
    };
    let (completion_sender, completion_receiver) = watch::channel(None);
    let worker_commands = command_sender.clone();
    let worker = tokio::spawn(async move {
        let outcome = factory(task_context).await;
        let _ = worker_commands
            .send(Command::Complete { id, outcome })
            .await;
    });

    let handle = TaskHandle {
        id,
        supervisor: identity,
        commands: command_sender,
        completion: completion_receiver,
    };
    records.insert(
        id,
        TaskRecord {
            parent,
            children: HashSet::new(),
            cancellation,
            completion: completion_sender,
            work: None,
            terminal: None,
            worker,
        },
    );
    if let Some(parent_id) = parent {
        records
            .get_mut(&parent_id)
            .expect("validated parent remains in the supervisor")
            .children
            .insert(id);
    }
    let _ = reply.send(Ok(handle));
}

fn complete_task(
    id: TaskId,
    outcome: Result<Vec<u8>, String>,
    records: &mut HashMap<TaskId, TaskRecord>,
) {
    let (cancel_children, children) = {
        let Some(record) = records.get_mut(&id) else {
            return;
        };
        if record.terminal.is_some() {
            return;
        }

        let state = match outcome {
            Ok(bytes) => TaskState::Succeeded(bytes),
            Err(error) => TaskState::Failed(error),
        };
        let cancel_children = matches!(state, TaskState::Failed(_));
        record.work = Some(state);
        (
            cancel_children,
            record.children.iter().copied().collect::<Vec<_>>(),
        )
    };

    if cancel_children {
        for child in children {
            cancel_tree(child, records);
        }
    }
    finalize_if_ready(id, records);
}

fn cancel_tree(id: TaskId, records: &mut HashMap<TaskId, TaskRecord>) {
    let children = records
        .get(&id)
        .map(|record| record.children.iter().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for child in children {
        cancel_tree(child, records);
    }

    if let Some(record) = records.get_mut(&id)
        && record.terminal.is_none()
    {
        record.cancellation.cancel();
        record.worker.abort();
        record.work = Some(TaskState::Cancelled);
    }
    finalize_if_ready(id, records);
}

fn finalize_if_ready(id: TaskId, records: &mut HashMap<TaskId, TaskRecord>) {
    let Some((parent, state, completion)) = records.get_mut(&id).and_then(|record| {
        if record.terminal.is_some() || record.work.is_none() || !record.children.is_empty() {
            return None;
        }
        let state = record.work.take().expect("checked above");
        record.terminal = Some(state.clone());
        Some((record.parent, state, record.completion.clone()))
    }) else {
        return;
    };

    let _ = completion.send(Some(state));
    if let Some(parent_id) = parent {
        if let Some(parent_record) = records.get_mut(&parent_id) {
            parent_record.children.remove(&id);
        }
        finalize_if_ready(parent_id, records);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_finished_task_can_be_waited_on_repeatedly() {
        let supervisor = Supervisor::new();
        let task = supervisor
            .spawn(None, false, |_context| async { Ok(b"done".to_vec()) })
            .await
            .expect("spawn succeeds");

        let first = task.wait(Duration::from_secs(1)).await.expect("finishes");
        let second = task.wait(Duration::from_secs(1)).await.expect("repeats");

        assert_eq!(first, TaskState::Succeeded(b"done".to_vec()));
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn a_timed_out_wait_does_not_consume_completion() {
        let supervisor = Supervisor::new();
        let task = supervisor
            .spawn(None, false, |_context| async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(Vec::new())
            })
            .await
            .expect("spawn succeeds");

        assert!(matches!(
            task.wait(Duration::from_millis(1)).await,
            Err(WaitError::Timeout { .. })
        ));
        assert_eq!(
            task.wait(Duration::from_secs(1)).await.expect("finishes"),
            TaskState::Succeeded(Vec::new())
        );
    }

    #[tokio::test]
    async fn cancellation_is_downward_and_settles_waiters() {
        let supervisor = Supervisor::new();
        let parent = supervisor
            .spawn(None, false, |context| async move {
                context.cancellation().cancelled().await;
                Ok(Vec::new())
            })
            .await
            .expect("parent spawns");
        let child = supervisor
            .spawn(Some(&parent), false, |context| async move {
                context.cancellation().cancelled().await;
                Ok(Vec::new())
            })
            .await
            .expect("child spawns");

        parent.cancel().await.expect("cancellation accepted");

        assert_eq!(
            parent
                .wait(Duration::from_secs(1))
                .await
                .expect("parent settles"),
            TaskState::Cancelled
        );
        assert_eq!(
            child
                .wait(Duration::from_secs(1))
                .await
                .expect("child settles"),
            TaskState::Cancelled
        );
    }

    #[tokio::test]
    async fn an_attached_child_keeps_a_successful_parent_scope_open() {
        let supervisor = Supervisor::new();
        let (release_parent, parent_gate) = oneshot::channel();
        let parent = supervisor
            .spawn(None, false, move |_context| async move {
                parent_gate
                    .await
                    .map_err(|_| "parent gate closed".to_string())?;
                Ok(b"parent".to_vec())
            })
            .await
            .expect("parent spawns");
        let child = supervisor
            .spawn(Some(&parent), false, |_context| async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(b"child".to_vec())
            })
            .await
            .expect("child spawns while parent is running");

        release_parent.send(()).expect("parent gate is open");

        assert_eq!(
            child
                .wait(Duration::from_secs(1))
                .await
                .expect("child finishes"),
            TaskState::Succeeded(b"child".to_vec())
        );
        assert_eq!(
            parent
                .wait(Duration::from_secs(1))
                .await
                .expect("parent waits for child"),
            TaskState::Succeeded(b"parent".to_vec())
        );
    }

    #[tokio::test]
    async fn a_parent_cannot_accept_work_after_its_work_finished() {
        let supervisor = Supervisor::new();
        let parent = supervisor
            .spawn(None, false, |_context| async { Ok(Vec::new()) })
            .await
            .expect("parent spawns");
        parent.wait(Duration::from_secs(1)).await.expect("finishes");

        let error = supervisor
            .spawn(Some(&parent), false, |_context| async { Ok(Vec::new()) })
            .await
            .expect_err("a terminal parent cannot own a child");
        assert_eq!(error, LifecycleError::ParentNotRunning(parent.id()));
    }

    #[tokio::test]
    async fn a_detached_task_must_be_a_new_root() {
        let supervisor = Supervisor::new();
        let parent = supervisor
            .spawn(None, false, |_context| async { Ok(Vec::new()) })
            .await
            .expect("parent spawns");

        let error = supervisor
            .spawn(Some(&parent), true, |_context| async { Ok(Vec::new()) })
            .await
            .expect_err("detached children are ambiguous");
        assert_eq!(error, LifecycleError::DetachedHasParent);
    }

    #[tokio::test]
    async fn handles_cannot_cross_supervisors() {
        let first = Supervisor::new();
        let second = Supervisor::new();
        let parent = first
            .spawn(None, false, |_context| async { Ok(Vec::new()) })
            .await
            .expect("parent spawns");

        let error = second
            .spawn(Some(&parent), false, |_context| async { Ok(Vec::new()) })
            .await
            .expect_err("foreign handles are rejected");
        assert_eq!(error, LifecycleError::WrongSupervisor(parent.id()));
    }
}
