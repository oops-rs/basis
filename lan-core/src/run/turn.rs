//! What one turn may spend, and how it can be stopped.
//!
//! A sibling of [`RunConfig`](super::RunConfig) and [`RunSpec`](crate::RunSpec)
//! rather than something owned by the run: those two say what a *run* may
//! spend, this says what one call may, and [`bounded`] is the only place that
//! distinction is resolved.

use std::time::{Duration, SystemTime};

use mentra::runtime::{CancellationToken, RunOptions};

/// Limits and stop signals for a single turn.
///
/// lan's own type rather than a re-export of mentra's `RunOptions`: the same
/// reasoning as [`Event`](crate::Event) — lan owns its surface so mentra's
/// internals can move without breaking lan's callers. Only the knobs a harness
/// actually needs are exposed; the rest stay at mentra's defaults.
#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    /// Trips to abandon the turn. The turn fails and is rolled back — what a
    /// client's stop button means.
    pub cancel: Option<CancellationToken>,
    /// Trips to end the turn gracefully at the next round boundary, keeping
    /// what the model has already committed.
    ///
    /// One caveat, upstream and honest: mentra ends the turn but still owes its
    /// caller a final assistant message, so a stop that lands after a *tool*
    /// round — where the last committed message is the tool's result — comes
    /// back as a failed turn even though nothing was rolled back. The work is
    /// kept either way; the report is what disagrees. `lan-core`'s
    /// `tests/cancellation.rs` pins that behavior so a change to it is noticed.
    pub stop: Option<CancellationToken>,
    /// Gives up on the turn after this long.
    pub deadline: Option<Duration>,
    /// Caps how many tool calls one turn may make.
    pub tool_budget: Option<usize>,
    /// Caps the tokens one turn may report using, input plus output.
    ///
    /// Soft by construction: usage is only known once a round has streamed in
    /// full, so the round that crosses the line is always allowed to finish.
    /// It ends the turn *gracefully* at the next boundary — what the model
    /// already committed is kept, so the work is not thrown away for being one
    /// round too long.
    pub token_budget: Option<u64>,
}

impl TurnOptions {
    /// A turn that can be abandoned through the returned token.
    ///
    /// What a client's stop button trips: the turn fails with
    /// [`RunOutcome::Error`](crate::RunOutcome::Error) and mentra rolls it back, so the session is left
    /// as it was before the prompt. For "stop when you have enough, and keep
    /// it", see [`stoppable`](Self::stoppable).
    pub fn cancellable() -> (Self, CancellationToken) {
        let token = CancellationToken::default();
        (
            Self {
                cancel: Some(token.clone()),
                ..Self::default()
            },
            token,
        )
    }

    /// A turn that can be ended gracefully through the returned token.
    ///
    /// The other half of the pair, and the difference is what happens to the
    /// work: this one lets the round in flight finish and keeps everything the
    /// model committed, where [`cancellable`](Self::cancellable) throws the
    /// turn away. A caller watching the stream and deciding it has read enough
    /// wants this one; a caller whose user pressed stop wants the other. Mind
    /// the caveat on [`stop`](Self::stop) about how the kept work is reported.
    pub fn stoppable() -> (Self, CancellationToken) {
        let token = CancellationToken::default();
        (
            Self {
                stop: Some(token.clone()),
                ..Self::default()
            },
            token,
        )
    }

    /// Attaches a token that abandons the turn — for a caller that already
    /// holds one, because it arms the token before it knows which turn it will
    /// stop.
    pub fn with_cancel(self, cancel: CancellationToken) -> Self {
        Self {
            cancel: Some(cancel),
            ..self
        }
    }

    /// Attaches a token that ends the turn gracefully at the next round
    /// boundary.
    pub fn with_stop(self, stop: CancellationToken) -> Self {
        Self {
            stop: Some(stop),
            ..self
        }
    }

    pub fn with_deadline(self, deadline: Duration) -> Self {
        Self {
            deadline: Some(deadline),
            ..self
        }
    }

