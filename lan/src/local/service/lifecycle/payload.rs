//! The JSON payloads a terminal or accepted task reports to its caller.

use serde_json::{Value, json};

use crate::local::service::Shared;

pub(in crate::local::service) fn terminal_payload(
    shared: &Shared,
    task: &str,
) -> Result<Option<Value>, String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    let record = journal
        .get(task)
        .ok_or_else(|| format!("task {task} does not exist"))?;
    let Some(payload) = record.terminal_result() else {
        return Ok(None);
    };
    Ok(Some(decorate_terminal(task, payload)))
}

pub(super) fn decorate_terminal(task: &str, mut payload: Value) -> Value {
    let object = payload
        .as_object_mut()
        .expect("terminal payload is an object");
    object.insert("task".to_string(), json!(task));
    object.insert(
        "next".to_string(),
        json!(format!("lan watch {task} or lan inbox {task}")),
    );
    payload
}

pub(in crate::local::service) fn accepted_payload(task: &str) -> Value {
    json!({
        "task": task,
        "state": "running",
        "next": format!("lan wait {task}"),
    })
}
