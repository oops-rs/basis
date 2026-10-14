//! mentra's A2A client as a [`Peer`] (ADR-0038 §3).
//!
//! The exchange — submit with `returnImmediately`, wait for the task to
//! settle within the agent's wait bound (following its stream when the card
//! offers one, polling otherwise, never resending), cancel when the run is
//! cancelled — is mentra's ([`mentra::a2a::exchange`]), and so is the framing
//! that marks the remote agent's words as its own. What this module adds is
//! the hops extension on every message (`docs/a2a/hops-v1.md`, ADR-0030 §9)
//! and the mapping of mentra's answers onto the contract `spawn` is written
//! against.

use std::{sync::Arc, time::SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use mentra::a2a::{
    A2aClient, A2aError as ClientError, A2aExchange, A2aExchangeError, A2aRequest, A2aRpcError,
    A2aSend, A2aStillWorking, A2aSubmitted, is_settled, remote_output, remote_reply,
    settle as settle_task, submit, types::TaskState,
};
use serde_json::{Value, json};

use super::HOPS_EXTENSION;
use crate::agents::{
    ExchangeBounds, Failure, Message, Outcome, Peer, PeerError, Request, Resolved, Started,
    StillWorking,
};

/// A remote agent reached over A2A.
pub struct A2aPeer {
    client: Arc<A2aClient>,
    /// The name the output is framed under — what the model calls the agent.
    name: String,
}

impl A2aPeer {
    /// Speaks to `client` as the agent the model calls `name`.
    pub fn new(client: Arc<A2aClient>, name: impl Into<String>) -> Self {
        Self {
            client,
            name: name.into(),
        }
    }

    /// The client underneath, for a resolver that keeps one per agent.
    pub fn client(&self) -> &Arc<A2aClient> {
        &self.client
    }
}

impl std::fmt::Debug for A2aPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aPeer")
            .field("name", &self.name)
            .field("endpoint", &self.client.endpoint().as_str())
            .finish()
    }
}

impl Resolved {
    /// A remote agent reached over A2A through `client`, called `name`: the
    /// peer is an [`A2aPeer`], the endpoint the client's URL, and the id the
    /// name until [`with_id`](Self::with_id) says otherwise.
    pub fn over_a2a(client: Arc<A2aClient>, name: impl Into<String>) -> Self {
        let name = name.into();
        let endpoint = client.endpoint().to_string();
        Self::new(Arc::new(A2aPeer::new(client, name.clone())), name, endpoint)
    }
}

#[async_trait]
impl Peer for A2aPeer {
    async fn exchange(
        &self,
        request: Request,
        bounds: &ExchangeBounds<'_>,
    ) -> Result<Outcome, PeerError> {
        let request = match request {
            Request::Wait { task_id } => A2aRequest::resume(task_id),
            Request::Send(send) => message(send).into(),
            // `Request` is non-exhaustive for callers outside the crate; a
            // variant this adapter does not know cannot be sent.
            #[allow(unreachable_patterns)]
            _ => return Err(PeerError::Unrecognized),
        };
        let max = self.max_output_bytes();
        let outcome = match submit(&self.client, request).await {
            Ok(A2aSubmitted::Task(task)) if !is_settled(&task.status.state) => {
                (bounds.started)(Started {
                    task_id: task.id.clone(),
                    context_id: Some(task.context_id.clone()),
                });
                settle_task(&self.client, task, bounds.options, bounds.progress).await
            }
            Ok(A2aSubmitted::Task(task)) => Ok(A2aExchange::Settled(task)),
            Ok(A2aSubmitted::Reply(message)) => Ok(A2aExchange::Reply(message)),
            Ok(_) => return Err(PeerError::Unrecognized),
            Err(error) => Err(error),
        };
        map(&self.name, outcome, max)
    }

    fn max_output_bytes(&self) -> usize {
        self.client.limits().max_output_bytes
    }
}

