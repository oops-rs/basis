//! The A2A operations, over one workspace's `basis-tasks` or a host's own
//! tasks.
//!
//! [`A2aService`] is transport-free: every operation takes the spec's request
//! type and returns the spec's response type or an [`A2AError`]. The
//! JSON-RPC binding in [`crate::rpc`] is one way in; a host that wants a
//! different one calls these directly.
//!
//! # Who drives a task
//!
//! This process does. For a hosted task, driving is only waiting: the host
//! runs it, and its driver awaits [`TaskHost::settled`] and records the
//! settling — on acceptance, and again on every [`A2aService::open`] until
//! it has. A hosted task paused on a question to its caller (ADR-0034) has
//! not settled, so its driver goes on waiting through the question. The rest of this section is about `basis-tasks`.
//!
//! [`TaskHost::settled`]: crate::TaskHost::settled
//!
//! For a `basis-tasks` task, `basis-tasks` lets any process attach and drive a task
//! (ADR-0019), and nothing else is going to: an A2A caller cannot attach to a
//! directory on this machine. So every task this service accepts gets a
//! driver — one tokio task holding a `Tasks::wait` that attaches and runs it
//! to its terminal — and [`A2aService::open`] gives one to every task the
//! index says it accepted and that has not settled, so a restart costs a
//! task its in-flight turn, never the task.
//!
//! A driver never outlives the service's shutdown. [`A2aService::shutdown`]
//! flips the `Stop` every driver drives under: each abandons its turn where
//! it stands, as a crash would, and records nothing — no terminal state, and
//! no answer to a question it had open, which is resolved as interrupted.
//! The next start drives the task again from what its conversation
//! committed. Dropping the last handle on the service does the same, for a
//! host that never said it was going.
//!
//! Every `basis-tasks` spawn is `detached`: a served task is an independent root, never a
//! child of whatever task the serving process might itself be running as.

mod ask;
mod backend;
mod drive;
mod hops;
mod parts;
mod push;
mod send;
mod streams;

pub(crate) use ask::{APPROVAL_EXTENSION, Asked};
pub(crate) use backend::{Backend, TasksBackend, from_host, host_id_of, still_working};
pub use hops::DEFAULT_HOP_CAP;
pub(crate) use hops::HOPS_EXTENSION;
pub(crate) use parts::INPUT_MODES;
pub(crate) use push::Push;
pub use streams::DEFAULT_STREAM_CAP;
pub(crate) use streams::StreamSlot;
use streams::Streams;

use std::{
    collections::HashMap,
    fs, io,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use a2a::{
    A2AError, CancelTaskRequest, GetTaskRequest, ListTasksRequest, ListTasksResponse,
    SendMessageRequest, SendMessageResponse, StreamResponse, SubscribeToTaskRequest, Task,
    TaskStatusUpdateEvent,
};
use basis_tasks::{Stop, Tasks};
use futures::stream::BoxStream;

use crate::{
    authenticator::{Authenticator, Security},
    card::Features,
    config::{
        AgentIdentity, Custom, ExtendedCard, InboundApproval, Principal, PrincipalError,
        Principals, ServeConfig, Source,
    },
    index::{Index, Record},
    project::{self, Standing, View, internal},
    push::PushStore,
    stream,
};
use ask::NobodyHost;
use drive::Driver;

/// The name `Tasks::host_dir` files this crate's index under.
const HOST: &str = "a2a";

/// How long `CancelTask` waits for the cancel to land before answering with
/// whatever state the task is in. A cancel cuts the running turn short, so
/// the task normally settles well inside this; a tool that ignores its
/// cancellation can still outlast it, and the caller then sees `WORKING` and
/// a later `GetTask` sees `CANCELED`.
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
    /// The state directory of a server whose tasks a host runs could not be
    /// made.
    #[error("create the server's state directory: {0}")]
    StateDir(std::io::Error),
    /// A setting that configures a `basis-tasks` run, on a server whose
    /// tasks a host runs (ADR-0033 §2): it would do nothing, so it is
    /// refused rather than ignored.
    #[error(
        "{0} configures a basis-tasks run, and this server's tasks are run by its host; \
         remove it"
    )]
    NotForHostedTasks(&'static str),
    #[error("configure the server's principals: {0}")]
    Principals(#[from] PrincipalError),
    /// Tokens and an authenticator of the operator's own: the authenticator
    /// replaces the tokens (ADR-0036), so they would be silently ignored.
    #[error("this server has both tokens and an authenticator, which replaces them; configure one")]
    TokensWithAuthenticator,
    /// The spec requires the extended card to be served only to
    /// authenticated callers, and a server that authenticates nobody has
    /// none.
    #[error("an extended card needs a token to serve it to; configure one")]
    ExtendedCardWithoutToken,
    /// Push notifications or asking the caller, on a server where every
    /// local process is the same anonymous caller.
    #[error(
        "{0} on a server with no token lets any local process — or a web page that reaches it — \
         use it; configure a token, or trust anonymous callers explicitly"
    )]
    NeedsToken(&'static str),
}

