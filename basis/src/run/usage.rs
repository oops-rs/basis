//! What a run reported spending.
//!
//! Its own module because it is the thing everything else about cost is built
//! on: the per-run bounds of ADR-0014 enforce a budget, and ADR-0010's shared
//! `BudgetPool` will draw from one — but both need somewhere honest to read
//! *what a turn actually cost*, and this is it.

use serde::{Deserialize, Serialize};

/// What a run reported spending, summed over the rounds it took.
///
/// mentra emits one usage report per completed model response, carrying *that
/// round's* numbers rather than a running total — its own soft token budget is
/// a sum of them, and so is this. A turn that took four rounds reports four
/// times and lands here as one figure.
///
/// Three things to know before treating this as a bill.
///
/// It is *reported*, not measured. A provider that says nothing leaves this at
/// zero, and a stream that lagged — the [`Notice`](crate::Event::Notice) says
/// how many events were dropped — may have lost a report along with everything
/// else, which undercounts.
///
/// It counts the rounds of the run's own agent *and* of the work that agent
/// delegates. A delegating tool — basis's [`spawn`](crate::tools::spawn), or
/// mentra's own `task` intrinsic — drives its subagent on the parent run's
/// [`RunOptions::child`](mentra::runtime::RunOptions::child) and relays the
/// child's usage reports onto the parent's bus, so a delegated round arrives
/// here like any other. Indistinguishable from the parent's own, which is
/// consistent with the accounting being aggregate: mentra's usage report
/// carries no agent id for anyone to attribute by. Summing the stream and
/// reading this figure therefore give one answer, and it is the same answer
/// [`TurnOptions::token_budget`](super::TurnOptions) and
/// [`BudgetPool`](crate::BudgetPool) are enforced against, because the child
/// shares the parent's accounting handle rather than getting one of its own.
///
/// Both halves had to be built for that to hold. The shared counter came
/// first, and until mentra made its event relay public the two disagreed for
/// basis's own delegations: the *bound* saw a delegated round and the *tally*
/// did not, so a run could stop on a total it reported a fraction of.
///
/// One edge survives, and it fails loudly rather than leaking: a delegation
/// issued once the shared budget is already crossed inherits an allowance with
/// nothing left in it, does zero rounds, and comes back as a failed tool call
/// instead of an empty success. That is the round-boundary softness
/// [`TurnOptions::token_budget`](super::TurnOptions) already describes, seen
/// from the delegating side, and mentra pins it
/// (`delegating_with_the_budget_already_spent_fails_the_delegation`).
///
/// And cache tokens are counted but never mixed in — see
/// [`total_tokens`](Self::total_tokens).
///
/// It is serializable because the same four numbers are what
/// [`Event::RunFinished`](crate::Event) puts on the wire and what basis's CLI
/// records beside a finished task: one shape, so a host reading the stream and
/// a host holding the report cannot disagree about what a run cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Reasoning the model did *inside* `output_tokens` — the Responses wire
    /// counts it there, so adding it to the total would count it twice.
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// Thinking the model did *outside* `output_tokens` — Gemini counts its
    /// thoughts beside the candidates, not in them. Two fields rather than one
    /// sum because a sum would be wrong for one of the two wires, and a reader
    /// who wants "how much did it think" adds them; one who wants "what was I
    /// billed for output" must not. Absent from a record written before the
    /// split, both read as zero.
    #[serde(default)]
    pub thoughts_tokens: u64,
}

impl RunUsage {
    /// Input plus output — what [`RunConfig::with_token_budget`](super::RunConfig) counts.
    ///
    /// Cache reads and cache writes are left out for the same reason mentra
    /// leaves them out of the budget it enforces: they are priced differently
    /// everywhere, and a total that mixed them would answer no question
    /// exactly.
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// This total plus `other`'s — how a caller adds up several runs.
    ///
    /// The shape a shared budget wants: a fan-out folds its reports into one
    /// figure without any of the runs knowing about the others.
    #[must_use]
    pub const fn plus(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_read_tokens: self.cache_read_tokens + other.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens + other.cache_creation_tokens,
            reasoning_tokens: self.reasoning_tokens + other.reasoning_tokens,
            thoughts_tokens: self.thoughts_tokens + other.thoughts_tokens,
        }
    }

    /// This total plus whatever `event` reported, unchanged when it reported
    /// nothing.
    ///
    /// Read from the session event rather than from basis's mapped
    /// [`Event::Usage`](crate::Event::Usage) so that a sink which stopped
    /// accepting events still gets an honest total: what the run spent is not
    /// contingent on anyone having listened.
    pub(crate) fn recording(self, event: &mentra::SessionEvent) -> Self {
        let mentra::SessionEvent::UsageReport {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            reasoning_tokens,
            thoughts_tokens,
            ..
        } = event
        else {
            return self;
        };

        Self {
            input_tokens: self.input_tokens + *input_tokens,
            output_tokens: self.output_tokens + *output_tokens,
            cache_read_tokens: self.cache_read_tokens + *cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens + *cache_creation_tokens,
            reasoning_tokens: self.reasoning_tokens + *reasoning_tokens,
            thoughts_tokens: self.thoughts_tokens + *thoughts_tokens,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// One round's worth of reported usage, as mentra emits it.
    fn usage_report(input: u64, output: u64) -> mentra::SessionEvent {
        mentra::SessionEvent::UsageReport {
            agent_id: "a1".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 1,
            cache_creation_tokens: 2,
            reasoning_tokens: 3,
            thoughts_tokens: 4,
        }
    }

    #[test]
    fn usage_is_summed_over_the_rounds_of_a_turn() {
        // mentra reports per completed model response, not cumulatively — its
        // own token budget adds the reports up, and a basis that treated the
        // last one as the total would under-report every multi-round turn.
        let usage = RunUsage::default()
            .recording(&usage_report(100, 20))
            .recording(&usage_report(150, 30));

        assert_eq!(usage.input_tokens, 250);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_read_tokens, 2);
        assert_eq!(usage.cache_creation_tokens, 4);
        assert_eq!(usage.reasoning_tokens, 6);
        assert_eq!(usage.thoughts_tokens, 8);
        assert_eq!(usage.total_tokens(), 300, "the budget counts these two");
    }

    #[test]
    fn an_event_that_reports_no_usage_changes_nothing() {
        let counted = RunUsage::default().recording(&usage_report(10, 5));
        let after = counted.recording(&mentra::SessionEvent::UserMessage {
            text: "hello".to_string(),
            image_count: 0,
        });

        assert_eq!(after, counted);
    }

    #[test]
    fn a_turn_that_reported_nothing_reports_zero() {
        // A provider that sends no usage leaves this at zero rather than
        // guessing — which is why the field is a tally and not a bill.
        let usage = RunUsage::default();

        assert_eq!(usage.total_tokens(), 0);
    }

    #[test]
    fn usage_adds_up_across_runs() {
        // What a shared budget folds: neither run knows about the other.
        let one = RunUsage::default().recording(&usage_report(100, 10));
        let two = RunUsage::default().recording(&usage_report(200, 20));

        assert_eq!(one.plus(two).total_tokens(), 330);
        assert_eq!(one.total_tokens(), 110, "the originals are untouched");
    }
}
