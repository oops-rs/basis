//! A task's progress, as an A2A event stream.
//!
//! For a hosted task the source is its host's progress notes, each a
//! `WORKING` update with the note as its message (see `hosted`). For a
//! `basis-tasks` task it is the task's own `events.jsonl`, through
//! `basis_tasks::EventCursor` — the journal every executor writes whoever
//! drives the task, so a stream reads the same whether this process is
//! driving or a CLI `basis wait` attached first. What is already in the
//! journal when a stream opens is skipped: the spec's stream begins with the
//! task as it is *now*, and the history up to now is what that snapshot is.
//!
//! The stream is ordered — one producer, one channel — and ends after the
//! terminal status update. Each subscriber gets its own cursor, so a second
//! stream on the same task neither steals from nor ends the first.
//!
//! Only three of a run's events become updates, because a caller's UI wants
//! progress and not a transcript: the run starting (`WORKING`), each
//! completed assistant message (`WORKING`, with that text as a progress
//! note), and each tool starting (`WORKING`, naming the tool). Deltas,
//! reasoning, usage and the rest stay in the journal for `basis watch`.

mod hosted;

use std::sync::Arc;

use a2a::{A2AError, StreamResponse, TaskArtifactUpdateEvent, TaskState, TaskStatusUpdateEvent};
use basis::Event;
use basis_tasks::{EventCursor, POLL};
use futures::stream::{self, BoxStream, StreamExt};
use tokio::sync::mpsc;

use crate::{
    host::{HostedProgress, TaskHost},
    index::Record,
    project::{self, Standing, View, internal},
    service::{A2aService, Backend, StreamSlot, host_id_of},
};

/// How many updates may wait unread before the producer waits for the
/// subscriber. A subscriber that stops reading holds back only its own
/// stream.
const BUFFER: usize = 64;

/// Where a stream's progress comes from.
enum Source {
    /// A `basis-tasks` task's journal.
    Journal(EventCursor),
    /// A host's progress notes on its task.
    Host {
        host: Arc<dyn TaskHost>,
        host_id: String,
        progress: BoxStream<'static, HostedProgress>,
    },
}

/// Streams `record`'s task: its current state, then its progress, ending at
/// its terminal state — or at a question to its caller, the interrupted
/// state the spec closes a stream at too (§11.7). The caller answers, then
/// subscribes again.
/// `slot` is the stream's place under its caller's cap, held for as long as
/// the stream is.
pub(crate) async fn follow(
    service: A2aService,
    record: Record,
    history_length: Option<usize>,
    slot: StreamSlot,
) -> Result<BoxStream<'static, StreamResponse>, A2AError> {
    // Opened before the snapshot is read, so nothing that happens between
    // the two is missed.
    let source = match service.backend() {
        Backend::Tasks(backend) => {
            let mut cursor = backend
                .tasks
                .watch(project::handle_of(&record)?)
                .map_err(internal)?;
            // Everything journaled so far is what the snapshot below already
            // says.
            cursor.poll().map_err(internal)?;
            Source::Journal(cursor)
        }
        Backend::Hosted(host) => {
            let host_id = host_id_of(&record)?.to_string();
            let progress = host.progress(&host_id);
            Source::Host {
                host: Arc::clone(host),
                host_id,
                progress,
            }
        }
    };
    let standing = service.standing(&record).await?;
    let snapshot = StreamResponse::Task(project::task(
        &record,
        &standing,
        View::full(history_length),
    ));

    let (sender, receiver) = mpsc::channel(BUFFER);
    if !standing.is_interrupted_or_terminal() {
        let working = matches!(standing, Standing::Working);
        match source {
            Source::Journal(cursor) => {
                tokio::spawn(produce(service, record, cursor, working, sender));
            }
            Source::Host {
                host,
                host_id,
                progress,
            } => {
                let followed = hosted::Followed {
                    service,
                    record,
                    host,
                    host_id,
                };
                tokio::spawn(hosted::produce(followed, progress, working, sender));
            }
        }
    }
    let rest = stream::unfold((receiver, slot), |(mut receiver, slot)| async move {
        receiver
            .recv()
            .await
            .map(|update| (update, (receiver, slot)))
    });
    Ok(stream::once(async move { snapshot }).chain(rest).boxed())
}

