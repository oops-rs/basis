//! Who drives a task: this process, from acceptance (or the next start) to
//! its terminal — see the parent module. A hosted task's driver only waits:
//! its host runs it.

use std::{
    collections::HashMap,
    sync::{Arc, MutexGuard},
    time::Duration,
};

use basis_tasks::{POLL, TaskHandle, Tasks, WaitOutcome, now_ms};
use futures::FutureExt;
use tokio::sync::watch;

use super::{
    A2aService, Backend,
    ask::{CallerHost, Questions},
};
use crate::{config::InboundApproval, host::TaskHost, index::Record, project::Standing};

/// This process's hold on one task it drives.
#[derive(Clone)]
pub(crate) struct Driver {
    /// Flipped to `true` when the driver returns.
    done: watch::Receiver<bool>,
    /// The question the task has open to its caller, if any.
    pub(crate) questions: Arc<Questions>,
}

impl A2aService {
    fn drivers(&self) -> MutexGuard<'_, HashMap<String, Driver>> {
        self.inner
            .drivers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The questions `id` has open, while this process drives it.
    pub(crate) fn questions(&self, id: &str) -> Option<Arc<Questions>> {
        self.drivers()
            .get(id)
            .map(|driver| Arc::clone(&driver.questions))
    }

    /// Returns once this process's driver for `id` has returned — at once
    /// when there is none.
    pub(crate) async fn settled(&self, id: &str) {
        let driver = self.drivers().get(id).cloned();
        if let Some(mut driver) = driver {
            let _ = driver.done.wait_for(|done| *done).await;
        }
    }

    /// Returns once `id` has settled or is waiting on its caller — where a
    /// blocking `SendMessage` answers (spec §3.1.1).
    pub(crate) async fn settled_or_asking(&self, record: &Record) {
        let Some(mut driver) = self.drivers().get(&record.id).cloned() else {
            return;
        };
        match self.backend() {
            Backend::Tasks(_) => {
                let mut asked = driver.questions.watch();
                tokio::select! {
                    _ = driver.done.wait_for(|done| *done) => {}
                    _ = asked.wait_for(Option::is_some) => {}
                }
            }
            // A host's question is read off its standing (ADR-0034), at the
            // cadence a stream reads it.
            Backend::Hosted(_) => {
                tokio::select! {
                    _ = driver.done.wait_for(|done| *done) => {}
                    () = self.host_asks(record) => {}
                }
            }
        }
    }

    /// Returns once `record`'s hosted task stands `INPUT_REQUIRED` — or its
    /// standing cannot be read, which nothing waiting on it can wait out.
    async fn host_asks(&self, record: &Record) {
        loop {
            match self.standing(record).await {
                Ok(Standing::HostAsking(_)) | Err(_) => return,
                Ok(_) => tokio::time::sleep(POLL).await,
            }
        }
    }

    /// Stops driving every task, and returns once no driver is left.
    ///
    /// Each turn in flight is abandoned where it stands, as a crash would
    /// leave it, and nothing is recorded about it: no terminal state, and a
    /// question the turn had open to its caller is resolved as interrupted,
    /// so a later answer to it is refused. Every task stays resumable, and
    /// the next [`open`](A2aService::open) drives it again — a turn that was
    /// asking asks again, under a fresh call id. Nothing new is driven once
    /// this is called. Idempotent.
    ///
    /// [`A2aServer::serve_until`](crate::A2aServer::serve_until) calls this
    /// once its listener has drained. A host serving [`router`](crate::router)
    /// on a listener of its own calls it before its runtime goes away: a
    /// driver works on a blocking thread, which the runtime's shutdown does
    /// not stop.
    pub async fn shutdown(&self) {
        self.inner.stop.stop();
        let drivers: Vec<Driver> = self.drivers().values().cloned().collect();
        for driver in &drivers {
            driver.questions.interrupt();
        }
        for mut driver in drivers {
            let _ = driver.done.wait_for(|done| *done).await;
        }
    }

    fn stopping(&self) -> bool {
        self.inner.stop.is_stopping()
    }

