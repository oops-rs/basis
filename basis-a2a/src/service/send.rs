//! Accepting a message: validating it, deciding which conversation it
//! continues, and starting the task that answers it — a `basis-tasks` spawn
//! ([`spawn`]) or a host's own ([`hosted`]).

mod hosted;
mod spawn;

use a2a::{A2AError, Message, SendMessageRequest};
use basis_tasks::now_ms;

use super::{
    A2aService, Backend,
    ask::{self, Unanswered},
    hops, host_id_of, parts,
};
use crate::{
    card::TEXT,
    config::Principal,
    host::HostedStart,
    index::{Key, Record},
    project::internal,
};

/// The output modes a caller may accept for this server to be able to answer.
const TEXT_MODES: [&str; 4] = [TEXT, "text/*", "*/*", "text"];

/// What [`A2aService::accept`] settled on.
pub(crate) struct Accepted {
    pub(crate) record: Record,
    pub(crate) history_length: Option<usize>,
    pub(crate) return_immediately: bool,
}

/// A task a backend has started: the A2A id it is served under, the key
/// the backend knows it by, and the instant it gives up at.
struct Started {
    id: String,
    key: Key,
    deadline_ms: u64,
}

impl A2aService {
    /// Validates `request`, starts the task that answers it, records it, and
    /// starts driving it.
    pub(super) async fn accept(
        &self,
        caller: &Principal,
        request: SendMessageRequest,
    ) -> Result<Accepted, A2AError> {
        let SendMessageRequest {
            message,
            configuration,
            ..
        } = request;
        let configuration = configuration.unwrap_or(a2a::SendMessageConfiguration {
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            return_immediately: None,
        });
        if let Some(modes) = &configuration.accepted_output_modes
            && !modes.is_empty()
            && !modes
                .iter()
                .any(|mode| TEXT_MODES.contains(&mode.trim().to_ascii_lowercase().as_str()))
        {
            return Err(A2AError::content_type_not_supported());
        }
        let history_length = crate::project::history_length(configuration.history_length)?;
        let return_immediately = configuration.return_immediately.unwrap_or(false);
        if let Some(task_id) = &message.task_id {
            // A message naming a task is a reply to it: here, only ever the
            // answer to a question it has open (ADR-0030 §7).
            if configuration.task_push_notification_config.is_some() {
                return Err(A2AError::invalid_params(
                    "an answer to a question carries no push config; create one on the task",
                ));
            }
            let record = self.answer(caller, task_id, &message)?;
            return Ok(Accepted {
                record,
                history_length,
                return_immediately,
            });
        }
        let rendered = parts::render(&message)?;
        let inbound = hops::inbound(&message)?;
        hops::check_cap(inbound, self.inner.hop_cap)?;
        let (context_id, latest) = self.conversation(caller, &message)?;

        // Checked before anything is spawned, so a refused webhook leaves no
        // task behind it.
        let webhook = configuration.task_push_notification_config;
        if let Some(webhook) = &webhook {
            self.check_webhook(webhook).await?;
        }

        // Held from the quota check to the record's insertion, so two sends
        // at once cannot both take the last place.
        let _admission = self.inner.admission.lock().await;
        let live = self.live_tasks(caller).await;
        if live >= self.inner.live_task_quota {
            return Err(A2AError::unsupported_operation(format!(
                "principal {} has {live} tasks still running, the most this agent runs at once \
                 for one caller; wait for one to settle, or cancel one",
                caller.name()
            )));
        }
        let accepted_ms = now_ms();
        let started = match self.backend() {
            Backend::Tasks(backend) => {
                spawn::spawn(
                    backend,
                    &rendered.prompt,
                    latest.as_ref(),
                    inbound,
                    accepted_ms,
                )
                .await?
            }
            Backend::Hosted(host) => {
                let continues = latest
                    .as_ref()
                    .map(|latest| host_id_of(latest).map(str::to_string))
                    .transpose()?;
                let start = HostedStart {
                    principal: caller.name().to_string(),
                    context_id: context_id.clone(),
                    continues,
                    prompt: rendered.prompt.clone(),
                    parts: message.parts.clone(),
                    hop: inbound.hop,
                    deadline_ms: hosted::deadline_ms(inbound, accepted_ms)?,
                };
                hosted::start(host.as_ref(), start).await?
            }
        };
        let record = Record::keyed(
            started.id,
            started.key,
            caller,
            context_id,
            message.message_id,
            rendered.prompt,
            accepted_ms,
        )
        .with_parts((!rendered.only_text).then_some(message.parts))
        .with_hops(inbound.hop, started.deadline_ms);
        self.inner.index.insert(record.clone()).map_err(internal)?;
        // Stored before the task is driven, so its first change of status is
        // already one the webhook hears about. A store that fails still
        // leaves the task driven: it was accepted, and `ListTasks` finds it.
        let stored = match webhook {
            Some(webhook) => Some(self.store_push_config(&record, webhook).await),
            None => None,
        };
        self.drive(&record);
        if let Some(Err(error)) = stored {
            return Err(error);
        }
        Ok(Accepted {
            record,
            history_length,
            return_immediately,
        })
    }

