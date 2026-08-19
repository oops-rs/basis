//! Asking a run for a value instead of prose.
//!
//! ADR-0010 calls this the primitive workflows live on: a run that answers in
//! prose composes with nothing, because the next step has to parse English to
//! find out what happened. A run that answers in a declared shape composes with
//! everything.
//!
//! The mechanism is mentra's — one generated terminal tool whose input *is*
//! the answer — and basis's job here is to own the way a caller asks for it.
//! Hence [`OutputSpec`] rather than a re-export of mentra's
//! `TerminalOutputSpec`, for the reason [`Event`](crate::Event) and
//! [`TurnOptions`](super::TurnOptions) exist: basis's surface should not move
//! when mentra's does.
//!
//! What a typed turn may *do* on its way to that answer is the spec's to say,
//! and by default it may do very little: the terminal tool is the only tool the
//! run holds, so the turn cannot read a file, run a command, or reach an MCP
//! server. It shapes what earlier turns on the same run already gathered — the
//! work happens on an ordinary turn, and the type comes after.
//!
//! [`OutputSpec::with_tools`] is the other way: the ordinary toolset stays on
//! the request beside the terminal tool, and one turn reads and then answers.
//! What it gives up is the forcing — a shaping turn is *made* to answer, and a
//! working turn can simply talk instead. Neither is the right default for the
//! other's job, so the choice is the caller's and lives on the spec.
//!
//! The schema is the caller's to write. basis derives nothing — no
//! `schemars`, no proc macro — because a derived schema is a second
//! description of the type that drifts from the first, and because the schema
//! is a *prompt*: its descriptions are what the model reads to decide what to
//! put in each field, and nobody writes those by deriving them.

use mentra::TerminalOutputSpec;
use serde_json::Value;

use super::RunReport;

/// The shape a turn must answer in.
///
/// The three fields describing that shape are all load-bearing, so they arrive
/// together through one constructor: a spec missing any of them would describe
/// a tool the model cannot use. [`with_tools`](Self::with_tools) is a builder
/// because what it says is not part of the shape — it is what the turn wearing
/// that shape is allowed to do on its way to filling it in.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSpec {
    /// What the answering tool is called, as the model sees it — `report` or
    /// `submit_findings` rather than an internal identifier. mentra derives the
    /// tool's actual name from this and makes it unique per call, so a run's
    /// stream will not show this string verbatim.
    pub name: String,
    /// What the tool is for, in the imperative. The model reads this to decide
    /// what a complete answer looks like, so "one entry per problem you saw on
    /// the last turn" is worth more than "returns findings".
    ///
    /// Whether it may also ask for *work* is [`with_tools`](Self::with_tools).
    /// By default the turn holds no tool but this one, so a description asking
    /// the model to go and read something describes work it cannot do —
    /// describe the answer, not a task. A turn that kept its tools has the
    /// opposite exposure, since nothing makes it stop working and answer, and
    /// this is where the condition for stopping belongs: "call this once you
    /// have read every changed file and have nothing further to check".
    pub description: String,
    /// JSON Schema for the answer. The field descriptions in it are read by the
    /// model, not just validated against.
    pub schema: Value,
    /// Whether the turn keeps its ordinary tools while it answers. False unless
    /// [`with_tools`](Self::with_tools) says otherwise.
    pub keeps_tools: bool,
}

impl OutputSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            keeps_tools: false,
        }
    }

    /// Lets the turn work before it answers, instead of only shaping what the
    /// conversation already holds.
    ///
    /// A shaping turn asked for something it has not been told answers from
    /// nothing it looked at: well-formed, empty, and reported as a success. The
    /// way around that has been to spend two turns on every read-then-answer
    /// job, one to gather and one to shape. This spends one — the run keeps the
    /// whole toolset beside the terminal tool, works as many rounds as it
    /// needs, and ends the turn by calling it.
    ///
    /// The cost is that nothing forces the ending. A model that works and then
    /// answers in prose, or that runs out of budget mid-gather, produces no
    /// value at all; [`PreparedRun::output`](super::PreparedRun::output) says
    /// what comes back instead. Two turns also remain the better shape when the
    /// reading should *not* share a context with the answering — one reader per
    /// reviewer, as in `examples/review_workflow.rs`.
    pub fn with_tools(self) -> Self {
        Self {
            keeps_tools: true,
            ..self
        }
    }

    pub(crate) fn into_terminal_spec(self) -> TerminalOutputSpec {
        let Self {
            name,
            description,
            schema,
            keeps_tools,
        } = self;

        let spec = TerminalOutputSpec::new(name, description, schema);
        if keeps_tools { spec.with_tools() } else { spec }
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

    #[test]
    fn a_shaping_turn_is_what_a_caller_gets_without_asking_for_more() {
        // The default is the narrow turn, and it stays that way through the
        // conversion: a spec that never mentioned tools must not acquire them
        // on the way to mentra.
        assert!(!spec().keeps_tools);
        assert!(!spec().into_terminal_spec().keeps_tools);
    }

    #[test]
    fn asking_for_the_toolset_survives_the_trip_to_mentra() {
        // The whole of `with_tools` is one bool that has to arrive: dropped
        // here, the run still answers — from a model that read nothing — and
        // nothing about the result says the tools went missing.
        let terminal = spec().with_tools().into_terminal_spec();

        assert!(terminal.keeps_tools);
        assert_eq!(terminal.tool_name, "report", "the rest of the spec travels");
    }

    #[test]
    fn asking_for_the_toolset_leaves_the_caller_a_spec_to_reuse() {
        // Same fan-out as above, one line later: `with_tools` returns a spec,
        // so twenty runs share one working template as they share a shaping one.
        let template = spec().with_tools();

        assert_eq!(template.clone(), template);
        assert_ne!(template, spec(), "and it is not the shaping spec");
    }
}
