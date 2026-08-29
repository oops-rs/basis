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

use mentra::{TerminalOutputReservation as MentraOutputReservation, TerminalOutputSpec};
use serde_json::Value;

use super::{RunError, RunReport};

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

    /// Reserve the exact generated output tool for one future validated run.
    ///
    /// Reservation has no runtime side effect. The generated name is exposed
    /// so a host can identify protocol events before the run starts.
    pub fn reserve(self) -> OutputReservation {
        OutputReservation {
            inner: self.into_terminal_spec().reserve(),
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

/// One generated output tool reserved for a validated run.
#[derive(Debug)]
pub struct OutputReservation {
    inner: MentraOutputReservation,
}

impl OutputReservation {
    /// The exact generated tool name the provider will see.
    pub fn tool_name(&self) -> &str {
        self.inner.tool_name()
    }

    pub(crate) fn into_inner(self) -> MentraOutputReservation {
        self.inner
    }
}

/// A host's decision over one schema-shaped candidate before termination.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputDecision {
    /// Commit this (possibly normalized) JSON value and terminate the run.
    Accept(Value),
    /// Return this model-visible tool error and continue the same run.
    Reject(String),
}

/// What a validated typed attempt produced.
#[derive(Debug)]
pub enum OutputAttempt<T> {
    /// The validator accepted a value and it decoded as `T`.
    Accepted(T),
    /// The validator accepted JSON that did not decode as `T`.
    Mismatch(serde_json::Error),
    /// The run ended without an accepted terminal value.
    Missing,
}

/// A validated typed attempt alongside the ordinary run report.
#[derive(Debug)]
pub struct OutputAttemptReport<T, S> {
    pub output: OutputAttempt<T>,
    pub report: RunReport<S>,
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

/// A typed turn that produced no value — and everything it reported anyway.
///
/// The turn still happened. It spent tokens, it may have been ended by a bound
/// rather than by the work, and it wrote a whole stream into the sink the
/// caller lent it. A typed turn used to build that [`RunReport`] and then drop
/// it on the floor, returning a bare [`RunError`]; a caller charging runs
/// against a shared allowance, or deciding between re-asking with a clearer
/// schema and backing off, was left to reconstruct from the event log what the
/// run had already written down. Library first (ADR-0003) is judged by the
/// in-process Rust consumer, and this is the shape that consumer needs: the
/// error *and* the account.
///
/// The error is deliberately the same [`RunError`] the turn returned before,
/// not a second vocabulary for the same failure — two names for one thing is
/// how a host ends up matching on the wrong one. [`From`] hands it straight
/// back, so a caller that wants nothing but the error writes
/// `.await.map_err(RunError::from)?` and receives exactly what it always did:
/// [`RunError::OutputMismatch`] for an answer `T` refused,
/// [`RunError::Runtime`] for a turn that failed or never called the terminal
/// tool.
///
/// Distinct from [`RunFailure`](crate::RunFailure), which is mentra's own error
/// as retained *inside* a report.
#[derive(Debug)]
pub struct OutputFailure<S> {
    /// Why there is no value, in the same terms as before this type existed.
    ///
    /// On a runtime failure this is where mentra's original error lives, rather
    /// than in [`RunReport::failure`]: a `RuntimeError` is not `Clone` and
    /// cannot be in two places, and the error is where a caller reaching for
    /// `?` looks. The validated path
    /// ([`output_parts_validated_with_options`](super::PreparedRun::output_parts_validated_with_options))
    /// returns `Ok`, so there the report is the only home and keeps it
    /// (ADR-0024 §4).
    pub error: RunError,
    /// Everything the turn reported before it came up empty, sink included.
    ///
    /// `None` only when there was no turn to report on — an empty prompt, an
    /// option set that cannot be drawn, a sink that refused a write, a
    /// forwarding task that did not come back. Every failure *of the turn
    /// itself* carries its report.
    pub report: Option<RunReport<S>>,
}

impl<S> From<RunError> for OutputFailure<S> {
    /// A failure with no turn behind it. See [`report`](OutputFailure::report).
    fn from(error: RunError) -> Self {
        Self {
            error,
            report: None,
        }
    }
}

impl<S> From<OutputFailure<S>> for RunError {
    /// The error alone, worded exactly as the typed turn worded it before it
    /// carried a report. The report — and with it the sink — is dropped here,
    /// which is the whole cost of `?` and the reason the richer type is what
    /// the turn returns.
    fn from(failure: OutputFailure<S>) -> Self {
        failure.error
    }
}

impl<S> std::fmt::Display for OutputFailure<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl<S: std::fmt::Debug> std::error::Error for OutputFailure<S> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
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

    fn a_mismatch() -> serde_json::Error {
        serde_json::from_str::<u32>("\"not a number\"").expect_err("a mismatch")
    }

    #[test]
    fn the_error_a_caller_reaches_through_question_mark_is_the_one_it_always_got() {
        // The whole migration story: a host that only wants the error writes
        // `?` and must receive the same variant it matched on before the
        // report came along. A conversion that re-labelled here would break
        // every such match silently, which is the failure mode this shape was
        // chosen to avoid.
        let failure: OutputFailure<()> = OutputFailure {
            error: RunError::OutputMismatch(a_mismatch()),
            report: None,
        };

        assert!(matches!(
            RunError::from(failure),
            RunError::OutputMismatch(_)
        ));
    }

    #[test]
    fn a_failure_with_no_turn_behind_it_carries_no_report() {
        // Setup refusals — an empty prompt, an undrawable option set — happen
        // before there is a turn to account for, and must say so rather than
        // inventing an empty report for a run that never started.
        let failure: OutputFailure<()> = RunError::EmptyPrompt.into();

        assert!(failure.report.is_none());
        assert!(matches!(failure.error, RunError::EmptyPrompt));
    }

    #[test]
    fn a_failure_reads_as_the_error_it_carries() {
        // `Display` is the error's, not a wrapper's: a host logging the failure
        // should not have to learn a second wording for a failure it already
        // knows how to print.
        let error = RunError::OutputMismatch(a_mismatch());
        let message = error.to_string();
        let failure: OutputFailure<()> = OutputFailure {
            error,
            report: None,
        };

        assert_eq!(failure.to_string(), message);
    }
}
