//! Accepting a message: validating it, deciding which conversation it
//! continues, and spawning the task that answers it.

use a2a::{A2AError, Message, SendMessageRequest};
use basis_tasks::{Continuation, Hint, RunSpec, now_ms};

use super::{
    A2aService,
    ask::{self, Unanswered},
    parts,
};
use crate::{card::TEXT, config::Principal, index::Record, project::internal};

/// The output modes a caller may accept for this server to be able to answer.
const TEXT_MODES: [&str; 4] = [TEXT, "text/*", "*/*", "text"];

/// What [`A2aService::accept`] settled on.
pub(crate) struct Accepted {
    pub(crate) record: Record,
    pub(crate) history_length: Option<usize>,
    pub(crate) return_immediately: bool,
}

impl A2aService {
    /// Validates `request`, spawns the task that answers it, records it, and
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
        let (context_id, continuation) = self.conversation(caller, &message)?;

        // Checked before anything is spawned, so a refused webhook leaves no
        // task behind it.
        let webhook = configuration.task_push_notification_config;
        if let Some(webhook) = &webhook {
            self.check_webhook(webhook).await?;
        }

        let spec = self.run_spec(rendered.prompt.clone(), continuation);
        let tasks = self.tasks().clone();
        let handle = tokio::task::spawn_blocking(move || tasks.spawn(spec))
            .await
            .map_err(internal)?
            .map_err(|error| match error.hint() {
                // `basis-tasks` keeps one executor per conversation; a second
                // message into a context whose last task is still running is
                // the caller's to wait out, not a fault of ours.
                Some(Hint::Wait(_)) => A2AError::unsupported_operation(
                    "a task in this context is still working; wait for it to settle, then send again",
                ),
                _ => internal(error),
            })?;

        let record = Record::new(
            &handle,
            caller,
            context_id,
            message.message_id,
            rendered.prompt,
            now_ms(),
        )
        .with_parts((!rendered.only_text).then_some(message.parts));
        self.inner.index.insert(record.clone()).map_err(internal)?;
        // Stored before the task is driven, so its first change of status is
        // already one the webhook hears about. A store that fails still
        // leaves the task driven: it was accepted, and `ListTasks` finds it.
        let stored = webhook.map(|webhook| self.store_push_config(&record, webhook));
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
            })?;
        Ok(record)
    }

    /// Which conversation `message` belongs to: the one its `contextId`
    /// names, or a new one.
    fn conversation(
        &self,
        caller: &Principal,
        message: &Message,
    ) -> Result<(String, Continuation), A2AError> {
        match &message.context_id {
            None => Ok((a2a::new_context_id(), Continuation::New)),
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
                Ok((context_id.to_string(), Continuation::Named(latest.handle())))
            }
        }
    }

    fn run_spec(&self, prompt: String, continuation: Continuation) -> RunSpec {
        let run = &self.inner.run;
        let mut spec = RunSpec::new(prompt)
            .with_approve(self.inner.approval.into())
            .detached()
            .continuing(continuation);
        if !self.inner.shell {
            spec = spec.without_shell();
        }
        if let Some(provider) = &run.provider {
            spec = spec.with_provider(provider.clone());
        }
        if let Some(base_url) = &run.base_url {
            spec = spec.with_base_url(base_url.clone());
        }
        if let Some(model) = &run.model {
            spec = spec.with_model(model.clone());
        }
        if let Some(effort) = run.effort {
            spec = spec.with_effort(effort);
        }
        if let Some(deadline) = run.deadline {
            spec = spec.with_deadline(deadline);
        }
        if let Some(tool_budget) = run.tool_budget {
            spec = spec.with_tool_budget(tool_budget);
        }
        if let Some(token_budget) = run.token_budget {
            spec = spec.with_token_budget(token_budget);
        }
        if let Some(system_prompt) = &run.system_prompt {
            spec = spec.with_system_prompt(system_prompt.clone());
        }
        spec
    }
}
