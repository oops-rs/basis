//! What a run reported spending.
//!
//! Its own module because it is the thing everything else about cost is built
//! on: the per-run bounds of ADR-0014 enforce a budget, and ADR-0010's shared
//! `BudgetPool` will draw from one — but both need somewhere honest to read
//! *what a turn actually cost*, and this is it.

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
/// It counts the rounds of the run's own agent. A subagent gets its own event
/// bus in mentra, and nothing relays it to the parent's, so work delegated
/// through `task` does not appear here. The token bounds have the same blind
/// spot rather than covering for it: mentra enforces
/// [`TurnOptions::token_budget`](super::TurnOptions) and
/// [`BudgetPool`](crate::BudgetPool) against an accounting handle it *can*
/// share with a child — `RunOptions::child` exists for exactly that — but its
/// own `task` intrinsic runs the child on fresh options, so a delegating run
/// spends tokens that neither this figure nor its budget ever sees. Closing
/// that needs a subagent's usage and accounting to reach the parent, which is
/// upstream work (ADR-0005), not something lan can infer from the outside.
///
/// And cache tokens are counted but never mixed in — see
/// [`total_tokens`](Self::total_tokens).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
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
        }
    }

    /// This total plus whatever `event` reported, unchanged when it reported
    /// nothing.
    ///
    /// Read from the session event rather than from lan's mapped
    /// [`Event::Usage`](crate::Event::Usage) so that a sink which stopped
    /// accepting events still gets an honest total: what the run spent is not
    /// contingent on anyone having listened.
    pub(crate) fn recording(self, event: &mentra::SessionEvent) -> Self {
        let mentra::SessionEvent::UsageReport {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
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
        }
    }

    #[test]
    fn usage_is_summed_over_the_rounds_of_a_turn() {
        // mentra reports per completed model response, not cumulatively — its
        // own token budget adds the reports up, and a lan that treated the
        // last one as the total would under-report every multi-round turn.
        let usage = RunUsage::default()
            .recording(&usage_report(100, 20))
            .recording(&usage_report(150, 30));

        assert_eq!(usage.input_tokens, 250);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_read_tokens, 2);
        assert_eq!(usage.cache_creation_tokens, 4);
        assert_eq!(usage.total_tokens(), 300, "the budget counts these two");
    }

    #[test]
    fn an_event_that_reports_no_usage_changes_nothing() {
        let counted = RunUsage::default().recording(&usage_report(10, 5));
        let after = counted.recording(&mentra::SessionEvent::UserMessage {
            text: "hello".to_string(),
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
