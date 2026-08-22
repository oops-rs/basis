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
    schema::v1::{
        ContentBlock, Error, ImageContent, PromptRequest, PromptResponse, SessionId, SessionUpdate,
        StopReason, UsageUpdate,
    },
};
use base64::Engine;

use super::notify;
use crate::{
    approver::AcpApprover, mode::ModedApprover, session::SessionRegistry, update::session_update,
};
use basis::{Event, PromptPart, run::EventSink};

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

    let parts = prompt_parts(&request.prompt)?;
    if parts.is_empty() {
        return Err(Error::invalid_params().data("prompt has nothing basis can send"));
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

    let report = run.send_parts(parts, sink, approver, options).await;
    session.end_turn();

    // Read before the run is dropped — `context_window` and
    // `estimated_context_tokens` are `PreparedRun`'s own, not the report's.
    let usage = usage_update(run.context_window(), run.estimated_context_tokens());
    drop(run);

    // Best-effort: a dead connection here must not turn an otherwise finished
    // turn into a failure the client never asked about — the response below
    // is what actually reports the outcome.
    if let Some(usage) = usage {
        let _ = notify(
            connection,
            &request.session_id,
            SessionUpdate::UsageUpdate(usage),
        );
    }

    // A cancelled turn fails inside mentra, so the token — not the error — is
    // what distinguishes "the client stopped it" from "it broke". ACP requires
    // `Cancelled` in that case.
    if cancelled.is_some_and(|token| token.is_cancelled()) {
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }

    match report {
        // The bound is read before the outcome, because a bound that ended a
        // turn is the answer whichever way the turn came back. `TokenBudget`
        // is graceful and can arrive on a run that answered (see
        // `Bound::TokenBudget`), and reporting that as `EndTurn` would drop
        // the one fact the client needs to know: there would have been more.
        Ok(report) => match report.stopped_by {
            Some(bound) => Ok(PromptResponse::new(stop_reason(bound))),
            None if report.succeeded() => Ok(PromptResponse::new(StopReason::EndTurn)),
            None => Err(Error::internal_error().data(match report.outcome {
                basis::RunOutcome::Error { message } => message,
                basis::RunOutcome::Ok => "the turn failed".to_string(),
            })),
        },
        Err(error) => Err(Error::internal_error().data(error.to_string())),
    }
}

/// This turn's `UsageUpdate`, or nothing when the window is unknown.
///
/// A per-turn token count beside a guessed ceiling would tell a client's usage
/// bar something basis does not actually know, and ACP's `UsageUpdate` has no
/// field for marking `size` as a guess — so unlike every other update in this
/// module, silence here is the honest answer rather than a gap. `used` is
/// [`PreparedRun::estimated_context_tokens`](basis::PreparedRun::estimated_context_tokens)'s
/// floor, not the provider's own count: ACP has no field for that distinction
/// either, and a floor that undercounts a full context is safer to show than
/// one that never appears.
pub(super) fn usage_update(
    context_window: Option<usize>,
    estimated_tokens: usize,
) -> Option<UsageUpdate> {
    context_window.map(|size| UsageUpdate::new(estimated_tokens as u64, size as u64))
}

/// Which of ACP's stop reasons a tripped bound is.
///
/// A bound is not a failure — committed work is kept, and the CLI has an exit
/// code of its own for exactly this (ADR-0014, ADR-0015). Reporting it as
/// `-32603` told a client that set a budget that basis had broken, which is the
/// one reading that is certainly wrong.
///
/// Two of the three land on a name ACP already has. The third does not, and the
/// choice is between four wrong answers:
///
/// - `Refusal` carries a documented consequence — "the user prompt and
///   everything that comes after it won't be included in the next prompt" —
///   which is false here. A deadline keeps what the turn committed.
/// - `Cancelled` is reserved: ACP says it MUST be returned when the client
///   sends `session/cancel`, so using it would tell a client its own stop
///   button was pressed when nobody touched it.
/// - `EndTurn` means the turn ended successfully, which hides the bound
///   entirely — the failure this mapping exists to fix, wearing a nicer code.
/// - `MaxTurnRequests` means the agent reached the allowance it had for
///   requests between user turns. It names the wrong unit and the right event:
///   an allowance the operator set ran out, and the agent was refused another
///   round. Everything a client does with it — say so, offer to continue — is
///   what a deadline calls for too.
///
/// So `MaxTurnRequests`, and the unit is the only thing lost. The exact bound
/// is still on the event stream as [`Event::RunFinished`](basis::Event::RunFinished)'s
/// `stopped_by`, for a client that wants it.
pub(super) fn stop_reason(bound: basis::Bound) -> StopReason {
    match bound {
        basis::Bound::TokenBudget => StopReason::MaxTokens,
        basis::Bound::ToolBudget | basis::Bound::Deadline => StopReason::MaxTurnRequests,
        // `Bound` is `#[non_exhaustive]`. A bound basis-acp has not been taught
        // still ended the turn, and every remaining variant is wrong in a way
        // this one is not: it is at least an allowance that ran out.
        _ => StopReason::MaxTurnRequests,
    }
}

/// A client's prompt, as the pieces basis sends.
///
/// Order is preserved, and consecutive text-ish blocks are joined into one
/// part rather than sent as several. Both halves matter: a client that put a
/// question after a screenshot means something different from one that put it
/// before, and a run of text blocks that arrived split is one thing the user
/// typed, not three.
///
/// Blocks basis cannot carry are dropped, not refused. `audio` is the only
/// one left, and `initialize` never claimed it — a client that sends one
/// anyway gets the rest of its prompt rather than an error about a capability
/// it was told basis does not have.
pub(super) fn prompt_parts(blocks: &[ContentBlock]) -> Result<Vec<PromptPart>, Error> {
    let mut parts = Vec::new();
    let mut text: Vec<String> = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Image(image) => {
                flush(&mut text, &mut parts);
                parts.push(image_part(image)?);
            }
            other => {
                if let Some(line) = block_text(other) {
                    text.push(line);
                }
            }
        }
    }
    flush(&mut text, &mut parts);

    Ok(parts)
}

/// Turns whatever text has accumulated into one part, and starts a new run.
fn flush(text: &mut Vec<String>, parts: &mut Vec<PromptPart>) {
    if !text.is_empty() {
        parts.push(PromptPart::text(std::mem::take(text).join("\n")));
    }
}

/// One ACP image, as bytes.
///
/// ACP carries the payload base64-encoded and mentra takes the bytes, so this
/// is where the two meet. Undecodable data is refused rather than dropped: a
/// prompt that quietly lost the screenshot it was about is worse than one that
/// says why it could not be sent, and `uri` — which ACP allows alongside the
/// data — is not a fallback basis can use, because Gemini rejects a URL image
/// outright.
fn image_part(image: &ImageContent) -> Result<PromptPart, Error> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|error| {
            Error::invalid_params().data(format!("image data is not valid base64: {error}"))
        })?;

    Ok(PromptPart::image(image.mime_type.clone(), data))
}

/// The text of one block, for the blocks that have some.
///
/// Resource links and embedded resources are named rather than inlined: basis
/// does not fetch on the client's behalf, and dropping them silently would
/// lose what the user attached.
fn block_text(block: &ContentBlock) -> Option<String> {
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
}

/// The text of a prompt, concatenating its text blocks.
///
/// What [`prompt_parts`] produces for a prompt with no images, which is nearly
/// all of them. Kept as its own function because that equivalence is worth
/// being able to assert.
#[cfg(test)]
pub(super) fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(block_text)
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
