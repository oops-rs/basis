//! The four push-config operations, and when a status change is sent.
//!
//! **Noticing a change.** A task with configs has a monitor: a loop that
//! reads where the task stands every [`POLL`] — the same cadence the stream
//! reads its journal at — and records each change of status in the
//! [`PushStore`]. The monitor starts when a config is created and, for every
//! task with configs, when the service opens; the store remembers the last
//! status it recorded, so a task that settled while no server was running is
//! still a change when the next one looks.
//!
//! **Sending it.** A task with undelivered changes has one deliverer, which
//! sends each config its next change in order and records the delivery
//! before moving on. Delivery is at least once (proposal 0005 §3): a failure
//! is retried with exponential backoff, from [`FIRST_RETRY`] doubling to
//! [`LAST_RETRY`] and then every [`LAST_RETRY`], for as long as
//! [`RETRY_HORIZON`] from that change's first failure.
//!
//! Past the horizon the change is given up on: logged, and recorded as had,
//! so the config's later changes — the task's terminal one above all — are
//! still sent, each with a horizon of its own. The horizon's clock is this
//! process's: a server restarted while a webhook is down starts it afresh
//! for the change it re-delivers, so a webhook is tried for at least the
//! horizon, never less. A delivery that lands but cannot be recorded is
//! retried on the same backoff, never at once: sending it again is the
//! at-least-once promise, sending it in a loop is not.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use a2a::{
    A2AError, DeleteTaskPushNotificationConfigRequest, GetTaskPushNotificationConfigRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    StreamResponse, TaskPushNotificationConfig,
};
use basis_tasks::POLL;
use tokio::{sync::Notify, time::Instant};

use super::A2aService;
use crate::{
    config::Principal,
    index::Record,
    project::internal,
    push::{MAX_CONFIGS, PushStore, deliver, guard},
};

const FIRST_RETRY: Duration = Duration::from_millis(500);
const LAST_RETRY: Duration = Duration::from_secs(5 * 60);
/// How long one change is retried for before it is given up on: a day, long
/// enough to outlast a webhook's deploy, an outage, or a night.
pub(crate) const RETRY_HORIZON: Duration = Duration::from_secs(24 * 60 * 60);

/// Where one config's failing delivery stands.
#[derive(Debug, Clone, Copy)]
struct Retry {
    attempts: u32,
    /// When the change it is trying first failed.
    since: Instant,
    /// When to try again.
    at: Instant,
}

/// What to do after a delivery that has failed `attempts` times, for
/// `failing_for` so far.
#[derive(Debug, PartialEq, Eq)]
enum AfterFailure {
    RetryIn(Duration),
    GiveUp,
}

fn after_failure(attempts: u32, failing_for: Duration) -> AfterFailure {
    if failing_for >= RETRY_HORIZON {
        AfterFailure::GiveUp
    } else {
        AfterFailure::RetryIn(backoff(attempts))
    }
}

/// The push half of a service, present only when the operator turned push
/// notifications on.
pub(crate) struct Push {
    store: PushStore,
    allow_private: bool,
    /// Tasks with a monitor running.
    monitored: Mutex<Vec<String>>,
    /// Tasks with a deliverer running, each with the bell that wakes it.
    delivering: Mutex<HashMap<String, Arc<Notify>>>,
}

impl Push {
    pub(crate) fn new(store: PushStore, allow_private: bool) -> Self {
        Self {
            store,
            allow_private,
            monitored: Mutex::new(Vec::new()),
            delivering: Mutex::new(HashMap::new()),
        }
    }

    fn delivering(&self) -> MutexGuard<'_, HashMap<String, Arc<Notify>>> {
        self.delivering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn monitored(&self) -> MutexGuard<'_, Vec<String>> {
        self.monitored
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl A2aService {
    fn push(&self) -> Result<&Push, A2AError> {
        self.inner
            .push
            .as_ref()
            .ok_or_else(A2AError::push_notification_not_supported)
    }

    /// Checks a webhook before anything is stored or spawned for it.
    pub(super) async fn check_webhook(
        &self,
        config: &TaskPushNotificationConfig,
    ) -> Result<(), A2AError> {
        let push = self.push()?;
        guard::check(&config.url, push.allow_private)
            .await
            .map(|_| ())
            .map_err(|refused| A2AError::invalid_params(refused.to_string()))
    }

    /// `CreateTaskPushNotificationConfig`: the webhook is checked, the
    /// config stored beside the task, and the task watched from now on.
    pub async fn create_push_config(
        &self,
        caller: &Principal,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.push()?;
        let record = self.record(caller, &config.task_id)?;
        self.check_webhook(&config).await?;
        self.store_push_config(&record, config).await
    }

    /// Stores `config` for `record`'s task, already checked, and watches the
    /// task for changes.
    pub(super) async fn store_push_config(
        &self,
        record: &Record,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let push = self.push()?;
        let config = TaskPushNotificationConfig {
            id: Some(
                config
                    .id
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()),
            ),
            task_id: record.id.clone(),
            tenant: None,
            ..config
        };
        let (status, _) = self.status_now(record).await?;
        let stored = push
            .store
            .upsert(&record.id, config, Some(status))
            .map_err(internal)?
            .ok_or_else(|| {
                A2AError::invalid_params(format!(
                    "a task carries at most {MAX_CONFIGS} push configs; delete one first"
                ))
            })?;
        self.monitor(record);
        Ok(redacted(stored))
    }

