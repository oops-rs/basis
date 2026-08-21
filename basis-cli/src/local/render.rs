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

/// Decorates a raw terminal record with its handle and the follow-up its
/// state admits.
pub(crate) fn decorate_terminal(task: &str, mut payload: Value) -> Value {
    let object = payload
        .as_object_mut()
        .expect("terminal payload is an object");
    let state = object
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    object.insert("task".to_string(), serde_json::json!(task));
    object.insert(
        "next".to_string(),
        serde_json::json!(next_step(&state, task)),
    );
    payload
}

/// The one follow-up a record's state actually admits.
///
/// A hint is a promise, and the state is what decides which promises basis can
/// keep. An agent that holds a terminal record accepts no further messages —
/// `inbox::enqueue` refuses one the moment `terminal.json` exists — so a
/// settled task is never told to continue a conversation it has closed, and a
/// settled *failure* is not told to read an inbox that will be empty when the
/// reason is already on stderr.
fn next_step(state: &str, task: &str) -> String {
    match state {
        // Answered. The journal is the one thing this handle still holds that
        // the terminal does not — and the one thing a redirected stdout, or a
        // scrollback that has moved on, did not keep.
        "succeeded" => format!("basis watch {task}"),
        // No answer was produced and none will be: this handle is spent, and
        // the work continues as a new task rather than as a message to a
        // closed one.
        "failed" | "cancelled" => "basis spawn <PROMPT>".to_string(),
        // Minted and unstarted. It advances exactly when something attaches.
        "resumable" => format!("basis wait {task}"),
        // Still moving: follow it, or read what it has already been sent.
        _ => format!("basis watch {task} or basis inbox {task}"),
    }
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
            "basis: task failed: {}",
            payload["error"].as_str().unwrap_or("unknown failure")
        ),
        "cancelled" => eprintln!("basis: task was cancelled"),
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
            eprintln!(
                "basis: {}",
                event["message"].as_str().unwrap_or("task event")
            )
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

/// One actionable line after a result, on stderr.
///
/// stderr because stdout is the answer and nothing else: `basis "…" > out.md`
/// has to leave a file holding what was asked for, and a hint addressed to
/// whoever is watching the terminal is not that. The `--json` payload carries
/// the same fact as its `next` field, so a script loses nothing.
pub(crate) fn print_hint(payload: &Value) {
    let _ = write_hint(payload, &mut io::stderr());
}

fn write_hint(payload: &Value, err: &mut impl Write) -> io::Result<()> {
    match payload["next"].as_str() {
        Some(next) => writeln!(err, "next: use `{next}`"),
        None => Ok(()),
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
    fn a_terminal_payload_carries_the_handle_it_settled_under() {
        let payload = decorate_terminal("w/t", json!({"state": "succeeded", "result": "done"}));
        assert_eq!(payload["task"], "w/t");
    }

    /// A hint is a promise: the command it names has to work on the task it
    /// names. Two of these were promises basis could not keep — a settled
    /// agent accepts no messages at all (`inbox::enqueue` refuses one the
    /// moment `terminal.json` exists), and pointing a failed run at `watch`
    /// and an empty `inbox` sends someone looking anywhere but at the
    /// failure, which is already on stderr.
    #[test]
    fn each_state_is_told_the_follow_up_that_works_on_it() {
        let hints = [
            ("succeeded", "basis watch w/t"),
            ("failed", "basis spawn <PROMPT>"),
            ("cancelled", "basis spawn <PROMPT>"),
            ("resumable", "basis wait w/t"),
            ("running", "basis watch w/t or basis inbox w/t"),
            ("accepted", "basis watch w/t or basis inbox w/t"),
            ("cancel_requested", "basis watch w/t or basis inbox w/t"),
        ];

        for (state, expected) in hints {
            assert_eq!(
                decorate_terminal("w/t", json!({"state": state}))["next"],
                expected,
                "the follow-up offered for {state}"
            );
        }
    }

    /// stdout is the answer; the hint is not part of it. `basis "…" > out.md`
    /// is the invocation that proves it.
    #[test]
    fn the_hint_goes_to_the_terminal_and_never_into_the_answer() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        write_hint(
            &json!({"state": "succeeded", "next": "basis watch w/t"}),
            &mut err,
        )
        .expect("writing to a vector");
        write_hint(&json!({"state": "succeeded"}), &mut out).expect("writing to a vector");

        assert_eq!(
            String::from_utf8(err).unwrap(),
            "next: use `basis watch w/t`\n"
        );
        assert!(out.is_empty(), "a payload without a next step says nothing");
    }
}
