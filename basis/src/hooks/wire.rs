//! The subprocess binding's encoding: one JSON object in, one JSON object out.
//!
//! What crosses the pipe, and nothing else. The types being encoded live in
//! [`contract`](super::contract) and are shared with the in-process binding —
//! this module is how they reach a program that is not compiled against basis
//! (ADR-0012: one contract, transports are adapters).
//!
//! Versioned the way [`crate::event`] versions its stream, and for the same
//! reason: a hook is written once against a shape and must be able to tell when
//! that shape has moved. Every request carries [`HOOK_SCHEMA_VERSION`] as
//! `hook_schema`, so the first thing a hook can check is whether it still
//! understands basis. An [`Interceptor`](super::Interceptor) needs no such check —
//! it is compiled against this crate and cannot skew — which is the one place
//! the two bindings genuinely differ.
//!
//! # Request (basis → hook, on stdin)
//!
//! ```json
//! {
//!   "hook_schema": 1,
//!   "event": "pre_tool_use",
//!   "workspace": "/repo",
//!   "agent_id": "agent-1",
//!   "tool_call_id": "call-1",
//!   "tool_name": "shell",
//!   "input": {"command": "git push --force"}
//! }
//! ```
//!
//! That is [`HookRequest`] serialized. Field names match
//! [`Event::ToolQueued`](crate::event::Event::ToolQueued) so a hook and a
//! stream consumer describe a tool call the same way. `input` is the parsed
//! tool input when it is valid JSON, and the raw string when it is not — the
//! same rule the event stream follows.
//!
//! A hook declared `"event": "post_tool_use"` is sent the same object with the
//! result on it:
//!
//! ```json
//! {
//!   "hook_schema": 1,
//!   "event": "post_tool_use",
//!   "workspace": "/repo",
//!   "agent_id": "agent-1",
//!   "tool_call_id": "call-1",
//!   "tool_name": "spawn",
//!   "input": {"command": "cat .env"},
//!   "output": "AWS_SECRET_ACCESS_KEY=…",
//!   "is_error": false
//! }
//! ```
//!
//! One envelope rather than two, so a hook that already reads `input` reads
//! `output` beside it. `input` there is the input the tool *ran* with, after
//! any `modify` — what happened rather than what was asked for. `output` is
//! the result as the runtime typed it: a structured result as itself, a text
//! result as a JSON string. Both it and `is_error` are absent before the call,
//! not null, because a call that has not run has no output to be null about.
//!
//! # Response (hook → basis, on stdout)
//!
//! ```json
//! {"decision": "allow"}
//! {"decision": "deny", "reason": "force-push is not allowed in this workspace"}
//! {"decision": "modify", "input": {"command": "git push"}, "reason": "dropped --force"}
//! {"decision": "replace", "output": "[redacted]", "is_error": false, "reason": "a token"}
//! ```
//!
//! `allow` and `deny` mean the same thing at either event, and the other two
//! belong to one each: `modify` rewrites an input that has not been used yet,
//! `replace` rewrites a result that has. A hook that answers with the wrong
//! one for the event it was asked at has not answered, and takes the failure
//! path below — quietly reinterpreting it would give a guard powers it did not
//! ask for.
//!
//! What `allow` and `deny` *do* is where the two events part. Before the call,
//! `deny` stops it. After it, nothing can be stopped — the tool has run and
//! the event stream already carries what it returned — so `deny` shows the
//! model the reason in place of the output, marked as an error, and `allow`
//! lets the output through unchanged.
//!
//! **stdout is the decision; the exit code is only a liveness signal.** Two
//! channels answering one question invites them to disagree, so there is one
//! authority: a hook that exits non-zero has failed regardless of what it
//! printed, and a hook that exits zero has decided whatever it printed.
//!
//! Silence is not an answer. Empty stdout is treated as a failure rather than
//! as consent, because a hook that crashed before printing looks exactly like
//! one that meant to say nothing. Saying yes costs a hook author one `echo`.
//!
//! # Rewriting a call, and rewriting a result
//!
//! `modify` replaces the tool's input, for the cases a veto answers badly:
//! redacting a secret out of an argument, pinning a ref, narrowing an
//! over-broad command. Denying those costs a round trip and often does not
//! converge, because the model is told "no" without being told what would have
//! been acceptable.
//!
//! The rules a modification obeys are the chain's, not this transport's, and
//! they are written down once, on [`HookRunner`](super::HookRunner) — they hold
//! identically for an interceptor, which is the point of there being one
//! contract.
//!
//! `replace` is the same move on the other side of the call, for the question
//! that only the output can answer: a command that succeeded and printed a
//! credential, a result worth annotating rather than hiding. It carries
//! `is_error` because a rewritten result is still a claim about whether the
//! call worked, and omitting it leaves that claim as the tool made it.
//!
//! `reason` is for the audit trail; it does not reach the model, because the
//! model is not being told "no" — it is simply running with different input,
//! or reading a different result.
//!
//! Neither is a way to get past a participant that speaks later: a hook that
//! runs after a rewrite sees the rewrite, and can still refuse it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The types a subprocess hook is handed, re-exported at the path a hook
/// author's code already names them by. They belong to the contract both
/// bindings speak; this module only encodes them.
pub use super::contract::{HookCall, HookRequest};