    /// `GetTaskPushNotificationConfig`.
    pub fn get_push_config(
        &self,
        caller: &Principal,
        request: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let push = self.push()?;
        let record = self.record(caller, &request.task_id)?;
        push.store
            .get(&record.id, &request.id)
            .map(redacted)
            .ok_or_else(|| no_such_config(&request.id))
    }

    /// `ListTaskPushNotificationConfigs`: a task holds at most eight, so one
    /// page always holds them all and no page token is ever issued.
    pub fn list_push_configs(
        &self,
        caller: &Principal,
        request: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let push = self.push()?;
        let record = self.record(caller, &request.task_id)?;
        Ok(ListTaskPushNotificationConfigsResponse {
            configs: push
                .store
                .list(&record.id)
                .into_iter()
                .map(redacted)
                .collect(),
            next_page_token: None,
        })
    }

    /// `DeleteTaskPushNotificationConfig`.
    pub fn delete_push_config(
        &self,
        caller: &Principal,
        request: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let push = self.push()?;
        let record = self.record(caller, &request.task_id)?;
        if push
            .store
            .delete(&record.id, &request.id)
            .map_err(internal)?
        {
            Ok(())
        } else {
            Err(no_such_config(&request.id))
        }
    }

    /// Picks up every task with configs after a restart: watches each for
    /// the changes it missed, and sends what was not delivered.
    pub(super) fn resume_push(&self) {
        let Some(push) = &self.inner.push else {
            return;
        };
        for task in push.store.tasks_with_configs() {
            if let Some(record) = self.inner.index.get(&task) {
                self.monitor(&record);
            }
            self.wake_delivery(&task);
        }
    }