/// mentra's answer, as the contract says it.
fn map(
    name: &str,
    outcome: Result<A2aExchange, A2aExchangeError>,
    max: usize,
) -> Result<Outcome, PeerError> {
    match outcome {
        Ok(A2aExchange::Reply(message)) => Ok(Outcome::Replied {
            text: remote_reply(name, &message, max),
            context_id: message.context_id,
        }),
        Ok(A2aExchange::Settled(task)) => {
            let text = remote_output(name, &task, max);
            let task_id = task.id.clone();
            let context_id = Some(task.context_id.clone());
            Ok(match task.status.state {
                TaskState::InputRequired => Outcome::NeedsInput {
                    task_id,
                    context_id,
                    text,
                },
                TaskState::AuthRequired => Outcome::NeedsAuthorization {
                    task_id,
                    context_id,
                    text,
                },
                TaskState::Failed | TaskState::Rejected | TaskState::Canceled => Outcome::Failed {
                    task_id,
                    context_id,
                    text,
                    how: match task.status.state {
                        TaskState::Rejected => Failure::Rejected,
                        TaskState::Canceled => Failure::Canceled,
                        _ => Failure::Failed,
                    },
                },
                _ => Outcome::Finished {
                    task_id,
                    context_id,
                    text,
                },
            })
        }
        Ok(A2aExchange::StillWorking { task, reason }) => Ok(Outcome::Working {
            text: remote_output(name, &task, max),
            task_id: task.id,
            context_id: Some(task.context_id),
            why: match reason {
                A2aStillWorking::WaitBound(waited) => StillWorking::WaitBound(waited),
                _ => StillWorking::RunStopping,
            },
        }),
        Ok(_) => Err(PeerError::Unrecognized),
        Err(error) if task_not_found(&error) => Err(PeerError::UnknownTask),
        // mentra's error carries no server text and no header value.
        Err(A2aExchangeError::Send(error)) => Err(PeerError::Send(error.to_string())),
        Err(A2aExchangeError::Poll { task_id, error }) => Err(PeerError::Check {
            task_id,
            error: error.to_string(),
        }),
        Err(A2aExchangeError::Abandoned {
            task_id, canceled, ..
        }) => Err(PeerError::Abandoned {
            task_id,
            canceled: canceled.map_err(|error| error.to_string()),
        }),
        Err(error) => Err(PeerError::Send(error.to_string())),
    }
}

/// Whether the agent answered that it does not know the task asked about.
fn task_not_found(error: &A2aExchangeError) -> bool {
    let not_found = |error: &ClientError| {
        matches!(
            error,
            ClientError::Rpc {
                error: A2aRpcError::TaskNotFound,
                ..
            }
        )
    };
    match error {
        A2aExchangeError::Send(error) | A2aExchangeError::Poll { error, .. } => not_found(error),
        _ => false,
    }
}

/// The A2A message for `send`: continuing its context, answering its task,
/// and saying which hop it is.
fn message(send: Message) -> A2aSend {
    let mut message = A2aSend::new(send.text)
        .with_extension(HOPS_EXTENSION)
        .with_metadata(HOPS_EXTENSION, hop_metadata(send.hop, send.deadline));
    if let Some(task_id) = send.answering {
        message = message.with_task_id(task_id);
    }
    if let Some(context_id) = send.context_id {
        message = message.with_context_id(context_id);
    }
    message
}