    pub fn with_tool_budget(self, tool_budget: usize) -> Self {
        Self {
            tool_budget: Some(tool_budget),
            ..self
        }
    }

    pub fn with_token_budget(self, token_budget: u64) -> Self {
        Self {
            token_budget: Some(token_budget),
            ..self
        }
    }

    pub(super) fn into_run_options(self) -> RunOptions {
        RunOptions {
            cancellation: self.cancel,
            stop: self.stop,
            deadline: self.deadline.map(|after| SystemTime::now() + after),
            tool_budget: self.tool_budget,
            token_budget: self.token_budget,
            ..RunOptions::default()
        }
    }
}
/// Fills in whatever `options` left unset from the run's configured bounds.
///
/// A caller that passes options in order to attach a cancellation token has
/// said nothing about limits, and reading that silence as "no deadline" would
/// unbound a run whose config asked for one.
pub(super) fn bounded(options: TurnOptions, bounds: &TurnOptions) -> TurnOptions {
    TurnOptions {
        deadline: options.deadline.or(bounds.deadline),
        tool_budget: options.tool_budget.or(bounds.tool_budget),
        token_budget: options.token_budget.or(bounds.token_budget),
        ..options
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaching_a_token_does_not_unbound_a_configured_run() {
        // What ACP does on every turn: options exist only to carry a stop
        // button. Reading that as "and no deadline either" would silently
        // remove the bound an unattended caller asked for.
        let configured = TurnOptions::default()
            .with_deadline(Duration::from_secs(600))
            .with_tool_budget(12);
        let (options, token) = TurnOptions::cancellable();

        let merged = bounded(options, &configured);

        assert_eq!(merged.deadline, Some(Duration::from_secs(600)));
        assert_eq!(merged.tool_budget, Some(12));
        assert!(merged.cancel.is_some(), "the token still arrives");
        assert!(!token.is_cancelled());
    }

    #[test]
    fn stopping_and_cancelling_are_different_signals() {
        // They end a turn differently — one keeps the committed work and
        // reports success, the other throws it away and reports failure — so a
        // turn must never receive one where the caller asked for the other.
        let (cancellable, cancel) = TurnOptions::cancellable();
        let (stoppable, stop) = TurnOptions::stoppable();

        assert!(cancellable.cancel.is_some() && cancellable.stop.is_none());
        assert!(stoppable.stop.is_some() && stoppable.cancel.is_none());

        cancel.cancel();
        assert!(
            !stop.is_cancelled(),
            "one turn's stop button is not another's"
        );
    }

    #[test]
    fn a_turn_can_carry_both_signals_at_once() {
        // A harness offering both "stop when you have enough" and "abandon
        // this" arms both, so attaching one must not clear the other.
        let (options, cancel) = TurnOptions::cancellable();
        let stop = CancellationToken::default();

        let both = options.with_stop(stop.clone());

        assert!(
            both.cancel.is_some(),
            "the first signal survives the second"
        );
        assert!(both.stop.is_some());
        assert!(!cancel.is_cancelled() && !stop.is_cancelled());
    }

    #[test]
    fn attaching_a_token_returns_a_new_value() {
        let base = TurnOptions::default();
        let armed = base.clone().with_cancel(CancellationToken::default());

        assert!(base.cancel.is_none(), "the original must be untouched");
        assert!(armed.cancel.is_some());
    }

    #[test]
    fn an_explicit_bound_wins_over_the_configured_one() {
        let configured = TurnOptions::default().with_deadline(Duration::from_secs(600));
        let explicit = TurnOptions::default().with_deadline(Duration::from_secs(30));

        assert_eq!(
            bounded(explicit, &configured).deadline,
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn a_prepared_run_is_unbounded_until_it_is_bounded() {
        let unset = TurnOptions::default();

        assert_eq!(bounded(TurnOptions::default(), &unset).deadline, None);
        assert_eq!(bounded(TurnOptions::default(), &unset).tool_budget, None);
        assert_eq!(bounded(TurnOptions::default(), &unset).token_budget, None);
    }
}
