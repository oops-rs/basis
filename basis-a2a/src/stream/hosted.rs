//! A hosted task's progress, as an A2A event stream (ADR-0033 §1).
//!
//! Each of the host's progress notes is a `WORKING` update with the note as
//! the agent's message. The stream ends when the host says the task has
//! settled — with its result artifact and terminal state, as a
//! `basis-tasks` task's does — whether or not the host's progress stream
//! ended first. A task that starts working with nothing to say is still
//! announced as `WORKING`, read off its standing at the journal's cadence;
//! a task that pauses on a question to its caller (ADR-0034) is read off it
//! the same way, and the stream closes on its `INPUT_REQUIRED` update.

use std::sync::Arc;

use a2a::StreamResponse;
use basis_tasks::POLL;
use futures::stream::{BoxStream, StreamExt};
use tokio::sync::mpsc;

use super::{settle, working};
use crate::{
    host::{HostedProgress, TaskHost},
    index::Record,
    project::{self, Standing},
    service::A2aService,
};

/// The task a stream follows, and who runs it.
pub(super) struct Followed {
    pub(super) service: A2aService,
    pub(super) record: Record,
    pub(super) host: Arc<dyn TaskHost>,
    pub(super) host_id: String,
}

pub(super) async fn produce(
    followed: Followed,
    mut progress: BoxStream<'static, HostedProgress>,
    mut announced_working: bool,
    sender: mpsc::Sender<StreamResponse>,
) {
    let Followed {
        service,
        record,
        host,
        host_id,
    } = followed;
    let settled = host.settled(&host_id);
    tokio::pin!(settled);
    let mut progress_open = true;
    let mut tick = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            // Notes already given are sent before the end that follows them.
            biased;
            () = sender.closed() => return,
            note = progress.next(), if progress_open => match note {
                Some(note) => {
                    announced_working = true;
                    if sender.send(working(&record, Some(note.note))).await.is_err() {
                        return;
                    }
                }
                // Fewer notes, not a settled task: wait for the host's word.
                None => progress_open = false,
            },
            outcome = &mut settled => {
                if let Err(error) = outcome {
                    eprintln!("basis-a2a: await hosted task {} for a stream: {error}", record.id);
                }
                break;
            }
            _ = tick.tick() => match service.standing(&record).await {
                Ok(standing @ Standing::Working) if !announced_working => {
                    announced_working = true;
                    let update =
                        StreamResponse::StatusUpdate(project::status_update(&record, &standing));
                    if sender.send(update).await.is_err() {
                        return;
                    }
                }
                // Asking its caller (ADR-0034): the stream closes on the
                // question, as a `basis-tasks` task's does; the caller
                // replies, then subscribes again.
                Ok(standing @ Standing::HostAsking(_)) => {
                    let update =
                        StreamResponse::StatusUpdate(project::status_update(&record, &standing));
                    let _ = sender.send(update).await;
                    return;
                }
                _ => {}
            },
        }
    }
    // Settled — or the host could not say, and where it stands now is the
    // last thing this stream can tell.
    let Ok(standing) = service.standing(&record).await else {
        return;
    };
    let updates = if standing.is_terminal() {
        settle(&record, &standing)
    } else {
        vec![StreamResponse::StatusUpdate(project::status_update(
            &record, &standing,
        ))]
    };
    for update in updates {
        if sender.send(update).await.is_err() {
            return;
        }
    }
}
