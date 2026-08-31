//! The per-task message inbox, `inbox.json`.
//!
//! An atomic-rewrite JSON array of [`MessageRecord`]. Every rewrite happens
//! under `inbox.lock`, held for the rewrite only — senders enqueue, the
//! executor drains at turn boundaries — and never across a model turn. The
//! settle pass's two writes — sweeping every unanswered message durable in
//! `inbox.json`, then `terminal.json` — both land under one hold of this
//! lock, so an enqueue that saw no terminal record is guaranteed to have its
//! message resolved (delivered, reply or terminal-tagged) rather than
//! stranded. The *order* of those two writes is load-bearing
//! ([`finish_unanswered_durably`]): the inbox first, so a crash between them
//! leaves a task that is still resumable, not one whose messages a settled
//! task can no longer re-sweep.

use std::io;

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    data_dir::{AgentPaths, write_private_atomic},
    lock,
    state::{
        InboxRecord, MAX_MESSAGE, MAX_MESSAGES, MessageRecord, MessageReply, MessageState,
        bounded_text, cancel_requested, load_meta, now_ms, read_terminal,
    },
};

/// A lock-free read. Atomic replacement means a reader observes either the old
/// complete array or the new complete array, never a partial one.
pub(crate) fn load(paths: &AgentPaths) -> Result<Vec<MessageRecord>, String> {
    match std::fs::read(paths.inbox()) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|error| format!("decode task inbox: {error}"))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("read task inbox: {error}")),
    }
}

/// Locks the inbox, applies `mutate`, and rewrites the file atomically. The
/// settle pass writes the terminal record from inside its callback so both
/// land under one lock hold.
pub(crate) fn update<T>(
    paths: &AgentPaths,
    mutate: impl FnOnce(&mut Vec<MessageRecord>) -> Result<T, String>,
) -> Result<T, String> {
    let _lock = lock::exclusive(&paths.inbox_lock())
        .map_err(|error| format!("lock task inbox: {error}"))?;
    let mut messages = load(paths)?;
    let value = mutate(&mut messages)?;
    save(paths, &messages)?;
    Ok(value)
}

fn save(paths: &AgentPaths, messages: &[MessageRecord]) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(messages).map_err(|error| format!("encode task inbox: {error}"))?;
    write_private_atomic(&paths.inbox(), &bytes)
        .map_err(|error| format!("persist task inbox: {error}"))
}

/// The settle pass's first write: every message not already delivered its
/// own correlated reply is marked delivered, durably, before anything else.
/// Returns the still-held inbox lock, so the caller's second write —
/// `write_terminal` — lands before a concurrent enqueue can, and before
/// anything releases this hold.
///
/// **Order is the whole point.** A crash between this write and the
/// terminal one that follows it leaves `inbox.json` swept but no
/// `terminal.json` — a task that is still resumable, whose `meta.json`
/// already recorded `pending_terminal` (set before `settle` is ever called),
/// so the next attach skips the model and simply finishes the settle pass:
/// re-sweeping (idempotent) and retrying the terminal write. Writing
/// `terminal.json` first would risk the opposite: a settled task — nothing
/// will ever attach it again — whose inbox still shows a message `Pending`
/// forever, because only an attach re-sweeps it.
pub(crate) fn finish_unanswered_durably(paths: &AgentPaths) -> Result<lock::Lock, String> {
    let guard = lock::exclusive(&paths.inbox_lock())
        .map_err(|error| format!("lock task inbox: {error}"))?;
    let mut messages = load(paths)?;
    finish_unanswered(&mut messages);
    save(paths, &messages)?;
    Ok(guard)
}

/// Enqueues one message, enforcing the lifetime and size bounds and refusing a
/// task that no longer accepts work. The terminal/pending/cancel checks run
/// under the inbox lock, which is what closes the race against a concurrent
/// settle.
pub(crate) fn enqueue(paths: &AgentPaths, task: &str, body: String) -> Result<String, String> {
    if body.trim().is_empty() {
        return Err("message is empty".to_string());
    }
    if body.len() > MAX_MESSAGE {
        return Err(format!(
            "message is {} bytes; the limit is {MAX_MESSAGE}",
            body.len()
        ));
    }
    update(paths, |messages| {
        if read_terminal(paths)?.is_some()
            || cancel_requested(paths)
            || load_meta(paths)?.pending_terminal.is_some()
        {
            return Err(format!("task {task} no longer accepts messages"));
        }
        if messages.len() >= MAX_MESSAGES {
            return Err(format!("task inbox is full (limit {MAX_MESSAGES})"));
        }
        let id = Uuid::new_v4().simple().to_string();
        messages.push(MessageRecord {
            id: id.clone(),
            body,
            body_truncated: false,
            state: MessageState::Pending,
            created_ms: now_ms(),
            reply: None,
        });
        Ok(id)
    })
}

