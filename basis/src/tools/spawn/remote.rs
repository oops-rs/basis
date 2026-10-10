//! Handing a task to a remote agent, once the answer is in (ADR-0029).
//!
//! The exchange — submit with `returnImmediately`, poll to a settled state
//! within the agent's wait bound, cancel when the run is cancelled — is
//! mentra's ([`mentra::a2a::exchange`]), and so is the framing that marks the
//! remote agent's words as its own. What is basis's is everything the model
//! would otherwise have had to carry: which conversation a task belongs to,
//! and which task is still outstanding (ADR-0029 §5). So every guidance line
//! here is written in `spawn`'s grammar, and none of them mentions an id.

use mentra::a2a::{
    A2aExchange, A2aExchangeError, A2aRequest, A2aStillWorking, A2aSubmitted, is_settled,
    remote_output, remote_reply, settle as settle_task, submit, types::TaskState,
};
use mentra::tool::{ParallelToolContext, ToolContext, ToolResult};

use crate::a2a::remote::{Conversation, Outstanding, RemoteAgents, Waiting};

/// Hands `body` to the remote agent `name`, or — with an empty body — waits
/// again on its outstanding task, and reports what came back.
///
/// `remembered` says whether the calling agent's record is kept on disk: true
/// for an agent a workspace minted or resumed, false for a delegated child
/// (`crate::a2a::remote` has the rule).
pub(super) async fn delegate(
    ctx: ToolContext<'_>,
    agents: &RemoteAgents,
    name: &str,
    body: &str,
    remembered: bool,
) -> ToolResult {
    // Cloned out of the snapshot and held to the end of this call: a reload
    // that swaps the set meanwhile changes what the *next* `@<agent>` reaches,
    // never this exchange (ADR-0031 §6).
    let client = agents
        .snapshot()
        .get(name)
        .and_then(|agent| agent.client())
        .cloned()
        .ok_or_else(|| format!("the remote agent `{name}` is not reachable from this workspace"))?;
    let caller = ctx.agent_id.clone();
    let mut conversation = agents.conversation(&caller, name, remembered);
    let request = request(name, body, &conversation)?;

    // The run's own bounds — cancellation, stop, deadline — taken before the
    // context is handed over for progress, which is the one thing the
    // exchange needs from it while it waits.
    let options = ctx.child_run_options();
    let ctx: ParallelToolContext = ctx.into();
    let progress = |text: String| ctx.emit_progress(text);
    let save = |next: Conversation| {
        // Reported, not raised: the exchange it describes happened, and a
        // failed tool call would invite the model to do the work again.
        if let Err(error) = agents.record(&caller, name, next, remembered) {
            ctx.emit_progress(format!(
                "the conversation with `{name}` was not saved: {error}"
            ));
        }
    };

    let outcome = match submit(&client, request).await {
        Ok(A2aSubmitted::Task(task)) if !is_settled(&task.status.state) => {
            // Recorded before waiting, which is the whole point of submitting
            // separately: a process that dies while this waits comes back
            // knowing the task exists, and `@<agent>` alone picks it up
            // instead of the model handing the work off a second time.
            conversation = Conversation {
                context_id: Some(task.context_id.clone()),
                outstanding: Some(Outstanding {
                    task_id: task.id.clone(),
                    waiting_for: Waiting::Agent,
                }),
            };
            save(conversation.clone());
            settle_task(&client, task, &options, &progress).await
        }
        Ok(A2aSubmitted::Task(task)) => Ok(A2aExchange::Settled(task)),
        Ok(A2aSubmitted::Reply(message)) => Ok(A2aExchange::Reply(message)),
        Ok(_) => {
            return Err(format!(
                "Remote agent `{name}` answered in a way spawn does not understand."
            ));
        }
        Err(error) => Err(error),
    };

    let max_output_bytes = client.limits().max_output_bytes;
    let (next, result) = settle(name, conversation, outcome, max_output_bytes);
    save(next);
    result
}

/// What this call sends, given where the conversation stands — or why it
/// sends nothing.
fn request(name: &str, body: &str, conversation: &Conversation) -> Result<A2aRequest, String> {
    match (&conversation.outstanding, body.is_empty()) {
        (Some(outstanding), true) => Ok(A2aRequest::Resume {
            task_id: outstanding.task_id.clone(),
        }),
        (None, true) => Err(format!(
            "`{name}` has nothing outstanding to wait on; write `@{name} <task>` to hand it one"
        )),
        (
            Some(Outstanding {
                waiting_for: Waiting::Agent,
                ..
            }),
            false,
        ) => Err(format!(
            "`{name}` is still working on the last task you handed it, and takes one task at a \
             time; write `@{name}` alone to keep waiting for that one first"
        )),
        (Some(outstanding), false) => Ok(A2aRequest::Send {
            text: body.to_string(),
            task_id: Some(outstanding.task_id.clone()),
            context_id: conversation.context_id.clone(),
        }),
        (None, false) => Ok(A2aRequest::Send {
            text: body.to_string(),
            task_id: None,
            context_id: conversation.context_id.clone(),
        }),
    }
}

