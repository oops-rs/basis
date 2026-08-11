//! The interception contract: what lan asks, and what an answer may say.
//!
//! One contract per seam, per ADR-0012 — so these types are what *both*
//! bindings speak. The subprocess binding encodes them as JSON ([`wire`](super::wire));
//! the in-process binding receives them directly ([`Interceptor`](super::Interceptor)).
//! Nothing here knows which one it is talking to, which is the whole point: a
//! guard written as a shell script and a guard written as Rust have the same
//! powers and the same vocabulary.
//!
//! The vocabulary is three words: **allow**, **deny with a reason**, and
//! **modify with a replacement input and a reason**. It is mentra's
//! `HookDecision` verbatim, because lan's job at this seam is to own the
//! ordering rather than to invent a richer answer than the runtime can carry.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::wire::HOOK_SCHEMA_VERSION;

/// When a participant is consulted.
///
/// One variant, because mentra offers one interception point. It is spelled out
/// rather than assumed so a config file says when it fires, and so a second
/// point can arrive without changing the shape of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// After authorization, before the tool runs.
    #[default]
    PreToolUse,
}

/// The tool call the runtime is about to make.
///
/// lan's own type rather than a re-export of mentra's `PreExecutionContext`,
/// for the same reason [`Event`](crate::event::Event) and
/// [`TurnOptions`](crate::run::TurnOptions) are lan's own: lan owns its
/// surface. It is field-for-field what mentra's pre-execution context carries,
/// so the adapter that bridges them is a move, not a translation.
///
/// This is the *runtime's* view, with the input still JSON text. What a
/// participant is asked about is a [`HookRequest`], where the input is parsed.
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

/// The call as lan puts it to one participant.
///
/// The same struct reaches both bindings: a subprocess hook reads it as the
/// JSON object on its stdin, and an [`Interceptor`](super::Interceptor) is
/// handed a reference to it. That identity is what "one contract" means
/// concretely — there is no in-process shape a hook cannot see and no wire
/// field an interceptor is denied.
///
/// It carries [`HOOK_SCHEMA_VERSION`] for the binding that needs it. A
/// subprocess hook is compiled against nothing and must be able to tell when
/// the shape has moved; an interceptor is compiled against this crate and
/// cannot skew, so the field is simply true for it rather than useful.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookRequest {
    pub hook_schema: u32,
    pub event: HookEvent,
    /// The workspace root the run is scoped to, and a subprocess hook's working
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
    /// passing it through means a participant reads `.input.command` instead of
    /// decoding a nested document — which matters to a shell script and to a
    /// Rust interceptor alike; when it is not valid JSON there is nothing to
    /// parse and the raw string is carried instead, which is what
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
    /// participant is asked about the call as the previous one left it.
    pub fn with_input(self, input: Value) -> Self {
        Self { input, ..self }
    }
}

/// What the chain decided about a call.
///
/// lan's own type, shaped like mentra's `HookDecision` so the adapter bridging
/// them is a `match` and nothing more — but carrying the replacement input as
/// parsed JSON rather than a string, because an in-process host reading this
/// should not have to parse a document back out of it.
///
/// Also what a single [`Interceptor`](super::Interceptor) answers with. Two
/// bindings, one vocabulary: an interceptor's `Deny` and a hook's `deny` are
/// the same refusal, told to the model the same way.
#[derive(Debug, Clone, PartialEq)]
pub enum HookOutcome {
    /// No participant objected. Not the same as "nobody was asked".
    Allow,
    /// Blocked, with a reason meant to be read — by the model, which sees it as
    /// the tool's error, and by whoever has to work out what happened.
    Deny(String),
    /// Run the tool with this input instead.
    ///
    /// `input` is what the chain left behind after every modification, and
    /// `reason` names each participant that changed something — "the input is
    /// not what the model wrote" is exactly what an audit trail is for.
    Modify {
        input: Value,
        reason: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(input_json: &str) -> HookCall {
        HookCall::new("agent-1", "shell", "call-1", input_json)
    }

    #[test]
    fn a_request_carries_the_schema_version_first() {
        let request = HookRequest::from_call(
            HookEvent::PreToolUse,
            Path::new("/repo"),
            &call(r#"{"command":"ls"}"#),
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
            HookRequest::from_call(HookEvent::PreToolUse, Path::new("/repo"), &call("ls -l"));

        assert_eq!(request.input, json!("ls -l"));
    }

    #[test]
    fn a_request_can_be_rebuilt_around_a_new_input() {
        let original = HookRequest::from_call(
            HookEvent::PreToolUse,
            Path::new("/repo"),
            &call(r#"{"command":"rm -rf /"}"#),
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
}