/// One served agent's A2A operations. Cheap to clone; every clone is the
/// same service.
#[derive(Clone)]
pub struct A2aService {
    inner: Arc<Inner>,
}

struct Inner {
    backend: Backend,
    index: Index,
    identity: AgentIdentity,
    authenticator: Arc<dyn Authenticator>,
    /// What `authenticator` declared when the service opened.
    security: Option<Security>,
    public_url: Option<String>,
    extended_card: Option<ExtendedCard>,
    /// Present when push notifications are on.
    push: Option<Push>,
    hop_cap: u32,
    live_task_quota: usize,
    /// How long a task may run, before its caller's deadline clamps it.
    task_deadline: Duration,
    streams: Arc<Streams>,
    /// Serializes the live-task quota check with the insertion it admits.
    admission: tokio::sync::Mutex<()>,
    /// Every task this process is driving. Absent for a task nobody here is
    /// driving.
    drivers: Mutex<HashMap<String, Driver>>,
    /// What every driver drives under; flipped once, at shutdown.
    stop: Stop,
}

impl Drop for Inner {
    /// The last handle on the service is gone without a
    /// [`shutdown`](A2aService::shutdown) — its runtime is being torn down
    /// under it. The drivers' blocking threads outlive their tokio tasks, so
    /// they are told here, or they would go on to record turns nobody hears.
    fn drop(&mut self) {
        self.stop.stop();
        let drivers = self
            .drivers
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for driver in drivers.values() {
            driver.questions.interrupt();
        }
    }
}

impl A2aService {
    /// Opens the workspace's tasks — or, for a hosted config, its state
    /// directory — and this server's index, and resumes driving every task
    /// it accepted that has not settled. Must be called inside a tokio
    /// runtime.
    pub fn open(config: ServeConfig) -> Result<Self, OpenError> {
        let ServeConfig {
            source,
            identity,
            approval,
            shell,
            run,
            tokens,
            authenticator,
            public_url,
            data_dir,
            extended_card,
            push_notifications,
            private_webhooks,
            hop_cap,
            trust_anonymous,
            live_task_quota,
            stream_cap,
            task_deadline,
        } = config;
        if let Source::Hosted { .. } = source {
            refuse_for_hosted(shell, run.is_unset(), approval, data_dir.is_some())?;
        }
        let authenticator: Arc<dyn Authenticator> = match authenticator {
            Some(_) if !tokens.is_empty() => return Err(OpenError::TokensWithAuthenticator),
            Some(Custom(authenticator)) => authenticator,
            None => Arc::new(Principals::new(tokens)?),
        };
        let security = authenticator.security();
        if extended_card.is_some() && security.is_none() {
            return Err(OpenError::ExtendedCardWithoutToken);
        }
        if security.is_none() && !trust_anonymous {
            if push_notifications {
                return Err(OpenError::NeedsToken("push notifications"));
            }
            if matches!(approval, InboundApproval::AskCaller { .. }) {
                return Err(OpenError::NeedsToken("asking the caller"));
            }
        }
        let stop = Stop::new();
        let (backend, state_dir) = match source {
            Source::Workspace(workspace) => {
                let tasks = match data_dir {
                    Some(data_dir) => Tasks::open_at(data_dir, workspace)?,
                    None => Tasks::open(workspace)?,
                };
                // `basis-tasks` spawns a task that will ask only from a
                // `Tasks` that can; each is then driven through a host of
                // its own (`drive`).
                let tasks = match approval {
                    InboundApproval::AskCaller { .. } => {
                        tasks.with_prompt_host(Arc::new(NobodyHost))
                    }
                    InboundApproval::Never | InboundApproval::Always => tasks,
                };
                let tasks = tasks.with_stop(stop.clone());
                let host_dir = tasks.host_dir(HOST)?;
                let backend = TasksBackend {
                    tasks,
                    approval,
                    shell,
                    run,
                };
                (Backend::Tasks(Box::new(backend)), host_dir)
            }
            Source::Hosted { state_dir, host } => {
                create_private_dir(&state_dir).map_err(OpenError::StateDir)?;
                (Backend::Hosted(host), state_dir)
            }
        };
        let index = Index::open(&state_dir)?;
        let push = if push_notifications {
            Some(Push::new(PushStore::open(&state_dir)?, private_webhooks))
        } else {
            None
        };
        let service = Self {
            inner: Arc::new(Inner {
                backend,
                index,
                identity,
                authenticator,
                security,
                public_url,
                extended_card,
                push,
                hop_cap,
                live_task_quota,
                task_deadline: task_deadline.unwrap_or(basis_tasks::DEFAULT_DEADLINE),
                streams: Streams::new(stream_cap),
                admission: tokio::sync::Mutex::new(()),
                drivers: Mutex::new(HashMap::new()),
                stop,
            }),
        };
        service.resume_unsettled();
        service.resume_push();
        Ok(service)
    }

