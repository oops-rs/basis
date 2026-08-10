//! The hook wire contract: one JSON object in, one JSON object out.
//!
//! Versioned the way [`crate::event`] versions its stream, and for the same
//! reason: a hook is written once against a shape and must be able to tell when
//! that shape has moved. Every request carries [`HOOK_SCHEMA_VERSION`] as
//! `hook_schema`, so the first thing a hook can check is whether it still
//! understands lan.
//!
//! # Request (lan → hook, on stdin)
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
//! Field names match [`Event::ToolQueued`](crate::event::Event::ToolQueued) so
//! a hook and a stream consumer describe a tool call the same way. `input` is
//! the parsed tool input when it is valid JSON, and the raw string when it is
//! not — the same rule the event stream follows.
//!
//! # Response (hook → lan, on stdout)
//!
//! ```json
//! {"decision": "allow"}
//! {"decision": "deny", "reason": "force-push is not allowed in this workspace"}
//! {"decision": "modify", "input": {"command": "git push"}, "reason": "dropped --force"}
//! ```
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
//! # Modifying a call
//!
//! `modify` replaces the tool's input, for the cases a veto answers badly:
//! redacting a secret out of an argument, pinning a ref, narrowing an
//! over-broad command. Denying those costs a round trip and often does not
//! converge, because the model is told "no" without being told what would have
//! been acceptable.
//!
//! Three rules, matching what mentra does across its own hooks so that lan's
//! chain and mentra's behave identically:
//!
//! - **Modifications compose.** Each hook receives `input` as its predecessors
//!   left it, never the original.
//! - **A later hook can still deny.** `modify` is not a way to route a call
//!   past a hook that runs after it.
//! - **A modify lan cannot use blocks the call**, rather than falling back to
//!   the original — running the original would silently ignore a hook that
//!   believed it had intervened. That covers a `modify` with no `input` at all
//!   and an `input` that is not a JSON object, since a tool's input never is
//!   anything else.
//!
//! `reason` is for the audit trail; it does not reach the model, because the
//! model is not being told "no" — it is simply running with different input.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::HookEvent;

/// Version of the hook wire format. Bumped when a change would break a hook
/// that reads the current shape.
pub const HOOK_SCHEMA_VERSION: u32 = 1;

/// The tool call a hook is asked about.
///
/// lan's own type rather than a re-export of mentra's `PreExecutionContext`,
/// for the same reason [`Event`](crate::event::Event) and
/// [`TurnOptions`](crate::run::TurnOptions) are lan's own: lan owns its
/// surface. It is field-for-field what mentra's pre-execution context carries,
/// so the adapter that will bridge them is a move, not a translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCall {
    pub agent_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    /// The tool's input as the runtime hands it over: JSON text.
    pub input_json: String,
}

impl HookCall {
    pub fn new(
        agent_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_call_id: impl Into<String>,
        input_json: impl Into<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            tool_name: tool_name.into(),
            tool_call_id: tool_call_id.into(),
            input_json: input_json.into(),
        }
    }
}

/// What lan asks a hook about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookRequest {
    pub hook_schema: u32,
    pub event: HookEvent,
    /// The workspace root the run is scoped to, and the hook's working
    /// directory.
    pub workspace: PathBuf,
    pub agent_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Parsed tool input when it is valid JSON, else the raw string.
    pub input: Value,
}

impl HookRequest {
    /// Builds a request from the call the runtime is about to make.
    ///
    /// The call carries `input_json` as a string. Parsing it here rather than
    /// passing it through means a hook reads `.input.command` instead of
    /// decoding a nested document; when it is not valid JSON there is nothing
    /// to parse and the raw string is carried instead, which is what
    /// [`Event::ToolQueued`](crate::event::Event::ToolQueued) does with the
    /// same value.
    ///
    /// The call has no working directory of its own — mentra's context does not
    /// carry one — so `workspace` is the root lan scoped the run to.
    pub fn from_call(event: HookEvent, workspace: &Path, call: &HookCall) -> Self {
        Self {
            hook_schema: HOOK_SCHEMA_VERSION,
            event,
            workspace: workspace.to_path_buf(),
            agent_id: call.agent_id.clone(),
            tool_call_id: call.tool_call_id.clone(),
            tool_name: call.tool_name.clone(),
            input: serde_json::from_str(&call.input_json)
                .unwrap_or_else(|_| Value::String(call.input_json.clone())),
        }
    }

    /// The same request with a different tool input.
    ///
    /// What threading a modification through the chain is made of: the next
    /// hook is asked about the call as the previous one left it.
    pub fn with_input(self, input: Value) -> Self {
        Self { input, ..self }
    }
}

/// What a hook answered.
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
    Modify {
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(input_json: &str) -> HookCall {
        HookCall::new("agent-1", "shell", "call-1", input_json)
    }

    #[test]
    fn a_request_carries_the_schema_version_first() {
        let request = HookRequest::from_call(
            HookEvent::PreToolUse,
            Path::new("/repo"),
            &context(r#"{"command":"ls"}"#),
        );
        let json = serde_json::to_value(&request).expect("serializes");

        assert_eq!(json["hook_schema"], HOOK_SCHEMA_VERSION);
        assert_eq!(json["event"], "pre_tool_use");
        assert_eq!(json["workspace"], "/repo");
        assert_eq!(json["tool_name"], "shell");
        assert_eq!(json["tool_call_id"], "call-1");
        assert_eq!(json["input"]["command"], "ls");
    }

    #[test]
    fn input_that_is_not_json_is_carried_as_the_raw_string() {
        let request =
            HookRequest::from_call(HookEvent::PreToolUse, Path::new("/repo"), &context("ls -l"));

        assert_eq!(request.input, json!("ls -l"));
    }

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
                .expect("a hook may say more than lan reads");

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
    fn a_request_can_be_rebuilt_around_a_new_input() {
        let original = HookRequest::from_call(
            HookEvent::PreToolUse,
            Path::new("/repo"),
            &context(r#"{"command":"rm -rf /"}"#),
        );

        let next = original.clone().with_input(json!({"command": "ls"}));

        assert_eq!(
            original.input,
            json!({"command": "rm -rf /"}),
            "the original must be untouched"
        );
        assert_eq!(next.input, json!({"command": "ls"}));
        assert_eq!(next.tool_call_id, original.tool_call_id);
    }

    #[test]
    fn a_decision_lan_does_not_know_is_not_silently_an_allow() {
        assert!(
            serde_json::from_str::<HookResponse>(r#"{"decision":"maybe"}"#).is_err(),
            "an unreadable answer must reach the failure path, not the allow path"
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
