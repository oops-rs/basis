//! Handing a task to a remote agent, once the answer is in (ADR-0029).
//!
//! The agent is reached through the [`Peer`] its resolver handed back
//! (ADR-0038): the peer runs the exchange and frames the agent's words; what
//! is basis's is everything the model would otherwise have had to carry —
//! which conversation a task belongs to, and which task is still outstanding
//! (ADR-0029 §5). So every guidance line here is written in `spawn`'s
//! grammar, and none of them mentions an id.
//!
//! [`Peer`]: crate::agents::Peer

use std::time::SystemTime;

use mentra::tool::{ParallelToolContext, ToolContext, ToolResult};

use super::{describe::listed, no_remote_agents};
use crate::agents::{
    ExchangeBounds, Failure, Message, Outcome, PeerError, Request, Resolved, Route, Started,
    StillWorking, Unresolved,
    remote::{Conversation, Outstanding, Remembered, RemoteAgents, Waiting},
};

/// Whether `@<name>` can go anywhere, asked in the preview — before the
/// approver, and without waiting on the network (ADR-0037 §4).
///
/// A known agent is let through whether or not it is reachable now:
/// resolving it is the first thing [`delegate`] does (ADR-0031 §2), and
/// refusing it here would mean an agent that came up is never reached
/// without a reload. So is a name a resolver cannot classify without the
/// network. One tried too recently is refused, with how long until it is
/// worth asking again.
pub(super) fn authorize(agents: &RemoteAgents, name: &str) -> Result<(), String> {
    match agents.route(name) {
        Route::Known | Route::Maybe => Ok(()),
        Route::Backoff(wait) => Err(backing_off(name, wait)),
        Route::Refused(reason) => Err(refused(name, &reason)),
        Route::Unknown => Err(unknown(agents, name)),
    }
}

/// The refusal for an agent its resolver refused, in the resolver's words,
/// then what to do instead (ADR-0031 §5).
fn refused(name: &str, reason: &str) -> String {
    format!(
        "{reason}, so there is nothing to hand `@{name}` to; write the task with no `@` to hand \
         it to a subagent here"
    )
}

/// The refusal for a name no resolver of the calling agent's workspace knows.
pub(super) fn unknown(agents: &RemoteAgents, name: &str) -> String {
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

/// What the model reads when there is no peer to hand the task to.
fn unresolved(agents: &RemoteAgents, name: &str, why: Unresolved) -> String {
    match why {
        Unresolved::Backoff(wait) => backing_off(name, wait),
        Unresolved::Unreachable {
            error,
            retry_in: Some(retry_in),
        } => format!(
            "the remote agent `{name}` could not be reached: {error}. `@{name}` tries it again \
             in {}s",
            seconds(retry_in)
        ),
        Unresolved::Unreachable {
            error,
            retry_in: None,
        } => format!("the remote agent `{name}` could not be reached: {error}"),
        Unresolved::Refused(reason) => refused(name, &reason),
        Unresolved::Unknown => unknown(agents, name),
    }
}

/// Hands `body` to the remote agent `name`, or — with an empty body — waits
/// again on its outstanding task, and reports what came back.
///
/// `remembered` says where the calling agent's record is kept: on disk by
/// agent for one a workspace minted or resumed, under a host's key for one
/// bound to it, in memory for a delegated child (`crate::agents::remote`
/// has the rule).
pub(super) async fn delegate(
    ctx: ToolContext<'_>,
    agents: &RemoteAgents,
    name: &str,
    body: &str,
    metadata: Option<&serde_json::Map<String, serde_json::Value>>,
    remembered: &Remembered,
) -> ToolResult {
    // Held to the end of this call: a reload that swaps the set meanwhile
    // changes what the *next* `@<agent>` reaches, never this exchange
    // (ADR-0031 §6). An unreachable agent due a retry is connected here.
    let Resolved {
        peer, id, endpoint, ..
    } = agents
        .resolve(name)
        .await
        .map_err(|why| unresolved(agents, name, why))?;
    // The conversation is the agent's, not the name's (ADR-0037 §6): kept
    // under the id its resolver gave it, so a short handle and its full form
    // are one conversation, and a label handed to another agent does not
    // inherit one.
    let id = id.as_str();
    let caller = ctx.agent_id.clone();
    // The run's own bounds — cancellation, stop, deadline — taken before the
    // context is handed over for progress, which is the one thing the
    // exchange needs from it while it waits. The deadline also travels: the
    // remote agent is told when this side stops waiting.
    let options = ctx.child_run_options();
    let hop = Hop {
        hop: agents.hop().saturating_add(1),
        deadline: options.deadline,
    };
    let before = agents.conversation(&caller, id, remembered);
    let request = request(name, body, &before, hop)?;
    let request = match request {
        Request::Send(message) => Request::Send(message.with_metadata(metadata.cloned())),
        wait => wait,
    };

    let ctx: ParallelToolContext = ctx.into();
    let progress = |text: String| ctx.emit_progress(text);
    let save = |next: Conversation| {
        // Reported, not raised: the exchange it describes happened, and a
        // failed tool call would invite the model to do the work again.
        if let Err(error) = agents.record(&caller, id, next, remembered) {
            ctx.emit_progress(format!(
                "the conversation with `{name}` was not saved: {error}"
            ));
        }
    };
    // Recorded before the peer waits, which is the whole point of being
    // told: a process that dies while this waits comes back knowing the task
    // exists, and `@<agent>` alone picks it up instead of the model handing
    // the work off a second time.
    let started = |started: Started| {
        save(Conversation {
            context_id: started.context_id.or_else(|| before.context_id.clone()),
            outstanding: Some(Outstanding {
                task_id: started.task_id,
                waiting_for: Waiting::Agent,
                endpoint: Some(endpoint.clone()),
            }),
        });
    };

    let outcome = peer
        .exchange(
            request,
            &ExchangeBounds {
                options: &options,
                progress: &progress,
                started: &started,
            },
        )
        .await;

    let (next, result) = settle(name, before, outcome, &endpoint);
    save(next);
    result
}

/// Where one outbound message stands in a chain of agents: `hop` is this
/// message's — the runtime's own
/// plus one, so a top-level caller sends 1 — and `deadline` is when this
/// side stops waiting, which the run's options already hold as the earlier
/// of its own and any it inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hop {
    hop: u32,
    deadline: Option<SystemTime>,
}