    pub(crate) fn identity(&self) -> &AgentIdentity {
        &self.inner.identity
    }

    pub(crate) fn authenticator(&self) -> &dyn Authenticator {
        self.inner.authenticator.as_ref()
    }

    /// What the card declares a caller must present; `None` when this
    /// service authenticates nobody.
    pub(crate) fn security(&self) -> Option<&Security> {
        self.inner.security.as_ref()
    }

    pub(crate) fn extended_card(&self) -> Option<&ExtendedCard> {
        self.inner.extended_card.as_ref()
    }

    /// What the card may claim: exactly what this service serves.
    pub(crate) fn features(&self) -> Features {
        Features {
            security: self.inner.security.clone(),
            extended_card: self.inner.extended_card.is_some(),
            push_notifications: self.inner.push.is_some(),
            ask_caller: self.inner.backend.asks_caller(),
        }
    }

    pub(crate) fn public_url(&self) -> Option<&str> {
        self.inner.public_url.as_deref()
    }

    pub(crate) fn backend(&self) -> &Backend {
        &self.inner.backend
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
            self.settled_or_asking(&accepted.record).await;
        }
        let standing = self.standing(&accepted.record).await?;
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
        // Taken first, so a caller at its cap starts no task it cannot watch.
        let slot = self.inner.streams.take(caller)?;
        let accepted = self.accept(caller, request).await?;
        stream::follow(self.clone(), accepted.record, accepted.history_length, slot).await
    }

    /// `GetTask`.
    pub async fn get_task(
        &self,
        caller: &Principal,
        request: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let history_length = project::history_length(request.history_length)?;
        let record = self.record(caller, &request.id)?;
        let standing = self.standing(&record).await?;
        Ok(project::task(
            &record,
            &standing,
            View::full(history_length),
        ))
    }

    /// `ListTasks`: the caller's tasks, most recently changed first.
    pub async fn list_tasks(
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
            let standing = self.standing(&record).await?;
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

    /// `CancelTask`: asks the task to stop — cutting its running turn short —
    /// and answers with where it stands once that lands, or once a grace of
    /// a few seconds has passed.
    pub async fn cancel_task(
        &self,
        caller: &Principal,
        request: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let record = self.record(caller, &request.id)?;
        if self.standing(&record).await?.is_terminal() {
            return Err(A2AError::task_not_cancelable(&request.id));
        }
        match self.backend() {
            Backend::Tasks(backend) => {
                let tasks = backend.tasks.clone();
                let handle = project::handle_of(&record)?.clone();
                tokio::task::spawn_blocking(move || tasks.cancel(&handle, None))
                    .await
                    .map_err(internal)?
                    .map_err(internal)?;
                // A turn parked on a question would never reach the boundary
                // the cancel lands at: the question is denied, and so is
                // every later one.
                if let Some(questions) = self.questions(&record.id) {
                    questions.cancel();
                }
            }
            Backend::Hosted(host) => host.cancel(host_id_of(&record)?).await.map_err(from_host)?,
        }
        let _ = tokio::time::timeout(CANCEL_GRACE, self.settled(&record.id)).await;
        let standing = self.standing(&record).await?;
        Ok(project::task(&record, &standing, View::full(None)))
    }

    /// `SubscribeToTask`: the task now, then its progress to its terminal
    /// state. A settled task has nothing left to stream.
    pub async fn subscribe_to_task(
        &self,
        caller: &Principal,
        request: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, StreamResponse>, A2AError> {
        let record = self.record(caller, &request.id)?;
        if self.standing(&record).await?.is_terminal() {
            return Err(A2AError::unsupported_operation(format!(
                "task {} has settled; GetTask returns its result",
                record.id
            )));
        }
        let slot = self.inner.streams.take(caller)?;
        stream::follow(self.clone(), record, None, slot).await
    }

    /// Where `record`'s task stands now, as the status update that says so,
    /// and the key two observations of it compare equal under exactly when
    /// nothing a caller would be told about has changed.
    pub(crate) async fn status_now(
        &self,
        record: &Record,
    ) -> Result<(String, TaskStatusUpdateEvent), A2AError> {
        let standing = self.standing(record).await?;
        let update = project::status_update(record, &standing);
        let state = serde_json::to_value(&update.status.state)
            .ok()
            .and_then(|state| state.as_str().map(str::to_owned))
            .unwrap_or_default();
        // Two questions in a row are two changes, though both are
        // `INPUT_REQUIRED`.
        let key = match &standing {
            Standing::Asking(asked) => format!("{state}:{}", asked.call_id),
            // A host's question has no id; what it asks stands in for one.
            Standing::HostAsking(parts) => format!("{state}:{:016x}", fingerprint(parts)),
            _ => state,
        };
        Ok((key, update))
    }

    /// Where `record`'s task stands: what `basis-tasks` says, with the
    /// question this process holds open for it, if any — or what its host
    /// says.
    pub(crate) async fn standing(&self, record: &Record) -> Result<Standing, A2AError> {
        match self.backend() {
            Backend::Tasks(backend) => {
                let asked = self
                    .questions(&record.id)
                    .and_then(|questions| questions.asked());
                Ok(Standing::read(&backend.tasks, record)?.asking(asked))
            }
            Backend::Hosted(host) => Ok(host
                .standing(host_id_of(record)?)
                .await
                .map_err(from_host)?
                .into()),
        }
    }

    /// The task `id` names, when `caller` owns it; another principal's task
    /// is refused with the very error a missing one is.
    fn record(&self, caller: &Principal, id: &str) -> Result<Record, A2AError> {
        self.inner
            .index
            .owned(caller, id)
            .ok_or_else(|| A2AError::task_not_found(id))
    }
}

/// A digest of a host's question, so two different questions in a row are
/// two changes. Stable within one build, which is as long as a push store's
/// last-seen status needs it to be compared against: a rebuilt server that
/// sees a different digest for the same question sends it once more, which
/// at-least-once delivery allows.
fn fingerprint(parts: &[a2a::Part]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(parts)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Refuses, by name, each setting that configures a `basis-tasks` run on a
/// server whose tasks a host runs (ADR-0033 §2).
fn refuse_for_hosted(
    shell: bool,
    run_unset: bool,
    approval: InboundApproval,
    data_dir: bool,
) -> Result<(), OpenError> {
    let refused = if shell {
        "with_shell"
    } else if !run_unset {
        "with_run_defaults"
    } else if matches!(approval, InboundApproval::AskCaller { .. }) {
        "InboundApproval::AskCaller"
    } else if matches!(approval, InboundApproval::Always) {
        // A host's tasks have no basis-tasks approver for this to open.
        "InboundApproval::Always"
    } else if data_dir {
        "with_data_dir"
    } else {
        return Ok(());
    };
    Err(OpenError::NotForHostedTasks(refused))
}

/// Creates `dir` and any parent it lacks, each readable by its owner alone
/// where the platform says so: it holds push configs' credentials, as a
/// `basis-tasks` host directory does. A directory that already exists is
/// the operator's, and left as it is.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}