async fn produce(
    service: A2aService,
    record: Record,
    mut cursor: EventCursor,
    mut announced_working: bool,
    sender: mpsc::Sender<StreamResponse>,
) {
    loop {
        tokio::time::sleep(POLL).await;
        // A subscriber that went away while nothing happened would otherwise
        // only be noticed on the next send, which a task nobody drives never
        // makes.
        if sender.is_closed() {
            return;
        }
        let records = match cursor.poll() {
            Ok(records) => records,
            Err(error) => {
                eprintln!("basis-a2a: read task {} events: {error}", record.id);
                return;
            }
        };
        for event in records.iter().filter_map(|record| record.event.as_deref()) {
            let Some(update) = progress(&record, event) else {
                continue;
            };
            announced_working = true;
            if sender.send(update).await.is_err() {
                return;
            }
        }
        let standing = match service.standing(&record).await {
            Ok(standing) => standing,
            Err(_) => return,
        };
        match standing {
            Standing::Settled(_) => {
                for update in settle(&record, &standing) {
                    if sender.send(update).await.is_err() {
                        return;
                    }
                }
                return;
            }
            Standing::Asking(_) => {
                let update =
                    StreamResponse::StatusUpdate(project::status_update(&record, &standing));
                let _ = sender.send(update).await;
                return;
            }
            Standing::Working if !announced_working => {
                announced_working = true;
                let update =
                    StreamResponse::StatusUpdate(project::status_update(&record, &standing));
                if sender.send(update).await.is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// The update one journaled event is worth, if any.
fn progress(record: &Record, event: &Event) -> Option<StreamResponse> {
    let note = match event {
        Event::RunStarted { .. } => None,
        Event::AssistantMessage { text } if !text.trim().is_empty() => Some(text.clone()),
        Event::ToolStarted { tool_name, .. } => Some(format!("Using `{tool_name}`.")),
        _ => return None,
    };
    Some(working(record, note))
}

/// A `WORKING` status update, with `note` as the agent's message when there
/// is one.
fn working(record: &Record, note: Option<String>) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: record.id.clone(),
        context_id: record.context_id.clone(),
        status: a2a::TaskStatus {
            state: TaskState::Working,
            message: note.map(|text| project::agent_message(record, text)),
            timestamp: project::timestamp(basis_tasks::now_ms()),
        },
        metadata: None,
    })
}

/// The last updates of a settled task: its result, when it has one, then its
/// terminal status — in that order, so the stream closes on the state.
fn settle(record: &Record, standing: &Standing) -> Vec<StreamResponse> {
    let mut updates = Vec::with_capacity(2);
    if let Some(artifact) = project::result_artifact(standing) {
        updates.push(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: record.id.clone(),
            context_id: record.context_id.clone(),
            artifact,
            append: Some(false),
            last_chunk: Some(true),
            metadata: None,
        }));
    }
    updates.push(StreamResponse::StatusUpdate(project::status_update(
        record, standing,
    )));
    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis_tasks::{TaskHandle, TerminalRecord};
    use serde_json::json;

    fn record() -> Record {
        let handle = TaskHandle::parse(format!("{}/{}", "0".repeat(16), "a".repeat(32))).unwrap();
        Record::new(
            &handle,
            &crate::config::Principal::anonymous(),
            "ctx".into(),
            "m1".into(),
            "hi".into(),
            1,
        )
    }

    #[test]
    fn deltas_are_not_progress_but_messages_and_tools_are() {
        let record = record();
        assert!(progress(&record, &Event::AssistantDelta { text: "par".into() }).is_none());
        let Some(StreamResponse::StatusUpdate(update)) = progress(
            &record,
            &Event::AssistantMessage {
                text: "half way".into(),
            },
        ) else {
            panic!("an assistant message is progress");
        };
        assert_eq!(update.status.state, TaskState::Working);
        assert_eq!(update.status.message.unwrap().text(), Some("half way"));
    }

    #[test]
    fn a_completed_task_ends_with_its_artifact_then_its_state() {
        let raw = json!({"state": "succeeded", "result": "done"});
        let standing = Standing::Settled(
            TerminalRecord {
                terminal: serde_json::from_value(raw.clone()).ok(),
                raw,
                stopped_by: None,
                usage: None,
                result_truncated: false,
            }
            .into(),
        );
        let updates = settle(&record(), &standing);
        assert!(matches!(updates[0], StreamResponse::ArtifactUpdate(_)));
        let StreamResponse::StatusUpdate(last) = &updates[1] else {
            panic!("the stream closes on the state");
        };
        assert_eq!(last.status.state, TaskState::Completed);
    }
}