    /// Watches `record`'s task for status changes until it settles, unless
    /// something already is.
    fn monitor(&self, record: &Record) {
        let Some(push) = &self.inner.push else {
            return;
        };
        {
            let mut monitored = push.monitored();
            if monitored.contains(&record.id) {
                return;
            }
            monitored.push(record.id.clone());
        }
        let service = self.clone();
        let record = record.clone();
        tokio::spawn(async move {
            loop {
                let settled = service.observe(&record).await;
                if settled {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
            if let Some(push) = &service.inner.push {
                push.monitored().retain(|task| *task != record.id);
            }
        });
    }

    /// Records `record`'s current status if it changed; `true` once the task
    /// has settled and there is nothing left to watch.
    async fn observe(&self, record: &Record) -> bool {
        let Some(push) = &self.inner.push else {
            return true;
        };
        let (status, mut update) = match self.status_now(record).await {
            Ok(now) => now,
            Err(_) => return true,
        };
        let settled = update.status.state.is_terminal();
        // When this server saw the change, which is closer to when it
        // happened than when the task was accepted.
        update.status.timestamp = crate::project::timestamp(basis_tasks::now_ms());
        match push
            .store
            .record(&record.id, status, StreamResponse::StatusUpdate(update))
        {
            Ok(true) => self.wake_delivery(&record.id),
            Ok(false) => {}
            Err(error) => eprintln!(
                "basis-a2a: record a status change of task {}: {error}",
                record.id
            ),
        }
        settled
    }

    /// Makes sure `task`'s undelivered changes are being sent.
    fn wake_delivery(&self, task: &str) {
        let Some(push) = &self.inner.push else {
            return;
        };
        let mut delivering = push.delivering();
        if let Some(bell) = delivering.get(task) {
            bell.notify_one();
            return;
        }
        let bell = Arc::new(Notify::new());
        delivering.insert(task.to_string(), Arc::clone(&bell));
        tokio::spawn(self.clone().deliver_all(task.to_string(), bell));
    }

    async fn deliver_all(self, task: String, bell: Arc<Notify>) {
        let Some(push) = &self.inner.push else {
            return;
        };
        let mut retries: HashMap<String, Retry> = HashMap::new();
        loop {
            let pending = push.store.pending(&task);
            if pending.is_empty() {
                // Checked again under the lock a new change's bell is rung
                // under, so a change recorded between the two checks is
                // either seen here or finds no deliverer and starts one.
                let mut delivering = push.delivering();
                if push.store.pending(&task).is_empty() {
                    delivering.remove(&task);
                    return;
                }
                continue;
            }
            let mut next_wake: Option<Instant> = None;
            for (config, change) in pending {
                let id = config.id.clone().unwrap_or_default();
                if let Some(retry) = retries.get(&id)
                    && retry.at > Instant::now()
                {
                    next_wake = Some(next_wake.map_or(retry.at, |wake| wake.min(retry.at)));
                    continue;
                }
                let outcome = deliver::deliver(&config, &change.update, push.allow_private).await;
                let now = Instant::now();
                let previous = retries.get(&id).copied();
                let attempts = previous.map_or(0, |retry| retry.attempts) + 1;
                let since = previous.map_or(now, |retry| retry.since);
                let had = match outcome {
                    Ok(()) => true,
                    Err(error) => {
                        eprintln!(
                            "basis-a2a: push config {id} of task {task}, attempt {attempts}: {error}"
                        );
                        match after_failure(attempts, now.duration_since(since)) {
                            AfterFailure::GiveUp => {
                                eprintln!(
                                    "basis-a2a: push config {id} of task {task}: giving up on \
                                     change {} after {}h of failures",
                                    change.seq,
                                    RETRY_HORIZON.as_secs() / 3600
                                );
                                true
                            }
                            AfterFailure::RetryIn(wait) => {
                                retries.insert(
                                    id.clone(),
                                    Retry {
                                        attempts,
                                        since,
                                        at: now + wait,
                                    },
                                );
                                next_wake =
                                    Some(next_wake.map_or(now + wait, |wake| wake.min(now + wait)));
                                false
                            }
                        }
                    }
                };
                if !had {
                    continue;
                }
                match push.store.delivered(&task, &id, change.seq) {
                    Ok(()) => {
                        retries.remove(&id);
                    }
                    // Not recorded, so still pending: wait before sending it
                    // again rather than re-POSTing in a tight loop.
                    Err(error) => {
                        eprintln!("basis-a2a: record a push delivery for task {task}: {error}");
                        let at = now + backoff(attempts);
                        retries.insert(
                            id.clone(),
                            Retry {
                                attempts,
                                since,
                                at,
                            },
                        );
                        next_wake = Some(next_wake.map_or(at, |wake| wake.min(at)));
                    }
                }
            }
            if let Some(wake) = next_wake {
                tokio::select! {
                    () = tokio::time::sleep_until(wake) => {}
                    () = bell.notified() => {}
                }
            }
        }
    }
}

/// The wait before retry number `attempts`.
fn backoff(attempts: u32) -> Duration {
    FIRST_RETRY
        .saturating_mul(2_u32.saturating_pow(attempts.saturating_sub(1)))
        .min(LAST_RETRY)
}

/// A config as it is answered: everything the caller set, except that a
/// credential this server holds for it is never sent back out.
fn redacted(config: TaskPushNotificationConfig) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        authentication: config
            .authentication
            .map(|authentication| a2a::AuthenticationInfo {
                credentials: None,
                ..authentication
            }),
        ..config
    }
}

fn no_such_config(id: &str) -> A2AError {
    A2AError::invalid_params(format!("this task has no push config {id:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_its_bound() {
        assert_eq!(backoff(1), FIRST_RETRY);
        assert_eq!(backoff(2), FIRST_RETRY * 2);
        assert_eq!(backoff(3), FIRST_RETRY * 4);
        assert_eq!(backoff(30), LAST_RETRY);
        assert_eq!(backoff(u32::MAX), LAST_RETRY);
    }

    #[test]
    fn a_failing_change_is_retried_for_the_horizon_whatever_the_attempt_count() {
        assert_eq!(
            after_failure(1, Duration::ZERO),
            AfterFailure::RetryIn(FIRST_RETRY)
        );
        assert_eq!(
            after_failure(10_000, RETRY_HORIZON - Duration::from_secs(1)),
            AfterFailure::RetryIn(LAST_RETRY),
            "many attempts are no reason to stop"
        );
        assert_eq!(after_failure(3, RETRY_HORIZON), AfterFailure::GiveUp);
    }

    #[test]
    fn a_held_credential_is_never_answered() {
        let config = TaskPushNotificationConfig {
            url: "https://example.com/hook".into(),
            id: Some("c".into()),
            task_id: "t".into(),
            token: Some("caller-token".into()),
            authentication: Some(a2a::AuthenticationInfo {
                scheme: "Bearer".into(),
                credentials: Some("secret".into()),
            }),
            tenant: None,
        };
        let answered = redacted(config);
        let authentication = answered.authentication.unwrap();
        assert_eq!(authentication.scheme, "Bearer");
        assert!(authentication.credentials.is_none());
        assert_eq!(answered.token.as_deref(), Some("caller-token"));
    }
}