    /// Drives `record`'s task to its terminal on a task of its own — for a
    /// hosted task, awaits its host's word that it has settled.
    pub(super) fn drive(&self, record: &Record) {
        let questions = Arc::new(Questions::new());
        if self.stopping() {
            questions.interrupt();
        }
        let id = record.id.clone();
        let run = match self.backend() {
            Backend::Tasks(backend) => {
                let Some(handle) = record.handle().cloned() else {
                    eprintln!("basis-a2a: task {id} is not a basis-tasks task; not driving it");
                    return;
                };
                // A task's questions go to its own caller, so its driver asks
                // through a host bound to this task alone.
                let tasks = match backend.approval {
                    InboundApproval::AskCaller { deadline } => {
                        backend.tasks.clone().with_prompt_host(Arc::new(CallerHost {
                            questions: Arc::clone(&questions),
                            deadline,
                        }))
                    }
                    InboundApproval::Never | InboundApproval::Always => backend.tasks.clone(),
                };
                drive_task(self.clone(), tasks, handle, id.clone()).boxed()
            }
            Backend::Hosted(host) => {
                let Some(host_id) = record.host_id().map(str::to_string) else {
                    eprintln!("basis-a2a: task {id} is not a hosted task; not awaiting it");
                    return;
                };
                await_hosted(
                    self.clone(),
                    Arc::clone(host),
                    host_id,
                    id.clone(),
                    record.deadline_ms,
                )
                .boxed()
            }
        };
        let (done, receiver) = watch::channel(false);
        self.drivers().insert(
            id.clone(),
            Driver {
                done: receiver,
                questions,
            },
        );
        let service = self.clone();
        tokio::spawn(async move {
            run.await;
            service.drivers().remove(&id);
            let _ = done.send(true);
        });
    }

    /// Records that `id` settled, now.
    fn record_settled(&self, id: &str) {
        if let Err(error) = self.inner.index.finish(id, now_ms()) {
            eprintln!("basis-a2a: record task {id} settling: {error}");
        }
    }

    pub(super) fn resume_unsettled(&self) {
        for record in self.inner.index.all() {
            if record.finished_ms.is_some() {
                continue;
            }
            let backend = match self.backend() {
                Backend::Tasks(backend) => backend,
                // A host's task is awaited again, never started again
                // (ADR-0033 §5): if it settled while nobody here was
                // watching, its host says so at once.
                Backend::Hosted(_) => {
                    self.drive(&record);
                    continue;
                }
            };
            let Some(handle) = record.handle() else {
                eprintln!(
                    "basis-a2a: task {} is not a basis-tasks task; not resuming it",
                    record.id
                );
                continue;
            };
            match backend.tasks.terminal(handle) {
                // Settled while nobody here was watching: the time is when we
                // noticed, the closest fact there is.
                Ok(Some(_)) => self.record_settled(&record.id),
                Ok(None) => self.drive(&record),
                Err(error) => eprintln!("basis-a2a: resume task {}: {error}", record.id),
            }
        }
    }
}

/// Attaches to a `basis-tasks` task and runs it to its terminal.
async fn drive_task(service: A2aService, tasks: Tasks, handle: TaskHandle, id: String) {
    // `Duration::MAX` is "until it settles": the task's own deadline bounds
    // the run, and a wait is only ever an observation of it.
    match tasks.wait(&handle, None, Duration::MAX, None).await {
        Ok(WaitOutcome::Terminal(_)) => service.record_settled(&id),
        // Shutting down: left for the next start, on purpose.
        Ok(WaitOutcome::TimedOut { .. }) if service.stopping() => {}
        Ok(WaitOutcome::TimedOut { .. }) => {
            eprintln!("basis-a2a: task {id} stopped being driven before it settled");
        }
        Err(error) => eprintln!("basis-a2a: drive task {id}: {error}"),
    }
}

/// Waits for a host's task to settle, and records that it did. A shutdown
/// stops the waiting and records nothing; the next start waits again.
///
/// A task still unsettled at its deadline is asked to stop, once: the host
/// was to have settled it by then (ADR-0033 §1), and the server is what
/// promised the caller the deadline. The waiting goes on, because only the
/// host can say how the task ended — what it reports is what the caller is
/// told.
async fn await_hosted(
    service: A2aService,
    host: Arc<dyn TaskHost>,
    host_id: String,
    id: String,
    deadline_ms: Option<u64>,
) {
    let overdue = async {
        let Some(deadline_ms) = deadline_ms else {
            return std::future::pending().await;
        };
        tokio::time::sleep(Duration::from_millis(deadline_ms.saturating_sub(now_ms()))).await;
        eprintln!("basis-a2a: hosted task {id} is past its deadline; canceling it");
        if let Err(error) = host.cancel(&host_id).await {
            eprintln!("basis-a2a: cancel overdue hosted task {id}: {error}");
        }
        std::future::pending::<()>().await
    };
    tokio::select! {
        // A task that has settled is recorded so, even as the server stops.
        biased;
        settled = host.settled(&host_id) => match settled {
            Ok(()) => service.record_settled(&id),
            Err(error) => eprintln!("basis-a2a: await hosted task {id}: {error}"),
        },
        () = service.inner.stop.stopping() => {}
        () = overdue => {}
    }
}
