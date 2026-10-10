//! Accepting a message: validating it, deciding which conversation it
//! continues, and spawning the task that answers it.

use a2a::{A2AError, Message, SendMessageRequest};
use basis_tasks::{Continuation, Hint, RunSpec, now_ms};

use super::{A2aService, parts};
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
        if configuration.task_push_notification_config.is_some() {
            return Err(A2AError::push_notification_not_supported());
        }
        if let Some(modes) = &configuration.accepted_output_modes
            && !modes.is_empty()
            && !modes
                .iter()
                .any(|mode| TEXT_MODES.contains(&mode.trim().to_ascii_lowercase().as_str()))
        {
            return Err(A2AError::content_type_not_supported());
        }
        let history_length = crate::project::history_length(configuration.history_length)?;
        let rendered = parts::render(&message)?;
        let (context_id, continuation) = self.conversation(caller, &message)?;

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
        self.drive(&record);
        Ok(Accepted {
            record,
            history_length,
            return_immediately: configuration.return_immediately.unwrap_or(false),
        })
    }

    /// Which conversation `message` belongs to: the one its `contextId`
    /// names, or a new one. A message naming a task is a reply to a task
    /// waiting for input, and no task this server runs ever waits for input
    /// (ADR-0028 §4), so it is refused with the reason.
    fn conversation(
        &self,
        caller: &Principal,
        message: &Message,
    ) -> Result<(String, Continuation), A2AError> {
        if let Some(task_id) = &message.task_id {
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
            return Err(A2AError::unsupported_operation(format!(
                "task {task_id} is not waiting for input; send a message with its contextId and \
                 no taskId to continue the conversation"
            )));
        }
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