/// What this call sends, given where the conversation stands — or why it
/// sends nothing.
fn request(
    name: &str,
    body: &str,
    conversation: &Conversation,
    hop: Hop,
) -> Result<Request, String> {
    match (&conversation.outstanding, body.is_empty()) {
        (Some(outstanding), true) => Ok(Request::wait(&outstanding.task_id)),
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
        (Some(outstanding), false) => Ok(Request::Send(
            Message::new(body)
                .in_context(conversation.context_id.clone())
                .answering(Some(outstanding.task_id.clone()))
                .at_hop(hop.hop, hop.deadline),
        )),
        (None, false) => Ok(Request::Send(
            Message::new(body)
                .in_context(conversation.context_id.clone())
                .at_hop(hop.hop, hop.deadline),
        )),
    }
}

/// Where the conversation stands after this exchange, and what the model
/// reads about it. `endpoint` is the one this exchange was sent to, recorded
/// with any task left outstanding.
fn settle(
    name: &str,
    before: Conversation,
    outcome: Result<Outcome, PeerError>,
    endpoint: &str,
) -> (Conversation, ToolResult) {
    let outstanding = |task_id: String, waiting_for: Waiting| {
        Some(Outstanding {
            task_id,
            waiting_for,
            endpoint: Some(endpoint.to_string()),
        })
    };
    let settled = |context_id: Option<String>| Conversation {
        context_id: context_id.or_else(|| before.context_id.clone()),
        outstanding: None,
    };
    match outcome {
        Ok(Outcome::Replied { context_id, text }) => (
            settled(context_id),
            Ok(said(format!("Remote agent `{name}` replied:"), text)),
        ),
        Ok(Outcome::Finished {
            context_id, text, ..
        }) => (
            settled(context_id),
            Ok(said(format!("Remote agent `{name}` replied:"), text)),
        ),
        Ok(Outcome::NeedsInput {
            task_id,
            context_id,
            text,
        }) => (
            Conversation {
                context_id: context_id.or_else(|| before.context_id.clone()),
                outstanding: outstanding(task_id, Waiting::Input),
            },
            Ok(said(
                format!(
                    "Remote agent `{name}` needs more before it can finish. Answer with \
                     `@{name} <your answer>`."
                ),
                text,
            )),
        ),
        Ok(Outcome::NeedsAuthorization {
            context_id, text, ..
        }) => (
            settled(context_id),
            Err(said(
                format!(
                    "Remote agent `{name}` needs authorization this conversation cannot give \
                     it, so the task stopped there."
                ),
                text,
            )),
        ),
        Ok(Outcome::Failed {
            context_id,
            text,
            how,
            ..
        }) => (
            settled(context_id),
            Err(said(
                format!(
                    "Remote agent `{name}` {}.",
                    match how {
                        Failure::Rejected => "rejected the task",
                        Failure::Canceled => "reports the task was canceled",
                        _ => "reports the task failed",
                    }
                ),
                text,
            )),
        ),
        Ok(Outcome::Working {
            task_id,
            context_id,
            text,
            why,
        }) => {
            let why = match why {
                StillWorking::WaitBound(waited) => {
                    format!("did not finish within {}s", waited.as_secs().max(1))
                }
                _ => "was still working when this run began to stop".to_string(),
            };
            (
                Conversation {
                    context_id: context_id.or_else(|| before.context_id.clone()),
                    outstanding: outstanding(task_id, Waiting::Agent),
                },
                Ok(said(
                    format!(
                        "Remote agent `{name}` {why}; it keeps working remotely. Write `@{name}` \
                         alone to keep waiting for it, and do not hand it the task again."
                    ),
                    text,
                )),
            )
        }
        // The agent moved since this task was handed over, and the new
        // endpoint could not pick it up — whatever it answered: one that does
        // not know a task need not say so (an HTTP 404, an invalid-params
        // refusal). Waiting again would fail the same way for good, so the
        // record ends here, and nothing is sent again.
        Err(error) if moved(&before, endpoint) && cause(&error).is_some() => (
            Conversation::default(),
            Err(format!(
                "Remote agent `{name}` has moved to a new address since it was handed the task it \
                 was working on, and the new address could not pick that task up ({}), so it is \
                 forgotten and the conversation with it starts over. Hand it the task again with \
                 `@{name} <task>` if it still needs doing.",
                cause(&error).expect("checked by the guard")
            )),
        ),
        Err(PeerError::UnknownTask) if before.outstanding.is_some() => (
            Conversation::default(),
            Err(format!(
                "Remote agent `{name}` no longer knows the task it was working on — it may have \
                 moved to a new address — so that task is forgotten and the conversation with \
                 it starts over. Hand it the task again with `@{name} <task>` if it still needs \
                 doing."
            )),
        ),
        Err(PeerError::UnknownTask) => (
            before,
            Err(format!(
                "Handing the task to remote agent `{name}` failed: it does not know the task \
                 this answers"
            )),
        ),
        Err(PeerError::Send(error)) => (
            before,
            Err(format!(
                "Handing the task to remote agent `{name}` failed: {error}"
            )),
        ),
        Err(PeerError::Check { task_id, error }) => (
            Conversation {
                outstanding: outstanding(task_id, Waiting::Agent),
                ..before
            },
            Err(format!(
                "Checking on remote agent `{name}` failed: {error}. The task may still be \
                 running; write `@{name}` alone to check on it again."
            )),
        ),
        Err(PeerError::Abandoned { canceled, .. }) => (
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

/// Whether the outstanding task in `conversation` was handed to an endpoint
/// other than `endpoint`. A record that predates the field cannot say, and is
/// taken not to have moved.
fn moved(conversation: &Conversation, endpoint: &str) -> bool {
    conversation
        .outstanding
        .as_ref()
        .and_then(|outstanding| outstanding.endpoint.as_deref())
        .is_some_and(|recorded| recorded != endpoint)
}

/// The words behind a request or a check that failed — which name no task
/// id, unlike the error around it. `None` for an abandoned or unrecognized
/// exchange, which is not evidence of a move.
fn cause(error: &PeerError) -> Option<String> {
    match error {
        PeerError::Send(error) | PeerError::Check { error, .. } => Some(error.clone()),
        PeerError::UnknownTask => Some(error.to_string()),
        _ => None,
    }
}

/// A headline in basis's words, then the remote agent's own, already framed
/// as its output by the peer.
fn said(headline: String, output: String) -> String {
    if output.is_empty() {
        return headline;
    }
    format!("{headline}\n\n{output}")
}

#[cfg(test)]
mod tests;