/// Marks the next pending message in-flight and returns it, for the executor's
/// turn boundary.
/// Whether a message is waiting to be driven — [`start_next`] without the
/// claim, for an executor deciding whether there *is* a next turn before
/// committing to run it.
pub(crate) fn has_pending(paths: &AgentPaths) -> Result<bool, String> {
    Ok(load(paths)?
        .iter()
        .any(|message| message.state == MessageState::Pending))
}

pub(crate) fn start_next(paths: &AgentPaths) -> Result<Option<(String, String)>, String> {
    update(paths, |messages| {
        let Some(message) = messages
            .iter_mut()
            .find(|message| message.state == MessageState::Pending)
        else {
            return Ok(None);
        };
        message.state = MessageState::InFlight;
        Ok(Some((message.id.clone(), message.body.clone())))
    })
}

/// Records one correlated reply and delivers its message.
pub(crate) fn finish(
    paths: &AgentPaths,
    id: &str,
    reply: Option<MessageReply>,
) -> Result<(), String> {
    update(paths, |messages| {
        if let Some(message) = messages.iter_mut().find(|message| message.id == id) {
            message.state = MessageState::Delivered;
            message.reply = reply;
        }
        Ok(())
    })
}

/// A message left in flight by a crash reverts to pending on attach; the
/// re-driven turn may repeat tool side effects (ADR-0019 states this in bold).
pub(crate) fn revert_in_flight(paths: &AgentPaths) -> Result<(), String> {
    update(paths, |messages| {
        for message in messages.iter_mut() {
            if message.state == MessageState::InFlight {
                message.state = MessageState::Pending;
            }
        }
        Ok(())
    })
}

pub(crate) fn finish_unanswered(messages: &mut [MessageRecord]) {
    for message in messages.iter_mut() {
        if message.state != MessageState::Delivered {
            message.state = MessageState::Delivered;
        }
    }
}

/// The `basis inbox` payload: bounded 4 KiB summaries with truncation metadata.
pub(crate) fn inbox_record(task: &str, messages: &[MessageRecord]) -> InboxRecord {
    let messages: Vec<MessageRecord> = messages
        .iter()
        .map(|message| {
            let (body, body_truncated) = bounded_text(message.body.clone(), 4 * 1024);
            let reply = message.reply.as_ref().map(|reply| {
                let (result, result_truncated) = bounded_text(reply.result.clone(), 4 * 1024);
                MessageReply {
                    result,
                    result_truncated: reply.result_truncated || result_truncated,
                    stopped_by: reply.stopped_by,
                }
            });
            MessageRecord {
                id: message.id.clone(),
                body,
                body_truncated: message.body_truncated || body_truncated,
                state: message.state,
                created_ms: message.created_ms,
                reply,
            }
        })
        .collect();
    let raw_messages: Vec<Value> = messages
        .iter()
        .map(|message| {
            let reply = message.reply.as_ref().map(|reply| {
                json!({
                    "result": reply.result,
                    "result_truncated": reply.result_truncated,
                    "stopped_by": reply.stopped_by,
                })
            });
            json!({
                "id": message.id,
                "state": message.state,
                "body": message.body,
                "body_truncated": message.body_truncated,
                "reply": reply,
            })
        })
        .collect();
    InboxRecord {
        raw: json!({
            "task": task,
            "messages": raw_messages,
            "next": format!("basis watch {task}"),
        }),
        messages,
    }
}