    /// Answers the question `task_id` has open with `message`. A task with
    /// none open is refused as it always was: the reply belongs in a new
    /// message on its context. An answer naming another call, or giving no
    /// decision, is refused and the question stays open.
    fn answer(
        &self,
        caller: &Principal,
        task_id: &str,
        message: &Message,
    ) -> Result<Record, A2AError> {
        if message.role != a2a::Role::User || message.message_id.trim().is_empty() {
            return Err(A2AError::invalid_params(
                "a message to an agent has role ROLE_USER and a messageId",
            ));
        }
        let record = self.record(caller, task_id)?;
        if message
            .context_id
            .as_ref()
            .is_some_and(|context| *context != record.context_id)
        {
            return Err(A2AError::invalid_params(
                "contextId does not match the task's context",
            ));
        }
        let not_waiting = || {
            A2AError::unsupported_operation(format!(
                "task {task_id} is not waiting for input; send a message with its contextId and \
                 no taskId to continue the conversation"
            ))
        };
        let questions = self
            .questions(&record.id)
            .filter(|questions| questions.asked().is_some())
            .ok_or_else(not_waiting)?;
        let (call_id, approve) = ask::answer_of(message)?;
        questions
            .answer(&call_id, approve)
            .map_err(|unanswered| match unanswered {
                Unanswered::NotAsking => not_waiting(),
                Unanswered::Mismatch { open } => A2AError::invalid_params(format!(
                    "{call_id} is not the open question; it is {open}"
                )),
                Unanswered::Closed => A2AError::invalid_params(format!(
                    "the question {call_id} closed before this answer arrived, so the answer \
                     was not taken; the task carries on without it"
                )),
            })?;
        Ok(record)
    }

    /// Which conversation `message` belongs to: the one its `contextId`
    /// names — with the latest task in it, which a follow-up continues — or
    /// a new one.
    fn conversation(
        &self,
        caller: &Principal,
        message: &Message,
    ) -> Result<(String, Option<Record>), A2AError> {
        match &message.context_id {
            None => Ok((a2a::new_context_id(), None)),
            Some(context_id) => {
                // Another principal's context is as unknown as one never
                // issued: naming it continues nothing and reveals nothing.
                let latest = self
                    .inner
                    .index
                    .latest_in(caller, context_id)
                    .ok_or_else(|| {
                        A2AError::invalid_params(format!(
                            "contextId {context_id} is not one this server issued"
                        ))
                    })?;
                Ok((context_id.to_string(), Some(latest)))
            }
        }
    }

    /// How many of `caller`'s tasks have not settled — read off the backend
    /// too: a task its backend says has settled has, whether or not its
    /// driver has recorded so yet. A task whose standing cannot be read
    /// counts as live.
    async fn live_tasks(&self, caller: &Principal) -> usize {
        let unsettled = self.inner.index.unsettled_owned(caller);
        match self.backend() {
            Backend::Tasks(backend) => unsettled
                .iter()
                .filter(|record| {
                    !record
                        .handle()
                        .is_some_and(|handle| matches!(backend.tasks.terminal(handle), Ok(Some(_))))
                })
                .count(),
            Backend::Hosted(host) => {
                let mut live = 0;
                for record in &unsettled {
                    let settled = match record.host_id() {
                        Some(id) => {
                            matches!(host.standing(id).await, Ok(standing) if standing.is_terminal())
                        }
                        None => false,
                    };
                    if !settled {
                        live += 1;
                    }
                }
                live
            }
        }
    }
}