/// Version of the hook wire format. Bumped when a change would break a hook
/// that reads the current shape.
///
/// `post_tool_use` did not bump it, and the test is not "did anything change"
/// but "can a hook written against the old shape still be right". A hook is
/// told which event it is being asked at, and one that never declared
/// `post_tool_use` is never asked at it: its requests are byte-identical, and
/// its answers mean exactly what they did. The new fields are absent from
/// them, and the new decision is one nobody has to send.
///
/// The version is also the `schema` in `.basis/hooks.json`, which is checked
/// for equality ([`HookConfigError::UnsupportedSchema`](super::HookConfigError::UnsupportedSchema)) —
/// so a bump refuses every hooks file in existence. That is the right answer
/// when the meaning of a field has moved and the wrong one when nothing a hook
/// relies on has.
pub const HOOK_SCHEMA_VERSION: u32 = 1;

/// What a hook answered.
///
/// The wire spelling of [`HookOutcome`](super::HookOutcome), which is what an
/// in-process [`Interceptor`](super::Interceptor) returns directly. Two shapes
/// for one vocabulary, because JSON has no enums and a shell script has no
/// `serde_json::Value` — everything past the parse is shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookResponse {
    /// No objection. `reason` is ignored, and exists so a hook can say why for
    /// a human reading its own logs.
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Block the call. `reason` reaches the model as the tool's error, so it
    /// should read as an explanation, not a status code.
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Run the tool with this input instead of the one the model produced.
    ///
    /// `input` is required: a `modify` without one is a hook that meant to
    /// intervene and did not say how, and quietly running the original would
    /// be the one outcome nobody asked for.
    ///
    /// Only at `pre_tool_use`. After the call there is no input left to
    /// change, and a hook that asks for one is answered as a hook that could
    /// not answer.
    Modify {
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Show the model this result instead of the one the tool returned.
    ///
    /// `output` is required, for the reason `modify`'s `input` is. A string is
    /// a text result and anything else is a structured one, which is the same
    /// distinction the runtime already draws — so a hook that rewrites a line
    /// of text writes a JSON string and gets text back, with no quoting to
    /// undo.
    ///
    /// `is_error` is optional and means "leave it as it was" when omitted: a
    /// hook that redacts a secret out of a failing result has not thereby
    /// declared the call a success. Say it explicitly to change it.
    ///
    /// Only at `post_tool_use`.
    Replace {
        output: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_bare_decision_parses() {
        let allow: HookResponse = serde_json::from_str(r#"{"decision":"allow"}"#).expect("parses");
        let deny: HookResponse =
            serde_json::from_str(r#"{"decision":"deny","reason":"no"}"#).expect("parses");

        assert_eq!(allow, HookResponse::Allow { reason: None });
        assert_eq!(
            deny,
            HookResponse::Deny {
                reason: Some("no".to_string())
            }
        );
    }

    #[test]
    fn unknown_fields_do_not_break_an_older_lan() {
        let response: HookResponse =
            serde_json::from_str(r#"{"decision":"allow","invented_later":true}"#)
                .expect("a hook may say more than basis reads");

        assert_eq!(response, HookResponse::Allow { reason: None });
    }

    #[test]
    fn a_modify_carries_the_replacement_input() {
        let response: HookResponse = serde_json::from_str(
            r#"{"decision":"modify","input":{"command":"ls"},"reason":"narrowed"}"#,
        )
        .expect("parses");

        assert_eq!(
            response,
            HookResponse::Modify {
                input: json!({"command": "ls"}),
                reason: Some("narrowed".to_string()),
            }
        );
    }

    #[test]
    fn a_modify_without_an_input_is_not_a_decision() {
        assert!(
            serde_json::from_str::<HookResponse>(r#"{"decision":"modify"}"#).is_err(),
            "a hook that meant to intervene and did not say how must reach the failure path"
        );
    }

    #[test]
    fn a_decision_lan_does_not_know_is_not_silently_an_allow() {
        assert!(
            serde_json::from_str::<HookResponse>(r#"{"decision":"maybe"}"#).is_err(),
            "an unreadable answer must reach the failure path, not the allow path"
        );
    }

    #[test]
    fn a_replacement_result_carries_what_the_model_should_see() {
        let response: HookResponse = serde_json::from_str(
            r#"{"decision":"replace","output":"[redacted]","is_error":true,"reason":"a token"}"#,
        )
        .expect("parses");

        assert_eq!(
            response,
            HookResponse::Replace {
                output: json!("[redacted]"),
                is_error: Some(true),
                reason: Some("a token".to_string()),
            }
        );
    }

    #[test]
    fn a_replacement_that_says_nothing_about_failure_changes_nothing_about_it() {
        // Omitting `is_error` is not a claim that the call succeeded; it is a
        // hook rewriting the output and leaving the rest alone.
        let response: HookResponse =
            serde_json::from_str(r#"{"decision":"replace","output":{"lines":[]}}"#)
                .expect("parses");

        assert_eq!(
            response,
            HookResponse::Replace {
                output: json!({"lines": []}),
                is_error: None,
                reason: None,
            }
        );
    }

    #[test]
    fn a_replacement_without_an_output_is_not_a_decision() {
        assert!(
            serde_json::from_str::<HookResponse>(r#"{"decision":"replace"}"#).is_err(),
            "a hook that meant to rewrite a result and did not say what to must reach the \
             failure path"
        );
    }

    #[test]
    fn responses_round_trip() {
        let deny = HookResponse::Deny {
            reason: Some("because".to_string()),
        };
        let text = serde_json::to_string(&deny).expect("serializes");
        let back: HookResponse = serde_json::from_str(&text).expect("deserializes");

        assert_eq!(deny, back);
    }
}
