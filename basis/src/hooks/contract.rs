//! The interception contract: what basis asks, and what an answer may say.
//!
//! One contract per seam, per ADR-0012 — so these types are what *both*
//! bindings speak. The subprocess binding encodes them as JSON ([`wire`](super::wire));
//! the in-process binding receives them directly ([`Interceptor`](super::Interceptor)).
//! Nothing here knows which one it is talking to, which is the whole point: a
//! guard written as a shell script and a guard written as Rust have the same
//! powers and the same vocabulary.
//!
//! The vocabulary is four words, and which of them are on offer depends on
//! when a participant was asked. Before the call: **allow**, **deny with a
//! reason**, **modify with a replacement input**. After it: **allow** again —
//! the result stands — **deny**, which now means the model is shown the reason
//! instead of the output, and **replace with a different output**. It is
//! mentra's `HookDecision` and `ResultDecision` between them, because basis's
//! job at this seam is to own the ordering rather than to invent a richer
//! answer than the runtime can carry.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::wire::HOOK_SCHEMA_VERSION;

/// When a participant is consulted.
///
/// One variant per interception point mentra offers. It is spelled out rather
/// than assumed so a config file says when a hook fires — and the default is
/// [`PreToolUse`](Self::PreToolUse), so every hooks file written before there
/// was a second point still means what it said.
///
/// The two are not two halves of one question. Before the call, the whole
/// question is whether it should happen and in what form; afterwards it has
/// happened, and the only thing left to decide is what the model is shown. A
/// hook that wants both says so twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before the tool runs — and, since mentra 0.24, before the schema check
    /// and the [`ToolAuthorizer`](mentra::tool::ToolAuthorizer) too.
    ///
    /// So a hook is asked about *every* registered call, including ones the
    /// approver goes on to refuse: being consulted is not being approved, and
    /// a participant with side effects of its own must not read it that way.
    /// What it may take from the ordering is that a rewrite it returns is what
    /// the approver is then asked about.
    #[default]
    PreToolUse,
    /// After the tool ran, before the model is shown what it returned.
    ///
    /// Too late to stop anything: the side effects have happened and the
    /// unmodified result has already reached the event stream. What a
    /// participant still has is the output, which is the only place some
    /// questions can be answered at all — whether a grep pulled a credential
    /// out of a file nobody meant to expose, whether stderr carries an
    /// internal hostname.
    PostToolUse,
}

/// The tool call the runtime is about to make.
///
/// basis's own type rather than a re-export of mentra's `PreExecutionContext`,
/// for the same reason [`Event`](crate::event::Event) and
/// [`TurnOptions`](crate::run::TurnOptions) are basis's own: basis owns its
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

/// The call as basis puts it to one participant.
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
    ///
    /// On a [`PostToolUse`](HookEvent::PostToolUse) request this is the input
    /// the tool *ran* with, after any earlier modification — what happened,
    /// not what the model asked for.
    pub input: Value,
    /// What the tool returned, on a [`PostToolUse`](HookEvent::PostToolUse)
    /// request and on no other.
    ///
    /// Absent rather than null before the call, because a call that has not
    /// run has no output to be null about. A structured result arrives as
    /// itself and a text result as a JSON string — the runtime already knows
    /// which it produced, so nothing here guesses by re-parsing text that
    /// happens to look like JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Whether the tool reported failure, beside the output it reported it
    /// with. Absent for the same reason.
    ///
    /// Named as [`Event::ToolCompleted`](crate::event::Event::ToolCompleted)
    /// names it, so a hook and a stream consumer describe one result the same
    /// way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
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
    /// `workspace` is the root basis scoped the run to, and it is also the
    /// call's working directory: mentra's `PreExecutionContext` carries one
    /// (`working_directory`, the agent's `base_dir`), which is the same path
    /// the open handed this runner — so the two are one by construction, and a
    /// second field would only invite them to disagree. The runner's own copy
    /// is what is reported, so a hook is told where its *workspace* is even
    /// when a call arrives from an agent scoped somewhere else.
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
            output: None,
            is_error: None,
        }
    }

    /// Builds a request from the call the runtime has just made, and what it
    /// produced.
    ///
    /// The event is [`PostToolUse`](HookEvent::PostToolUse) and not a
    /// parameter: an output is the one thing a request can carry that says
    /// which seam it came from, so the two cannot be set to disagree.
    pub fn from_result(workspace: &Path, call: &HookCall, output: Value, is_error: bool) -> Self {
        Self {
            output: Some(output),
            is_error: Some(is_error),
            ..Self::from_call(HookEvent::PostToolUse, workspace, call)
        }
    }

    /// The same request with a different tool input.
    ///
    /// What threading a modification through the chain is made of: the next
    /// participant is asked about the call as the previous one left it.
    pub fn with_input(self, input: Value) -> Self {
        Self { input, ..self }
    }

    /// The same request with a different result.
    ///
    /// The after-the-call twin of [`with_input`](Self::with_input), and the
    /// same property: a participant judges the result as its predecessors left
    /// it, never the one the tool returned.
    pub fn with_output(self, output: Value, is_error: bool) -> Self {
        Self {
            output: Some(output),
            is_error: Some(is_error),
            ..self
        }
    }
}

