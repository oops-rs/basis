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
    A2aError as ClientError, A2aExchange, A2aExchangeError, A2aRequest, A2aRpcError,
    A2aStillWorking, A2aSubmitted, is_settled, remote_output, remote_reply, settle as settle_task,
    submit, types::TaskState,
};
use mentra::tool::{ParallelToolContext, ToolContext, ToolResult};

use super::{describe::listed, no_remote_agents};
use crate::a2a::remote::{Conversation, Outstanding, RemoteAgents, Route, Unreached, Waiting};

/// Whether `@<name>` can go anywhere, asked in the preview — before the
/// approver, and without waiting on the network.
///
/// An unreachable agent due a retry is let through: connecting it is the
/// first thing [`delegate`] does (ADR-0031 §2), and refusing it here would
/// mean an agent that came up is never reached without a reload. One tried
/// too recently is refused, with how long until it is worth asking again.
pub(super) fn authorize(agents: &RemoteAgents, name: &str) -> Result<(), String> {
    match agents.route(name) {
        Route::Connected | Route::Retry => Ok(()),
        Route::Backoff(wait) => Err(backing_off(name, wait)),
        Route::Removed => Err(removed(name)),
        Route::Unknown => Err(unknown(agents, name)),
    }
}

/// The refusal for an agent a reload took away: by name, so the model stops
/// asking for it rather than guessing at a spelling (ADR-0031 §5).
fn removed(name: &str) -> String {
    format!(
        "the remote agent `{name}` is no longer configured for this workspace, so there is \
         nothing to hand `@{name}` to; write the task with no `@` to hand it to a subagent here"
    )
}

/// The refusal for a name the calling agent's workspace does not configure.
fn unknown(agents: &RemoteAgents, name: &str) -> String {
    let configured = agents.names();
    if configured.is_empty() {
        return no_remote_agents(name);
    }
    format!(
        "there is no remote agent named `{name}`; the ones this workspace configures are {}, \
         and `@@` begins a task with a literal `@`",
        listed(&configured)
    )
}

/// The refusal for an unreachable agent tried too recently to try again.
fn backing_off(name: &str, wait: std::time::Duration) -> String {
    format!(
        "the remote agent `{name}` could not be reached when it was last tried; `@{name}` tries \
         it again in {}s",
        seconds(wait)
    )
}

/// Whole seconds, never zero: "again in 0s" reads as "now", which it is not.
fn seconds(wait: std::time::Duration) -> u64 {
    wait.as_secs_f64().ceil().max(1.0) as u64
}

/// What the model reads when there is no client to hand the task to.
fn unreached(agents: &RemoteAgents, name: &str, why: Unreached) -> String {
    match why {
        Unreached::Backoff(wait) => backing_off(name, wait),
        // mentra's error carries no server text and no header value.
        Unreached::Failed { error, retry_in } => format!(
            "the remote agent `{name}` could not be reached: {error}. `@{name}` tries it again \
             in {}s",
            seconds(retry_in)
        ),
        Unreached::Removed => removed(name),
        Unreached::Unknown => unknown(agents, name),
    }
}

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
    // Held to the end of this call: a reload that swaps the set meanwhile
    // changes what the *next* `@<agent>` reaches, never this exchange
    // (ADR-0031 §6). An unreachable agent due a retry is connected here.
    let client = agents
        .reach(name)
        .await
        .map_err(|why| unreached(agents, name, why))?;
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
        // The task basis kept for it is one the agent does not know — most
        // often because a reload moved the agent to a new endpoint while the
        // task was outstanding (ADR-0031 §5). Waiting on it again could only
        // fail again, so the record ends here, and the conversation with it:
        // its context is the old endpoint's too.
        Err(error) if before.outstanding.is_some() && task_not_found(&error) => (
            Conversation::default(),
            Err(format!(
                "Remote agent `{name}` no longer knows the task it was working on — it may have \
                 moved to a new address — so that task is forgotten and the conversation with \
                 it starts over. Hand it the task again with `@{name} <task>` if it still needs \
                 doing."
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
