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
    Approver, Ended, EventSink, OutputAttempt, OutputAttemptReport, OutputDecision, OutputFailure,
    OutputReport, OutputReservation, OutputSpec, PreparedRun, PromptPart, RunError, TurnOptions,
    bounded, drawable, prompt,
};

impl PreparedRun {
    /// Drive one multipart working or shaping turn whose candidate output is
    /// validated before its generated tool may terminate the run.
    ///
    /// Once the turn begins, every in-turn ending returns an
    /// [`OutputAttemptReport`], including a decode mismatch, a missing output,
    /// a tripped bound, or a runtime failure. Only setup and stream-forwarding
    /// failures remain outer [`RunError`]s.
    pub async fn output_parts_validated_with_options<T, S, A, V>(
        &mut self,
        parts: Vec<PromptPart>,
        reservation: OutputReservation,
        validator: V,
        sink: S,
        approver: A,
        options: TurnOptions,
    ) -> Result<OutputAttemptReport<T, S>, RunError>
    where
        T: DeserializeOwned,
        S: EventSink,
        A: Approver,
        V: Fn(&Value) -> OutputDecision + Send + Sync + 'static,
    {
        let options = bounded(options, &self.bounds);
        drawable(&options)?;
        let turn = self.begin(&parts, sink, approver)?;
        let (usage, usage_tap) = self.observe_usage();
        let run_options = self.run_options(options);
        let observed = run_options.clone();

        let result = self
            .session
            .append_turn_to_reserved_output::<Value, _>(
                prompt::into_blocks(parts),
                run_options,
                reservation.into_inner(),
                move |candidate| match validator(candidate) {
                    OutputDecision::Accept(value) => mentra::TerminalOutputDecision::Accept(value),
                    OutputDecision::Reject(reason) => {
                        mentra::TerminalOutputDecision::Reject(reason)
                    }
                },
            )
            .await;
        let usage = Self::finish_usage(usage, usage_tap);

        let (output, failure) = match result {
            Ok(output) => match serde_json::from_value::<T>(output.value) {
                Ok(value) => (OutputAttempt::Accepted(value), None),
                Err(error) => (OutputAttempt::Mismatch(error), None),
            },
            Err(error) => (OutputAttempt::Missing, Some(error)),
        };
        let ended = match (&output, &failure) {
            (OutputAttempt::Accepted(_), _) => Ended::Answered(None),
            (OutputAttempt::Mismatch(error), _) => Ended::Mismatched(error),
            (OutputAttempt::Missing, Some(error)) => Ended::Failed(error),
            (OutputAttempt::Missing, None) => unreachable!("a missing value carries its failure"),
        };
        let mut report = self.finish(turn, ended, &observed, usage).await?;
        report.failure = failure;

        Ok(OutputAttemptReport { output, report })
    }

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
    /// ([`send_with_options`](Self::send_with_options) or
    /// [`execute_with_approver`](Self::execute_with_approver)) and ask for the
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
    /// `Ok`, this returns `Err`: a typed turn without a value has no
    /// [`OutputReport`] to hand back. It hands back an [`OutputFailure`]
    /// instead, which is the same error it always returned —
    /// [`OutputFailure::error`] — beside the [`RunReport`](crate::RunReport)
    /// the turn earned anyway. A caller that wants only the error writes
    /// `.await.map_err(RunError::from)?` and is where it was before.
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
    ///   still gathering. Which bound that was is on the report, as
    ///   [`RunReport::stopped_by`](crate::RunReport::stopped_by) —
    ///   [`Bound::TokenBudget`](crate::Bound::TokenBudget) for an allowance
    ///   spent mid-gather — and on the stream, as
    ///   [`Event::RunFinished`](crate::Event::RunFinished)'s `stopped_by`. The
    ///   two say the same thing, which is why the report is worth returning:
    ///   a caller should not have to re-read its own event log to learn that a
    ///   budget, and not a broken provider, is what it just paid for.
    ///
    /// The stream is complete and closed in every one of those cases, and the
    /// sink comes back inside the report either way — so a `CollectingSink` on
    /// a turn that produced nothing is still readable.
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
    /// run.execute_with_approver(basis::NullSink, basis::AllowAll)
    ///     .await?;
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
    ) -> Result<OutputReport<T, S>, OutputFailure<S>> {
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
    ) -> Result<OutputReport<T, S>, OutputFailure<S>> {
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
    ) -> Result<OutputReport<T, S>, OutputFailure<S>> {
        let options = bounded(options, &self.bounds);
        drawable(&options)?;
        let parts = vec![PromptPart::Text(prompt)];
        let turn = self.begin(&parts, sink, approver)?;
        let (usage, usage_tap) = self.observe_usage();

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
        let usage = Self::finish_usage(usage, usage_tap);

        let typed = match result {
            Ok(output) => Ok(serde_json::from_value::<T>(output.value)),
            Err(error) => Err(error),
        };
        let ended = match &typed {
            Ok(Ok(_)) => Ended::Answered(None),
            Ok(Err(mismatch)) => Ended::Mismatched(mismatch),
            Err(error) => Ended::Failed(error),
        };

        let report = self.finish(turn, ended, &observed, usage).await?;

        // The report is built either way and handed back either way. What the
        // turn spent, which bound ended it, and the sink it wrote are facts
        // about the run, not about whether a value came out of it — dropping
        // them on the failing branch made the failing branch the one a caller
        // could say least about (ADR-0003).
        match typed {
            Ok(Ok(value)) => Ok(OutputReport { value, report }),
            Ok(Err(mismatch)) => Err(OutputFailure {
                error: RunError::OutputMismatch(mismatch),
                report: Some(report),
            }),
            Err(error) => Err(OutputFailure {
                error: RunError::Runtime(error),
                report: Some(report),
            }),
        }
    }
}
