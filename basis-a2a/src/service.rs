//! The A2A operations, over one workspace's `basis-tasks`.
//!
//! [`A2aService`] is transport-free: every operation takes the spec's request
//! type and returns the spec's response type or an [`A2AError`]. The
//! JSON-RPC binding in [`crate::rpc`] is one way in; a host that wants a
//! different one calls these directly.
//!
//! # Who drives a task
//!
//! This process does. `basis-tasks` lets any process attach and drive a task
//! (ADR-0019), and nothing else is going to: an A2A caller cannot attach to a
//! directory on this machine. So every task this service accepts gets a
//! driver — one tokio task holding a `Tasks::wait` that attaches and runs it
//! to its terminal — and [`A2aService::open`] gives one to every task the
//! index says it accepted and that has not settled, so a restart costs a
//! task its in-flight turn, never the task.
//!
//! Every spawn is `detached`: a served task is an independent root, never a
//! child of whatever task the serving process might itself be running as.

mod parts;

pub(crate) use parts::INPUT_MODES;
mod send;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use a2a::{
    A2AError, CancelTaskRequest, GetTaskRequest, ListTasksRequest, ListTasksResponse,
    SendMessageRequest, SendMessageResponse, StreamResponse, SubscribeToTaskRequest, Task,
};
use basis_tasks::{Tasks, WaitOutcome, now_ms};
use futures::stream::BoxStream;
use tokio::sync::watch;

use crate::{
    config::{
        AgentIdentity, InboundApproval, Principal, PrincipalError, Principals, RunDefaults,
        ServeConfig,
    },
    index::{Index, Record},
    project::{self, Standing, View, internal},
    stream,
};

/// The name `Tasks::host_dir` files this crate's index under.
const HOST: &str = "a2a";

/// How long `CancelTask` waits for the cancel to land before answering with
/// whatever state the task is in. Cancellation is honored at turn boundaries
/// (ADR-0019), so a long turn can outlast this; the caller then sees
/// `WORKING` and a later `GetTask` sees `CANCELED`.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// `ListTasks`' page size when the caller names none, and the most it may name.
const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 100;