/// What the chain decided about a call.
///
/// basis's own type, shaped like mentra's `HookDecision` so the adapter bridging
/// them is a `match` and nothing more — but carrying the replacement input as
/// parsed JSON rather than a string, because an in-process host reading this
/// should not have to parse a document back out of it.
///
/// Also what a single [`Interceptor`](super::Interceptor) answers with. Two
/// bindings, one vocabulary: an interceptor's `Deny` and a hook's `deny` are
/// the same refusal, told to the model the same way.
///
/// One enum for both events rather than two, because three of the four answers
/// mean the same thing at either — and the one that does not, [`Modify`](Self::Modify),
/// is refused by name after the call rather than reinterpreted into something
/// nobody asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum HookOutcome {
    /// No participant objected. Not the same as "nobody was asked".
    ///
    /// After the call this is *keep*: the result reaches the model as the tool
    /// produced it.
    Allow,
    /// Blocked, with a reason meant to be read — by the model, which sees it as
    /// the tool's error, and by whoever has to work out what happened.
    ///
    /// After the call nothing can be blocked, so a refusal is the strongest
    /// thing still available: the model is shown this reason instead of the
    /// output, marked as an error. What ran, ran — the event stream already
    /// carries what it returned.
    Deny(String),
    /// Run the tool with this input instead.
    ///
    /// `input` is what the chain left behind after every modification, and
    /// `reason` names each participant that changed something — "the input is
    /// not what the model wrote" is exactly what an audit trail is for.
    ///
    /// Before the call only.
    Modify {
        input: Value,
        reason: Option<String>,
    },
    /// Show the model this result instead of the one the tool returned.
    ///
    /// `is_error` is carried beside the output because the two are one
    /// statement: a replacement that redacts a secret out of a successful
    /// result is still a success, and one that turns an output into a refusal
    /// is not. `reason` names every participant that changed something, as
    /// [`Modify`](Self::Modify)'s does, and reaches the audit trail rather
    /// than the model — the model is not being told "no", it is simply being
    /// shown a different result.
    ///
    /// After the call only.
    Replace {
        output: Value,
        is_error: bool,
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

    #[test]
    fn a_result_request_carries_the_call_that_produced_it() {
        // Same envelope, two more fields: a hook that already reads `input`
        // reads `output` beside it rather than learning a second shape.
        let request = HookRequest::from_result(
            Path::new("/repo"),
            &call(r#"{"command":"cat .env"}"#),
            json!("TOKEN=hunter2"),
            false,
        );
        let json = serde_json::to_value(&request).expect("serializes");

        assert_eq!(json["hook_schema"], HOOK_SCHEMA_VERSION);
        assert_eq!(json["event"], "post_tool_use");
        assert_eq!(json["tool_call_id"], "call-1");
        assert_eq!(json["input"]["command"], "cat .env");
        assert_eq!(json["output"], "TOKEN=hunter2");
        assert_eq!(json["is_error"], false);
    }

    #[test]
    fn a_call_that_has_not_run_carries_no_result_at_all() {
        // Absent rather than null: a hook that tests for `output` must not
        // find one on a call whose output does not exist yet.
        let request = HookRequest::from_call(
            HookEvent::PreToolUse,
            Path::new("/repo"),
            &call(r#"{"command":"ls"}"#),
        );
        let json = serde_json::to_value(&request).expect("serializes");

        assert_eq!(request.output, None);
        assert_eq!(request.is_error, None);
        assert!(json.get("output").is_none(), "got {json}");
        assert!(json.get("is_error").is_none(), "got {json}");
    }

    #[test]
    fn a_request_can_be_rebuilt_around_a_new_result() {
        let original = HookRequest::from_result(
            Path::new("/repo"),
            &call("{}"),
            json!("TOKEN=hunter2"),
            false,
        );

        let next = original.clone().with_output(json!("[redacted]"), true);

        assert_eq!(
            original.output,
            Some(json!("TOKEN=hunter2")),
            "the original must be untouched"
        );
        assert_eq!(original.is_error, Some(false));
        assert_eq!(next.output, Some(json!("[redacted]")));
        assert_eq!(next.is_error, Some(true));
        assert_eq!(next.tool_call_id, original.tool_call_id);
    }
}
