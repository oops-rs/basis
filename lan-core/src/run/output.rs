//! Asking a run for a value instead of prose.
//!
//! ADR-0010 calls this the primitive workflows live on: a run that answers in
//! prose composes with nothing, because the next step has to parse English to
//! find out what happened. A run that answers in a declared shape composes with
//! everything.
//!
//! The mechanism is mentra's — one generated terminal tool, forced, whose input
//! *is* the answer — and lan's job here is to own the way a caller asks for it.
//! Hence [`OutputSpec`] rather than a re-export of mentra's
//! `TerminalOutputSpec`, for the reason [`Event`](crate::Event) and
//! [`TurnOptions`](super::TurnOptions) exist: lan's surface should not move
//! when mentra's does.
//!
//! One consequence of that mechanism shapes every use of it: while a run is
//! answering into a schema, the terminal tool is the *only* tool it holds.
//! A typed turn cannot read a file, run a command, or reach an MCP server —
//! it shapes what earlier turns on the same run already gathered. The work
//! happens on an ordinary turn; the type comes after.
//!
//! The schema is the caller's to write. lan derives nothing — no
//! `schemars`, no proc macro — because a derived schema is a second
//! description of the type that drifts from the first, and because the schema
//! is a *prompt*: its descriptions are what the model reads to decide what to
//! put in each field, and nobody writes those by deriving them.

use mentra::TerminalOutputSpec;
use serde_json::Value;

use super::RunReport;

/// The shape a turn must answer in.
///
/// All three fields are load-bearing, so there is a single constructor and no
/// `with_*` builders: a spec missing any of them would describe a tool the
/// model cannot use.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSpec {
    /// What the answering tool is called, as the model sees it — `report` or
    /// `submit_findings` rather than an internal identifier. mentra derives the
    /// tool's actual name from this and makes it unique per call, so a run's
    /// stream will not show this string verbatim.
    pub name: String,
    /// What the tool is for, in the imperative. The model reads this to decide
    /// what a complete answer looks like, so "one entry per problem you saw on
    /// the last turn" is worth more than "returns findings". It cannot ask for
    /// work — the typed turn holds no tool but this one — so describe the
    /// answer, not a task.
    pub description: String,
    /// JSON Schema for the answer. The field descriptions in it are read by the
    /// model, not just validated against.
    pub schema: Value,
}

impl OutputSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
        }
    }

    pub(crate) fn into_terminal_spec(self) -> TerminalOutputSpec {
        TerminalOutputSpec::new(self.name, self.description, self.schema)
    }
}

/// A typed turn's answer, alongside everything a plain turn reports.
///
/// Composition rather than a second report type: a typed turn is an ordinary
/// turn with one extra thing to say, and a caller still needs the outcome, the
/// usage and the sink — a fan-out that charges runs against a shared budget
/// needs them *most* on the typed path. Keeping [`RunReport`] whole in here is
/// what stops the two from drifting into two slightly different reports.
#[derive(Debug)]
pub struct OutputReport<T, S> {
    /// What the model committed through the terminal tool.
    pub value: T,
    /// Everything the same turn would have reported without a type on it.
    pub report: RunReport<S>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> OutputSpec {
        OutputSpec::new(
            "report",
            "the verdict you reached on the last turn",
            json!({
                "type": "object",
                "properties": {
                    "verdict": { "type": "string", "description": "ship or hold" }
                },
                "required": ["verdict"]
            }),
        )
    }

    #[test]
    fn a_spec_reaches_mentra_as_the_caller_wrote_it() {
        let terminal = spec().into_terminal_spec();

        assert_eq!(terminal.tool_name, "report");
        assert_eq!(
            terminal.description,
            "the verdict you reached on the last turn"
        );
        // The schema travels whole, descriptions included: they are what the
        // model reads to decide what belongs in the field.
        assert_eq!(
            terminal.schema["properties"]["verdict"]["description"],
            "ship or hold"
        );
    }

    #[test]
    fn a_spec_is_a_value_a_caller_can_keep_and_reuse() {
        // A fan-out asks twenty runs for the same shape, so a spec has to
        // survive being cloned rather than being consumed by the first send.
        let template = spec();

        assert_eq!(template.clone(), template);
    }
}
