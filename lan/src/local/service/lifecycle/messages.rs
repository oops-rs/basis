//! The per-task message inbox: enqueue, list, and resolve a dispatch.

use serde_json::{Value, json};

use super::transition::persist;
use crate::local::{
    service::{Shared, notify_changed},
    store,
};

pub(in crate::local::service) async fn enqueue_message(
    shared: &Shared,
    task: &str,
    message: String,
) -> Result<String, String> {
    if message.trim().is_empty() {
        return Err("message is empty".to_string());
    }
    let id = {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        let record = journal
            .get_mut(task)
            .ok_or_else(|| format!("task {task} does not exist"))?;
        if !record.accepts_work() {
            return Err(format!("task {task} no longer accepts messages"));
        }
        record.add_message(message)?
    };
    if let Err(error) = persist(shared).await {
        let mut journal = shared.journal.lock().expect("task journal poisoned");
        if let Some(record) = journal.get_mut(task)
            && let Some(index) = record
                .messages
                .iter()
                .position(|entry| entry.id == id && entry.state == store::MessageState::Pending)
        {
            record.messages.remove(index);
            record.updated_ms = store::now_ms();
            return Err(error);
        }
        return Err(format!(
            "persist task journal: {error}; message {id} was accepted in memory, inspect `lan inbox {task}`"
        ));
    }
    notify_changed(shared);
    Ok(id)
}

pub(in crate::local::service) fn inbox(shared: &Shared, task: &str) -> Result<Value, String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    let record = journal
        .get(task)
        .ok_or_else(|| format!("task {task} does not exist"))?;
    let messages: Vec<Value> = record
        .messages
        .iter()
        .map(|message| {
            let (body, body_truncated) = store::bounded_text(message.body.clone(), 4 * 1024);
            let reply = message.reply.as_ref().map(|reply| {
                let (result, result_truncated) =
                    store::bounded_text(reply.result.clone(), 4 * 1024);
                json!({
                    "result": result,
                    "result_truncated": reply.result_truncated || result_truncated,
                    "stopped_by": reply.stopped_by,
                })
            });
            json!({
                "id": message.id,
                "state": message.state,
                "body": body,
                "body_truncated": body_truncated,
                "reply": reply,
            })
        })
        .collect();
    Ok(json!({
        "task": task,
        "messages": messages,
        "next": format!("lan watch {task}"),
    }))
}

pub(in crate::local::service) fn message_payload_for_dispatch(
    shared: &Shared,
    task: &str,
    message_id: &str,
) -> Result<Option<Value>, String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    let record = journal
        .get(task)
        .ok_or_else(|| format!("task {task} does not exist"))?;
    let message = record
        .messages
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
            "next": format!("lan inbox {task}"),
        });
        if !reply.result_truncated {
            payload["result_truncated"] = Value::Null;
        }
        return Ok(Some(payload));
    }
    if message.state == store::MessageState::Delivered
        && let Some(mut terminal) = record.terminal_result()
    {
        let object = terminal
            .as_object_mut()
            .expect("terminal payload is an object");
        object.insert("task".to_string(), json!(task));
        object.insert("message".to_string(), json!(message_id));
        object.insert("next".to_string(), json!(format!("lan inbox {task}")));
        return Ok(Some(terminal));
    }
    Ok(None)
}
