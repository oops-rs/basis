//! A hosted task's progress, as an A2A event stream (ADR-0033 §1).
//!
//! Each of the host's progress notes is a `WORKING` update with the note as
//! the agent's message. The stream ends when the host says the task has
//! settled — with its result artifact and terminal state, as a
//! `basis-tasks` task's does — whether or not the host's progress stream
//! ended first. A task that starts working with nothing to say is still
//! announced as `WORKING`, read off its standing at the journal's cadence.

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
            _ = tick.tick(), if !announced_working => {
                if let Ok(standing @ Standing::Working) = service.standing(&record).await {
                    announced_working = true;
                    let update =
                        StreamResponse::StatusUpdate(project::status_update(&record, &standing));
                    if sender.send(update).await.is_err() {
                        return;
                    }
                }
            }
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
