//! Who drives a task: this process, from acceptance (or the next start) to
//! its terminal — see the parent module.

use std::{
    collections::HashMap,
    sync::{Arc, MutexGuard},
    time::Duration,
};

use basis_tasks::{WaitOutcome, now_ms};
use tokio::sync::watch;

use super::{
    A2aService,
    ask::{CallerHost, Questions},
};
use crate::{config::InboundApproval, index::Record};

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
    pub(crate) async fn settled_or_asking(&self, id: &str) {
        let Some(mut driver) = self.drivers().get(id).cloned() else {
            return;
        };
        let mut asked = driver.questions.watch();
        tokio::select! {
            _ = driver.done.wait_for(|done| *done) => {}
            _ = asked.wait_for(Option::is_some) => {}
        }
    }

    /// Drives `record`'s task to its terminal on a task of its own.
    pub(super) fn drive(&self, record: &Record) {
        let (done, receiver) = watch::channel(false);
        let questions = Arc::new(Questions::new());
        self.drivers().insert(
            record.id.clone(),
            Driver {
                done: receiver,
                questions: Arc::clone(&questions),
            },
        );
        // A task's questions go to its own caller, so its driver asks through
        // a host bound to this task alone.
        let tasks = match self.inner.approval {
            InboundApproval::AskCaller { deadline } => {
                self.tasks().clone().with_prompt_host(Arc::new(CallerHost {
                    questions,
                    deadline,
                }))
            }
            InboundApproval::Never | InboundApproval::Always => self.tasks().clone(),
        };
        let service = self.clone();
        let id = record.id.clone();
        let handle = record.handle();
        tokio::spawn(async move {
            // `Duration::MAX` is "until it settles": the task's own deadline
            // bounds the run, and a wait is only ever an observation of it.
            match tasks.wait(&handle, None, Duration::MAX, None).await {
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

    pub(super) fn resume_unsettled(&self) {
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
