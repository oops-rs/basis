//! Running one turn, and the sink that streams it.
//!
//! Alone in a module because it is the only handler that runs the agent: it
//! holds the session's turn lock for as long as the model takes, asks the
//! client for permission through [`AcpApprover`], and forwards every event on
//! the way past. Everything else a client can ask for is bookkeeping around
//! it, in [`lifecycle`](super::lifecycle).
//!
//! [`NotificationSink`] is here rather than beside [`session_update`] because
//! it is that mapping bound to one connection and one session, which is a
//! thing only a turn has.

use agent_client_protocol::{
    Client, ConnectionTo,
    schema::v1::{ContentBlock, Error, PromptRequest, PromptResponse, SessionId, StopReason},
};

use super::notify;
use crate::{
    approver::AcpApprover, mode::ModedApprover, session::SessionRegistry, update::session_update,
};
use basis::{Event, run::EventSink};

/// Runs one turn, streaming its events to the client as `session/update`.
///
/// Always called from a spawned task, never from the dispatch loop.
pub(super) async fn prompt(
    sessions: &SessionRegistry,
    connection: &ConnectionTo<Client>,
    request: PromptRequest,
) -> Result<PromptResponse, Error> {
    let session = sessions
        .get(&request.session_id)
        .ok_or_else(|| Error::invalid_params().data("unknown session"))?;

    let text = prompt_text(&request.prompt);
    if text.trim().is_empty() {
        return Err(Error::invalid_params().data("prompt has no text content"));
    }

    // The session's mode decides which of these requests the client actually
    // sees; the runtime surfaces every consequential call so that it can.
    let approver = ModedApprover::new(
        session.modes().clone(),
        AcpApprover::new(request.session_id.clone(), connection.clone()),
    );
    let sink = NotificationSink::new(request.session_id.clone(), connection.clone());

    // Held across the turn: one conversation runs one turn at a time, which is
    // what ACP's own model assumes. The cancellation token lives outside this
    // lock, so `session/cancel` can reach it while the turn holds it.
    let mut run = session.lock_turn().await;
    let options = session.begin_turn();
    let cancelled = options.cancel.clone();

    let report = run.send_with_options(text, sink, approver, options).await;
    session.end_turn();
    drop(run);

    // A cancelled turn fails inside mentra, so the token — not the error — is
    // what distinguishes "the client stopped it" from "it broke". ACP requires
    // `Cancelled` in that case.
    if cancelled.is_some_and(|token| token.is_cancelled()) {
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }

    match report {
        Ok(report) if report.succeeded() => Ok(PromptResponse::new(StopReason::EndTurn)),
        Ok(report) => Err(Error::internal_error().data(match report.outcome {
            basis::RunOutcome::Error { message } => message,
            basis::RunOutcome::Ok => "the turn failed".to_string(),
        })),
        Err(error) => Err(Error::internal_error().data(error.to_string())),
    }
}

/// The text of a prompt, concatenating its text blocks.
///
/// Resource links and embedded resources are named rather than inlined: basis
/// does not fetch on the client's behalf, and dropping them silently would
/// lose what the user attached.
pub(super) fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            ContentBlock::ResourceLink(link) => Some(format!("[{}]({})", link.name, link.uri)),
            ContentBlock::Resource(resource) => match &resource.resource {
                agent_client_protocol::schema::v1::EmbeddedResourceResource::TextResourceContents(
                    contents,
                ) => Some(contents.text.clone()),
                _ => None,
            },
            _ => None,
        }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An [`EventSink`] that forwards to the client as `session/update`.
struct NotificationSink {
    session_id: SessionId,
    connection: ConnectionTo<Client>,
}

impl NotificationSink {
    fn new(session_id: SessionId, connection: ConnectionTo<Client>) -> Self {
        Self {
            session_id,
            connection,
        }
    }
}

impl EventSink for NotificationSink {
    fn emit(&mut self, event: Event) -> std::io::Result<()> {
        let Some(update) = session_update(&event) else {
            return Ok(());
        };

        // Fire-and-forget, so this is safe from any task. A send failure means
        // the client is gone; returning the error stops forwarding for the
        // rest of the turn rather than writing into a dead socket repeatedly.
        notify(&self.connection, &self.session_id, update)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}