/// Resolves a `wait --message` dispatch: the correlated reply when one exists,
/// the terminal payload tagged with the message id when the message was
/// delivered without one, and `None` while the answer is still owed.
pub(crate) fn message_payload_for_dispatch(
    task: &str,
    messages: &[MessageRecord],
    message_id: &str,
    terminal: Option<&Value>,
) -> Result<Option<Value>, String> {
    let message = messages
        .iter()
        .find(|message| message.id == message_id)
        .ok_or_else(|| format!("message {message_id} does not exist on task {task}"))?;
    if let Some(reply) = &message.reply {
        let mut payload = json!({
            "task": task,
            "message": message_id,
            "state": "succeeded",
            "result": reply.result,
            "result_truncated": reply.result_truncated,
            "stopped_by": reply.stopped_by,
            "next": format!("basis inbox {task}"),
        });
        if !reply.result_truncated {
            payload["result_truncated"] = Value::Null;
        }
        return Ok(Some(payload));
    }
    if message.state == MessageState::Delivered
        && let Some(terminal) = terminal
    {
        let mut payload = terminal.clone();
        let object = payload
            .as_object_mut()
            .expect("terminal payload is an object");
        object.insert("task".to_string(), json!(task));
        object.insert("message".to_string(), json!(message_id));
        object.insert("next".to_string(), json!(format!("basis inbox {task}")));
        return Ok(Some(payload));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        data_dir::DataDir,
        state::{RunOptions, TaskMeta, save_meta, write_terminal},
    };

    fn agent(dir: &tempfile::TempDir) -> (AgentPaths, String) {
        let data = DataDir::from_path(dir.path()).unwrap();
        let task = "0123456789abcdef/0123456789abcdef0123456789abcdef".to_string();
        let paths = data.agent_dir(&task).unwrap();
        std::fs::create_dir_all(paths.dir()).unwrap();
        let meta = TaskMeta::new(
            task.clone(),
            None,
            true,
            "/repo".to_string(),
            "prompt".to_string(),
            RunOptions::default(),
            None,
        );
        save_meta(&paths, &meta).unwrap();
        (paths, task)
    }

    #[test]
    fn each_message_keeps_its_own_reply() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, task) = agent(&dir);
        let first = enqueue(&paths, &task, "first".to_string()).unwrap();
        let second = enqueue(&paths, &task, "second".to_string()).unwrap();

        let (started, _) = start_next(&paths).unwrap().unwrap();
        assert_eq!(started, first);
        finish(
            &paths,
            &first,
            Some(MessageReply {
                result: "reply one".to_string(),
                result_truncated: false,
                stopped_by: None,
            }),
        )
        .unwrap();
        let (next, _) = start_next(&paths).unwrap().unwrap();
        assert_eq!(next, second);
        finish(
            &paths,
            &second,
            Some(MessageReply {
                result: "reply two".to_string(),
                result_truncated: false,
                stopped_by: None,
            }),
        )
        .unwrap();

        let messages = load(&paths).unwrap();
        for (id, reply) in [(&first, "reply one"), (&second, "reply two")] {
            let payload = message_payload_for_dispatch(&task, &messages, id, None)
                .unwrap()
                .expect("reply is durable");
            assert_eq!(payload["message"], id.as_str());
            assert_eq!(payload["result"], reply);
            assert_eq!(payload["state"], "succeeded");
        }
    }

    #[test]
    fn a_terminal_task_resolves_unanswered_messages_with_its_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, task) = agent(&dir);
        let first = enqueue(&paths, &task, "first".to_string()).unwrap();
        let second = enqueue(&paths, &task, "second".to_string()).unwrap();
        start_next(&paths).unwrap();

        let terminal = json!({"state": "failed", "error": "provider failed"});
        update(&paths, |messages| {
            finish_unanswered(messages);
            write_terminal(&paths, &terminal)
        })
        .unwrap();

        let messages = load(&paths).unwrap();
        for id in [first, second] {
            let payload = message_payload_for_dispatch(&task, &messages, &id, Some(&terminal))
                .unwrap()
                .expect("terminal resolves the message");
            assert_eq!(payload["state"], "failed");
            assert_eq!(payload["message"], id);
        }
    }

    #[test]
    fn a_settled_or_cancelled_task_no_longer_accepts_messages() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, task) = agent(&dir);
        write_terminal(&paths, &json!({"state": "cancelled"})).unwrap();
        let error = enqueue(&paths, &task, "late".to_string()).unwrap_err();
        assert!(error.contains("no longer accepts"), "{error}");
    }

    #[test]
    fn the_inbox_is_bounded_for_life() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, task) = agent(&dir);
        for index in 0..MAX_MESSAGES {
            enqueue(&paths, &task, format!("message {index}")).unwrap();
        }
        let error = enqueue(&paths, &task, "one too many".to_string()).unwrap_err();
        assert!(error.contains("full"), "{error}");
    }

    #[test]
    fn inbox_record_keeps_the_raw_payload_and_bounded_typed_messages() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, task) = agent(&dir);
        enqueue(&paths, &task, "x".repeat(5 * 1024)).unwrap();

        let stored = load(&paths).unwrap();
        let record = inbox_record(&task, &stored);

        assert_eq!(record.raw["messages"][0]["body"], record.messages[0].body);
        assert_eq!(record.raw["messages"][0]["body_truncated"], true);
        assert!(record.messages[0].body_truncated);
        assert_eq!(record.messages[0].body.len(), 4 * 1024);
        assert!(record.raw["messages"][0].get("created_ms").is_none());
        assert_eq!(record.messages[0].created_ms, stored[0].created_ms);
    }

    #[test]
    fn a_crashed_turns_message_reverts_to_pending_on_attach() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, task) = agent(&dir);
        let id = enqueue(&paths, &task, "in flight".to_string()).unwrap();
        start_next(&paths).unwrap();
        revert_in_flight(&paths).unwrap();
        let (again, _) = start_next(&paths).unwrap().expect("re-driveable");
        assert_eq!(again, id);
    }
}