/// Why a service could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("open the workspace's tasks: {0}")]
    Tasks(#[from] basis_tasks::Error),
    #[error("read the A2A task index: {0}")]
    Index(#[from] std::io::Error),
    #[error("configure the server's principals: {0}")]
    Principals(#[from] PrincipalError),
}

/// One served agent's A2A operations. Cheap to clone; every clone is the
/// same service.
#[derive(Clone)]
pub struct A2aService {
    inner: Arc<Inner>,
}

struct Inner {
    tasks: Tasks,
    index: Index,
    approval: InboundApproval,
    shell: bool,
    run: RunDefaults,
    identity: AgentIdentity,
    principals: Principals,
    public_url: Option<String>,
    /// One receiver per task this process is driving, flipped to `true` when
    /// its driver returns. Absent for a task nobody here is driving.
    drivers: Mutex<HashMap<String, watch::Receiver<bool>>>,
}

impl A2aService {
    /// Opens the workspace's tasks and this server's index, and resumes
    /// driving every task it accepted that has not settled. Must be called
    /// inside a tokio runtime.
    pub fn open(config: ServeConfig) -> Result<Self, OpenError> {
        let ServeConfig {
            workspace,
            identity,
            approval,
            shell,
            run,
            tokens,
            public_url,
            data_dir,
        } = config;
        let principals = Principals::new(tokens)?;
        let tasks = match data_dir {
            Some(data_dir) => Tasks::open_at(data_dir, workspace)?,
            None => Tasks::open(workspace)?,
        };
        let index = Index::open(&tasks.host_dir(HOST)?)?;
        let service = Self {
            inner: Arc::new(Inner {
                tasks,
                index,
                approval,
                shell,
                run,
                identity,
                principals,
                public_url,
                drivers: Mutex::new(HashMap::new()),
            }),
        };
        service.resume_unsettled();
        Ok(service)
    }

    pub(crate) fn identity(&self) -> &AgentIdentity {
        &self.inner.identity
    }

    pub(crate) fn principals(&self) -> &Principals {
        &self.inner.principals
    }

    pub(crate) fn public_url(&self) -> Option<&str> {
        self.inner.public_url.as_deref()
    }

    pub(crate) fn tasks(&self) -> &Tasks {
        &self.inner.tasks
    }

    /// `SendMessage`: starts a task, and — unless the caller asked to be
    /// answered at once — waits for it to settle.
    pub async fn send_message(
        &self,
        caller: &Principal,
        request: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let accepted = self.accept(caller, request).await?;
        if !accepted.return_immediately {
            self.settled(&accepted.record.id).await;
        }
        let standing = Standing::read(self.tasks(), &accepted.record)?;
        Ok(SendMessageResponse::Task(project::task(
            &accepted.record,
            &standing,
            View::full(accepted.history_length),
        )))
    }

    /// `SendStreamingMessage`: starts a task and streams it — the task
    /// first, then its progress, ending at its terminal state.
    pub async fn send_streaming_message(
        &self,
        caller: &Principal,
        request: SendMessageRequest,
    ) -> Result<BoxStream<'static, StreamResponse>, A2AError> {
        let accepted = self.accept(caller, request).await?;
        stream::follow(self.clone(), accepted.record, accepted.history_length)
    }

    /// `GetTask`.
    pub fn get_task(&self, caller: &Principal, request: GetTaskRequest) -> Result<Task, A2AError> {
        let history_length = project::history_length(request.history_length)?;
        let record = self.record(caller, &request.id)?;
        let standing = Standing::read(self.tasks(), &record)?;
        Ok(project::task(
            &record,
            &standing,
            View::full(history_length),
        ))
    }

    /// `ListTasks`: the caller's tasks, most recently changed first.
    pub fn list_tasks(
        &self,
        caller: &Principal,
        request: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let page_size = match request.page_size {
            None => DEFAULT_PAGE_SIZE,
            Some(size) => usize::try_from(size)
                .ok()
                .filter(|size| (1..=MAX_PAGE_SIZE).contains(size))
                .ok_or_else(|| {
                    A2AError::invalid_params(format!("pageSize must be 1 to {MAX_PAGE_SIZE}"))
                })?,
        };
        let offset = match request.page_token.as_deref() {
            None | Some("") => 0,
            Some(token) => token
                .parse::<usize>()
                .map_err(|_| A2AError::invalid_params("pageToken is not one this server issued"))?,
        };
        let history_length = project::history_length(request.history_length)?;
        let view = View {
            history_length,
            include_artifacts: request.include_artifacts.unwrap_or(false),
        };
        let after_ms = request
            .status_timestamp_after
            .map(|after| after.timestamp_millis());

        let mut matching = Vec::new();
        for record in self.inner.index.all_owned(caller) {
            if request
                .context_id
                .as_ref()
                .is_some_and(|context| *context != record.context_id)
            {
                continue;
            }
            if after_ms
                .is_some_and(|after| i64::try_from(record.status_ms()).unwrap_or(i64::MAX) <= after)
            {
                continue;
            }
            let standing = Standing::read(self.tasks(), &record)?;
            if request
                .status
                .as_ref()
                .is_some_and(|status| *status != standing.state())
            {
                continue;
            }
            matching.push((record, standing));
        }
        matching.sort_by(|(left, _), (right, _)| {
            right
                .status_ms()
                .cmp(&left.status_ms())
                .then_with(|| left.id.cmp(&right.id))
        });

        let total = matching.len();
        let tasks: Vec<Task> = matching
            .iter()
            .skip(offset)
            .take(page_size)
            .map(|(record, standing)| project::task(record, standing, view))
            .collect();
        let next = offset.saturating_add(page_size);
        Ok(ListTasksResponse {
            page_size: i32::try_from(tasks.len()).unwrap_or(i32::MAX),
            tasks,
            next_page_token: if next < total {
                next.to_string()
            } else {
                String::new()
            },
            total_size: i32::try_from(total).unwrap_or(i32::MAX),
        })
    }

    /// `CancelTask`: asks the task to stop at its next turn boundary and
    /// answers with where it stands once that lands, or once
    /// [`CANCEL_GRACE`] has passed.
    pub async fn cancel_task(
        &self,
        caller: &Principal,
        request: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let record = self.record(caller, &request.id)?;
        if Standing::read(self.tasks(), &record)?.is_terminal() {
            return Err(A2AError::task_not_cancelable(&request.id));
        }
        let tasks = self.tasks().clone();
        let handle = record.handle();
        tokio::task::spawn_blocking(move || tasks.cancel(&handle, None))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        let _ = tokio::time::timeout(CANCEL_GRACE, self.settled(&record.id)).await;
        let standing = Standing::read(self.tasks(), &record)?;
        Ok(project::task(&record, &standing, View::full(None)))
    }

    /// `SubscribeToTask`: the task now, then its progress to its terminal
    /// state. A settled task has nothing left to stream.
    pub fn subscribe_to_task(
        &self,
        caller: &Principal,
        request: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, StreamResponse>, A2AError> {
        let record = self.record(caller, &request.id)?;
        if Standing::read(self.tasks(), &record)?.is_terminal() {
            return Err(A2AError::unsupported_operation(format!(
                "task {} has settled; GetTask returns its result",
                record.id
            )));
        }
        stream::follow(self.clone(), record, None)
    }

    /// The task `id` names, when `caller` owns it; another principal's task
    /// is refused with the very error a missing one is.
    fn record(&self, caller: &Principal, id: &str) -> Result<Record, A2AError> {
        self.inner
            .index
            .owned(caller, id)
            .ok_or_else(|| A2AError::task_not_found(id))
    }

    fn drivers(&self) -> MutexGuard<'_, HashMap<String, watch::Receiver<bool>>> {
        self.inner
            .drivers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns once this process's driver for `id` has returned — at once
    /// when there is none.
    pub(crate) async fn settled(&self, id: &str) {
        let receiver = self.drivers().get(id).cloned();
        if let Some(mut receiver) = receiver {
            let _ = receiver.wait_for(|done| *done).await;
        }
    }

    /// Drives `record`'s task to its terminal on a task of its own.
    fn drive(&self, record: &Record) {
        let (done, receiver) = watch::channel(false);
        self.drivers().insert(record.id.clone(), receiver);
        let service = self.clone();
        let id = record.id.clone();
        let handle = record.handle();
        tokio::spawn(async move {
            // `Duration::MAX` is "until it settles": the task's own deadline
            // bounds the run, and a wait is only ever an observation of it.
            match service
                .tasks()
                .wait(&handle, None, Duration::MAX, None)
                .await
            {
                Ok(WaitOutcome::Terminal(_)) => {
                    if let Err(error) = service.inner.index.finish(&id, now_ms()) {
                        eprintln!("basis-a2a: record task {id} settling: {error}");
                    }
                }
                Ok(WaitOutcome::TimedOut { .. }) => {
                    eprintln!("basis-a2a: task {id} stopped being driven before it settled");
                }
                Err(error) => eprintln!("basis-a2a: drive task {id}: {error}"),
            }
            service.drivers().remove(&id);
            let _ = done.send(true);
        });
    }

    fn resume_unsettled(&self) {
        for record in self.inner.index.all() {
            if record.finished_ms.is_some() {
                continue;
            }
            match self.tasks().terminal(&record.handle()) {
                // Settled while nobody here was watching: the time is when we
                // noticed, the closest fact there is.
                Ok(Some(_)) => {
                    if let Err(error) = self.inner.index.finish(&record.id, now_ms()) {
                        eprintln!("basis-a2a: record task {} settling: {error}", record.id);
                    }
                }
                Ok(None) => self.drive(&record),
                Err(error) => eprintln!("basis-a2a: resume task {}: {error}", record.id),
            }
        }
    }
}