/// Where the conversation stands after this exchange, and what the model
/// reads about it.
fn settle(
    name: &str,
    before: Conversation,
    outcome: Result<A2aExchange, A2aExchangeError>,
    max_output_bytes: usize,
) -> (Conversation, ToolResult) {
    match outcome {
        Ok(A2aExchange::Reply(message)) => (
            Conversation {
                context_id: message.context_id.clone().or(before.context_id),
                outstanding: None,
            },
            Ok(said(
                format!("Remote agent `{name}` replied:"),
                remote_reply(name, &message, max_output_bytes),
            )),
        ),
        Ok(A2aExchange::Settled(task)) => {
            let output = remote_output(name, &task, max_output_bytes);
            let context_id = Some(task.context_id.clone());
            match task.status.state {
                TaskState::InputRequired => (
                    Conversation {
                        context_id,
                        outstanding: Some(Outstanding {
                            task_id: task.id.clone(),
                            waiting_for: Waiting::Input,
                        }),
                    },
                    Ok(said(
                        format!(
                            "Remote agent `{name}` needs more before it can finish. Answer with \
                             `@{name} <your answer>`."
                        ),
                        output,
                    )),
                ),
                TaskState::AuthRequired => (
                    Conversation {
                        context_id,
                        outstanding: None,
                    },
                    Err(said(
                        format!(
                            "Remote agent `{name}` needs authorization this conversation cannot \
                             give it, so the task stopped there."
                        ),
                        output,
                    )),
                ),
                TaskState::Failed | TaskState::Rejected | TaskState::Canceled => (
                    Conversation {
                        context_id,
                        outstanding: None,
                    },
                    Err(said(
                        format!(
                            "Remote agent `{name}` {}.",
                            match task.status.state {
                                TaskState::Rejected => "rejected the task",
                                TaskState::Canceled => "reports the task was canceled",
                                _ => "reports the task failed",
                            }
                        ),
                        output,
                    )),
                ),
                _ => (
                    Conversation {
                        context_id,
                        outstanding: None,
                    },
                    Ok(said(format!("Remote agent `{name}` replied:"), output)),
                ),
            }
        }
        Ok(A2aExchange::StillWorking { task, reason }) => {
            let why = match reason {
                A2aStillWorking::WaitBound(waited) => {
                    format!("did not finish within {}s", waited.as_secs().max(1))
                }
                A2aStillWorking::RunStopping => {
                    "was still working when this run began to stop".to_string()
                }
                _ => "was still working".to_string(),
            };
            (
                Conversation {
                    context_id: Some(task.context_id.clone()),
                    outstanding: Some(Outstanding {
                        task_id: task.id.clone(),
                        waiting_for: Waiting::Agent,
                    }),
                },
                Ok(said(
                    format!(
                        "Remote agent `{name}` {why}; it keeps working remotely. Write `@{name}` \
                         alone to keep waiting for it, and do not hand it the task again."
                    ),
                    remote_output(name, &task, max_output_bytes),
                )),
            )
        }
        Ok(_) => (
            before,
            Err(format!(
                "Remote agent `{name}` answered in a way spawn does not understand."
            )),
        ),
        Err(A2aExchangeError::Send(error)) => (
            before,
            Err(format!(
                "Handing the task to remote agent `{name}` failed: {error}"
            )),
        ),
        Err(A2aExchangeError::Poll { task_id, error }) => (
            Conversation {
                outstanding: Some(Outstanding {
                    task_id,
                    waiting_for: Waiting::Agent,
                }),
                ..before
            },
            Err(format!(
                "Checking on remote agent `{name}` failed: {error}. The task may still be \
                 running; write `@{name}` alone to check on it again."
            )),
        ),
        Err(A2aExchangeError::Abandoned { canceled, .. }) => (
            Conversation {
                outstanding: None,
                ..before
            },
            Err(match canceled {
                Ok(()) => format!(
                    "The run was cancelled while remote agent `{name}` was working, and the task \
                     was asked to cancel."
                ),
                Err(error) => format!(
                    "The run was cancelled while remote agent `{name}` was working, and asking \
                     the task to cancel failed: {error}"
                ),
            }),
        ),
        Err(error) => (
            before,
            Err(format!(
                "Remote agent `{name}` could not be reached: {error}"
            )),
        ),
    }
}

/// A headline in basis's words, then the remote agent's own, already framed
/// as its output by mentra.
fn said(headline: String, output: String) -> String {
    if output.is_empty() {
        return headline;
    }
    format!("{headline}\n\n{output}")
}

#[cfg(test)]
mod tests;
