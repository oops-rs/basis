//! Who drives a task: this process, from acceptance (or the next start) to
//! its terminal — see the parent module.

use std::{collections::HashMap, sync::MutexGuard, time::Duration};

use basis_tasks::{WaitOutcome, now_ms};
use tokio::sync::watch;

use super::A2aService;
use crate::index::Record;

impl A2aService {
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
    pub(super) fn drive(&self, record: &Record) {
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
