//! The turn whose answer is a value rather than prose.
//!
//! Split from [`prepared`](super) for the parent's size, and along the seam
//! that was already there: everything here is ADR-0010's structured output —
//! one terminal tool, one deserialization, and the error distinction basis
//! draws that mentra does not. The untyped turn next door shares
//! [`begin`](super::PreparedRun::begin) and
//! [`finish`](super::PreparedRun::finish) with it, which is what keeps the two
//! announcing themselves identically on the stream.

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    Approver, Ended, EventSink, OutputReport, OutputSpec, PreparedRun, PromptPart, RunError,
    TurnOptions, bounded, drawable, prompt,
};

impl PreparedRun {
    /// Sends a prompt whose answer must be a value of type `T` rather than
    /// prose.
    ///
    /// ADR-0010's structured output, and the primitive a workflow is built on:
    /// the model is handed one terminal tool whose input *is* the answer, and
    /// `T` is deserialized from what it sent. The caller writes the schema —
    /// see [`OutputSpec`] for why basis derives nothing.
    ///
    /// **By default a typed turn is a shaping turn, not a working one.** That
    /// terminal tool is the *only* tool the turn holds — no files, no shell, no
    /// MCP — and the model is required to call it, so the turn can answer only
    /// from the conversation it already has. Asking it to review code in the
    /// same call returns a structurally valid answer from a model that read
    /// nothing, reported as a success. Two ways past that, and they are
    /// different trades. [`OutputSpec::with_tools`] keeps the ordinary toolset
    /// on this turn, so one call reads and answers — and gives up the forcing
    /// that guaranteed an answer. Or do the work on an ordinary turn
    /// ([`send`](Self::send) or [`execute`](Self::execute)) and ask for the
    /// shape on the next, which keeps the forcing and keeps each run's reading
    /// in a context of its own; `examples/review_workflow.rs` is that written
    /// out.
    ///
    /// The stream is unchanged. Header, forwarded events, permissions put to
    /// the approver, `RunFinished`: a client reading events cannot tell a typed
    /// turn from any other, which is the point — only the return value differs.
    /// The answer travels as the terminal tool's
    /// [`ToolQueued`](crate::Event::ToolQueued) input and
    /// [`ToolCompleted`](crate::Event::ToolCompleted) summary, and
    /// [`RunReport::final_message`](crate::RunReport::final_message) stays
    /// `None`, because a typed turn's
    /// committed final message is that tool result — putting a JSON payload in
    /// a field named for the assistant's prose would have every client render
    /// it as speech. Prose the model wrote alongside the call, usually none,
    /// arrives as [`Event::AssistantMessage`](crate::Event::AssistantMessage).
    ///
    /// Where a plain turn reports its failure on the stream and still returns
    /// `Ok`, this returns `Err`: a typed turn without a value has nothing to
    /// hand back.
    ///
    /// - [`RunError::OutputMismatch`] — an answer arrived that `T` did not
    ///   accept. mentra commits the exchange before basis reads it, so the
    ///   transcript keeps the attempt and a follow-up turn can say what was
    ///   wrong with it.
    /// - [`RunError::Runtime`] — the turn failed, *or* it finished without ever
    ///   calling the terminal tool. mentra reports both as
    ///   `MalformedProviderEvent` and basis will not read error prose to tell
    ///   them apart. A working turn ([`OutputSpec::with_tools`]) reaches the
    ///   second of those the most ways, since nothing forces its ending: it can
    ///   answer in prose, or be refused another round by a bound while it is
    ///   still gathering. Which bound that was is on the stream, as
    ///   [`Event::RunFinished`](crate::Event::RunFinished)'s `stopped_by` —
    ///   [`Bound::TokenBudget`](crate::Bound::TokenBudget) for an
    ///   allowance spent mid-gather — and only there, because the report that
    ///   would otherwise carry it is not handed back when there is no value to
    ///   hand back with it.
    ///
    /// The stream is complete and closed in every one of those cases, so a sink
    /// with somewhere to put events — a file, a channel — has the whole run.
    /// Only the sink *value* is lost, because it comes back inside the report.
    ///
    /// ```no_run
    /// use serde::Deserialize;
    /// use serde_json::json;
    ///
    /// #[derive(Deserialize)]
    /// struct Review {
    ///     verdict: String,
    /// }
    ///
    /// # async fn example(run: &mut basis::PreparedRun) -> Result<(), basis::RunError> {
    /// let spec = basis::OutputSpec::new(
    ///     "submit_review",
    ///     "call this once you have weighed everything you read on the last turn",
    ///     json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "verdict": { "type": "string", "description": "ship or hold" }
    ///         },
    ///         "required": ["verdict"]
    ///     }),
    /// );
    ///
    /// // The reading happened on an ordinary turn; this one only shapes it.
    /// run.execute(basis::NullSink).await?;
    /// let output = run
    ///     .output::<Review, _, _>(
    ///         "submit your review of what you just read",
    ///         spec,
    ///         basis::NullSink,
    ///         basis::AllowAll,
    ///     )
    ///     .await?;
    ///
    /// // A value, not a paragraph to parse — and what it cost, for a caller
    /// // adding runs up against a budget.
    /// println!("{} ({} tokens)", output.value.verdict, output.report.usage.total_tokens());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn output<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        spec: OutputSpec,
        sink: S,
        approver: A,
    ) -> Result<OutputReport<T, S>, RunError> {
        self.typed_turn(prompt.into(), spec, sink, approver, TurnOptions::default())
            .await
    }

    /// A typed turn with explicit run options.
    ///
    /// Same relationship to [`output`](Self::output) as
    /// [`send_with_options`](Self::send_with_options) has to
    /// [`send`](Self::send): a typed turn is cancellable and boundable like any
    /// other, and a fan-out that gives each of its runs a deadline should not
    /// have to give up types to get one.
    pub async fn output_with_options<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: impl Into<String>,
        spec: OutputSpec,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<OutputReport<T, S>, RunError> {
        self.typed_turn(prompt.into(), spec, sink, approver, options)
            .await
    }
    /// One typed turn. Identical to [`turn`](Self::turn) but for the one call
    /// in the middle — which is the whole reason both are written this way,
    /// since a second copy of the header-and-forwarding dance is a second thing
    /// to keep in step with the stream contract.
    ///
    /// mentra is asked for a [`Value`] rather than for `T` directly, and basis
    /// deserializes. That costs nothing (the payload is already JSON) and buys
    /// the error distinction: a value that does not fit `T` is basis's own
    /// finding, reported as [`RunError::OutputMismatch`], instead of arriving
    /// as one more `MalformedProviderEvent` indistinguishable from a provider
    /// that misbehaved.
    async fn typed_turn<T: DeserializeOwned, S: EventSink, A: Approver>(
        &mut self,
        prompt: String,
        spec: OutputSpec,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<OutputReport<T, S>, RunError> {
        let options = bounded(options, &self.bounds);
        drawable(&options)?;
        let parts = vec![PromptPart::Text(prompt)];
        let turn = self.begin(&parts, sink, approver)?;

        // The same clone the untyped turn keeps, for the same reason: a typed
        // turn is boundable like any other and owes the same account of why it
        // ended.
        let run_options = self.run_options(options);
        let observed = run_options.clone();

        let result = self
            .session
            .append_turn_to_output::<Value>(
                prompt::into_blocks(parts),
                run_options,
                spec.into_terminal_spec(),
            )
            .await;

        let typed = match result {
            Ok(output) => Ok(serde_json::from_value::<T>(output.value)),
            Err(error) => Err(error),
        };
        let ended = match &typed {
            Ok(Ok(_)) => Ended::Answered(None),
            Ok(Err(mismatch)) => Ended::Mismatched(mismatch),
            Err(error) => Ended::Failed(error),
        };

        let report = self.finish(turn, ended, &observed).await?;

        match typed {
            Ok(Ok(value)) => Ok(OutputReport { value, report }),
            Ok(Err(mismatch)) => Err(RunError::OutputMismatch(mismatch)),
            Err(error) => Err(RunError::Runtime(error)),
        }
    }
}
