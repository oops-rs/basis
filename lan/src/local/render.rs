//! How lifecycle payloads and events reach a person or a script.
//!
//! The JSON shapes and the exit-code mapping are contract (ADR-0015/0017);
//! `--json` prints payloads verbatim and the prose is derived from the same
//! fields, so the code a script reads cannot depend on the renderer.

use std::{
    io::{self, Write},
    process::ExitCode,
};

use serde_json::Value;

use crate::exit::{EXIT_BOUNDED, EXIT_FAILED, EXIT_OK};

use super::error::ClientError;

/// Decorates a raw terminal record with its handle and follow-up commands,
/// exactly as the daemon's payloads carried them.
pub(crate) fn decorate_terminal(task: &str, mut payload: Value) -> Value {
    let object = payload
        .as_object_mut()
        .expect("terminal payload is an object");
    object.insert("task".to_string(), serde_json::json!(task));
    object.insert(
        "next".to_string(),
        serde_json::json!(format!("lan watch {task} or lan inbox {task}")),
    );
    payload
}

pub(crate) fn render_result(payload: &Value, structured: bool) -> Result<ExitCode, ClientError> {
    if structured {
        println!("{payload}");
        return Ok(ExitCode::from(result_code(payload)));
    }

    match payload["state"].as_str().unwrap_or("unknown") {
        "running" | "resumable" | "accepted" | "cancel_requested" => {
            if let Some(task) = payload["task"].as_str() {
                let state = payload["state"].as_str().unwrap_or("unknown");
                if state == "accepted"
                    && let Some(message) = payload["message"].as_str()
                {
                    println!("task {task}: accepted message {message}");
                } else {
                    println!("task {task}: {state}");
                }
            }
        }
        "succeeded" => {
            if let Some(result) = payload["result"].as_str()
                && !result.is_empty()
            {
                print!("{result}");
                if !result.ends_with('\n') {
                    println!();
                }
            }
        }
        "failed" => eprintln!(
            "lan: task failed: {}",
            payload["error"].as_str().unwrap_or("unknown failure")
        ),
        "cancelled" => eprintln!("lan: task was cancelled"),
        state => println!("task state: {state}"),
    }
    print_hint(payload);
    io::stdout()
        .flush()
        .map_err(|error| ClientError::new(format!("flush task output: {error}")))?;
    Ok(ExitCode::from(result_code(payload)))
}

pub(crate) fn render_event(record: &Value, structured: bool) -> Result<(), ClientError> {
    if structured {
        println!("{record}");
        return Ok(());
    }
    let event = &record["event"];
    match event["type"].as_str().unwrap_or_default() {
        "assistant_delta" => {
            print!("{}", event["text"].as_str().unwrap_or_default());
            io::stdout()
                .flush()
                .map_err(|error| ClientError::new(format!("flush task progress: {error}")))?;
        }
        "tool_started" => eprintln!("  · {}", event["tool_name"].as_str().unwrap_or("tool")),
        "notice" | "error" => {
            eprintln!("lan: {}", event["message"].as_str().unwrap_or("task event"))
        }
        "run_finished" => println!(),
        _ => {}
    }
    Ok(())
}

pub(crate) fn result_code(payload: &Value) -> u8 {
    if !payload["stopped_by"].is_null() {
        return EXIT_BOUNDED;
    }
    match payload["state"].as_str() {
        Some("running" | "resumable" | "accepted" | "cancel_requested" | "succeeded") => EXIT_OK,
        _ => EXIT_FAILED,
    }
}

pub(crate) fn print_hint(payload: &Value) {
    if let Some(next) = payload["next"].as_str() {
        println!("next: use `{next}`");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terminal_codes_do_not_depend_on_rendering() {
        assert_eq!(result_code(&json!({"state": "succeeded"})), EXIT_OK);
        assert_eq!(result_code(&json!({"state": "failed"})), EXIT_FAILED);
        assert_eq!(result_code(&json!({"state": "resumable"})), EXIT_OK);
        assert_eq!(
            result_code(&json!({"state": "failed", "stopped_by": "deadline"})),
            EXIT_BOUNDED
        );
    }

    #[test]
    fn a_terminal_payload_names_concrete_follow_up_commands() {
        let payload = decorate_terminal("w/t", json!({"state": "succeeded", "result": "done"}));
        assert_eq!(payload["task"], "w/t");
        assert_eq!(payload["next"], "lan watch w/t or lan inbox w/t");
    }
}