/// The object the hops extension puts under its URI in a message's metadata
/// (`docs/a2a/hops-v1.md`): this message's hop, and when this side stops
/// waiting, which the run's options already hold as the earlier of its own
/// and any it inherited — so a deadline only shrinks across a hop, which is
/// the extension's rule.
fn hop_metadata(hop: u32, deadline: Option<SystemTime>) -> Value {
    let mut object = json!({ "hop": hop });
    if let Some(deadline) = deadline {
        object["deadline"] = Value::String(
            DateTime::<Utc>::from(deadline).to_rfc3339_opts(SecondsFormat::Millis, true),
        );
    }
    object
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mentra::a2a::types::{Artifact, Part, Task, TaskStatus};

    use super::*;

    const NAME: &str = "reviewer";
    const MAX: usize = 64 * 1024;

    fn task(state: TaskState, answer: Option<&str>) -> Task {
        Task {
            id: "task-1".to_string(),
            context_id: "context-1".to_string(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: answer.map(|text| {
                vec![Artifact {
                    artifact_id: "result".to_string(),
                    name: None,
                    description: None,
                    parts: vec![Part::text(text.to_string())],
                    metadata: None,
                    extensions: None,
                }]
            }),
            history: None,
            metadata: None,
        }
    }

    fn not_found(method: &'static str) -> ClientError {
        ClientError::Rpc {
            method,
            error: A2aRpcError::TaskNotFound,
        }
    }

    #[test]
    fn every_message_says_which_hop_it_is_and_when_this_side_stops_waiting() {
        let deadline = std::time::UNIX_EPOCH + Duration::from_millis(1_791_000_000_123);
        let message = message(Message::new("look").at_hop(3, Some(deadline)));

        assert_eq!(message.extensions, vec![HOPS_EXTENSION.to_string()]);
        assert_eq!(
            message.metadata.get(HOPS_EXTENSION),
            Some(&json!({"hop": 3, "deadline": "2026-10-03T04:00:00.123Z"}))
        );
    }

    #[test]
    fn a_top_level_caller_sends_hop_one_and_no_deadline_it_does_not_have() {
        let message = message(Message::new("look"));
        assert_eq!(
            message.metadata.get(HOPS_EXTENSION),
            Some(&json!({"hop": 1}))
        );
        assert_eq!(message.task_id, None);
        assert_eq!(message.context_id, None);
    }

    #[test]
    fn a_message_continues_its_context_and_answers_its_task() {
        let message = message(
            Message::new("yes")
                .in_context(Some("context-1".to_string()))
                .answering(Some("task-1".to_string())),
        );
        assert_eq!(message.context_id.as_deref(), Some("context-1"));
        assert_eq!(message.task_id.as_deref(), Some("task-1"));
    }

    #[test]
    fn each_task_state_is_one_outcome() {
        let finished = map(
            NAME,
            Ok(A2aExchange::Settled(task(
                TaskState::Completed,
                Some("looks good"),
            ))),
            MAX,
        )
        .expect("finished");
        let Outcome::Finished {
            task_id,
            context_id,
            text,
        } = finished
        else {
            panic!("{finished:?}");
        };
        assert_eq!(task_id, "task-1");
        assert_eq!(context_id.as_deref(), Some("context-1"));
        assert!(text.contains("looks good"), "{text}");

        assert!(matches!(
            map(
                NAME,
                Ok(A2aExchange::Settled(task(TaskState::InputRequired, None))),
                MAX
            ),
            Ok(Outcome::NeedsInput { .. })
        ));
        assert!(matches!(
            map(
                NAME,
                Ok(A2aExchange::Settled(task(TaskState::AuthRequired, None))),
                MAX
            ),
            Ok(Outcome::NeedsAuthorization { .. })
        ));
        for (state, how) in [
            (TaskState::Failed, Failure::Failed),
            (TaskState::Rejected, Failure::Rejected),
            (TaskState::Canceled, Failure::Canceled),
        ] {
            assert!(matches!(
                map(NAME, Ok(A2aExchange::Settled(task(state, None))), MAX),
                Ok(Outcome::Failed { how: got, .. }) if got == how
            ));
        }
        assert!(matches!(
            map(
                NAME,
                Ok(A2aExchange::StillWorking {
                    task: task(TaskState::Working, None),
                    reason: A2aStillWorking::WaitBound(Duration::from_secs(300)),
                }),
                MAX
            ),
            Ok(Outcome::Working {
                why: StillWorking::WaitBound(waited),
                ..
            }) if waited == Duration::from_secs(300)
        ));
    }

    #[test]
    fn a_task_the_agent_does_not_know_is_unknown_whichever_call_said_so() {
        assert_eq!(
            map(NAME, Err(A2aExchangeError::Send(not_found("GetTask"))), MAX),
            Err(PeerError::UnknownTask)
        );
        assert_eq!(
            map(
                NAME,
                Err(A2aExchangeError::Poll {
                    task_id: "task-1".to_string(),
                    error: not_found("GetTask"),
                }),
                MAX
            ),
            Err(PeerError::UnknownTask)
        );
    }

    #[test]
    fn other_failures_keep_their_kind_and_say_no_more_than_mentra_does() {
        assert!(matches!(
            map(
                NAME,
                Err(A2aExchangeError::Send(ClientError::Transport("connection reset"))),
                MAX
            ),
            Err(PeerError::Send(text)) if text.contains("connection reset")
        ));
        assert!(matches!(
            map(
                NAME,
                Err(A2aExchangeError::Poll {
                    task_id: "task-1".to_string(),
                    error: ClientError::Connect,
                }),
                MAX
            ),
            Err(PeerError::Check { task_id, .. }) if task_id == "task-1"
        ));
    }
}
