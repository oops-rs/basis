//! What one turn may spend, and how it can be stopped.
//!
//! A sibling of [`RunConfig`](super::RunConfig) and [`RunSpec`](crate::RunSpec)
//! rather than something owned by the run: those two say what a *run* may
//! spend, this says what one call may, and [`bounded`] is the only place that
//! distinction is resolved.

use std::time::{Duration, SystemTime};

use mentra::runtime::{CancellationToken, ProviderRetry, RunOptions};

use super::{Bounds, RunError};
use crate::budget::BudgetPool;

/// Limits and stop signals for a single turn.
///
/// basis's own type rather than a re-export of mentra's `RunOptions`: the same
/// reasoning as [`Event`](crate::Event) — basis owns its surface so mentra's
/// internals can move without breaking basis's callers. Only the knobs a harness
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
    /// kept either way; the report is what disagrees. `basis`'s
    /// `tests/cancellation.rs` pins that behavior so a change to it is noticed.
    ///
    /// A stopped turn reports no [`Bound`](crate::Bound), unlike a spent
    /// [`Bounds::token_budget`]. A bound is an allowance the
    /// run outgrew, and a script is right to retry one with a bigger number; a
    /// stop is an instruction whoever holds this token issued, and retrying it
    /// would undo their decision.
    pub stop: Option<CancellationToken>,
    /// What this one call may spend — deadline, tool budget, token budget.
    ///
    /// One caveat here beyond what [`Bounds`] itself says, and it matters: a
    /// token budget ends the turn *gracefully*, and the caveat on
    /// [`stop`](Self::stop) about how a graceful end is reported applies — a
    /// turn stopped after a tool round comes back failed for want of a final
    /// message, and without the named bound that failure is indistinguishable
    /// from a provider's. `basis`'s `tests/token_budget.rs` drives exactly
    /// that shape.
    pub bounds: Bounds,
    /// An allowance this turn shares with every other run drawing on it.
    ///
    /// The other kind of token bound, and the two compose rather than compete:
    /// [`Bounds::token_budget`] says what *this* turn may spend, a
    /// pool says what the whole job may, and a turn carrying both stops at
    /// whichever comes first. See [`BudgetPool`] for how that is arranged, and
    /// for the overshoot a shared soft bound implies.
    ///
    /// A turn drawing on a pool with nothing left is refused before its prompt
    /// is sent, with [`RunError::BudgetExhausted`](crate::RunError::BudgetExhausted).
    pub budget: Option<BudgetPool>,
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

    /// Gives up on the turn after `deadline`. Sugar into
    /// [`bounds`](Self::bounds).
    pub fn with_deadline(self, deadline: Duration) -> Self {
        Self {
            bounds: self.bounds.with_deadline(deadline),
            ..self
        }
    }

    /// Caps how many tool calls this turn may make. Sugar into
    /// [`bounds`](Self::bounds).
    pub fn with_tool_budget(self, tool_budget: usize) -> Self {
        Self {
            bounds: self.bounds.with_tool_budget(tool_budget),
            ..self
        }
    }

    /// Caps the tokens this turn may report using. Sugar into
    /// [`bounds`](Self::bounds).
    pub fn with_token_budget(self, token_budget: u64) -> Self {
        Self {
            bounds: self.bounds.with_token_budget(token_budget),
            ..self
        }
    }

    /// Draws this turn's tokens from an allowance shared with other runs.
    ///
    /// Immutable like the rest — a new value, the same pool. That is the whole
    /// shape of the exception [`BudgetPool`] makes: options are copied, the
    /// allowance is shared.
    pub fn with_budget(self, budget: BudgetPool) -> Self {
        Self {
            budget: Some(budget),
            ..self
        }
    }

    /// `provider_retry` and `retry_budget` are the *runtime's*, not this
    /// turn's: how patiently a failing provider is waited out describes the
    /// provider connection (ADR-0018), so both arrive from
    /// [`Runtime`](crate::Runtime) through the run rather than from knobs on
    /// this type. They are parameters rather than fields for exactly that
    /// reason — a `TurnOptions` a caller built has no business carrying them.
    pub(super) fn into_run_options(
        self,
        provider_retry: ProviderRetry,
        retry_budget: usize,
    ) -> RunOptions {
        let options = RunOptions {
            cancellation: self.cancel,
            stop: self.stop,
            deadline: self.bounds.deadline.map(|after| SystemTime::now() + after),
            tool_budget: self.bounds.tool_budget,
            token_budget: self.bounds.token_budget,
            provider_retry,
            retry_budget,
            ..RunOptions::default()
        };

        // Installing the pool's counter is what makes the bound shared: mentra
        // adds each round's usage to whatever handle it was given and checks
        // the bound against that total, so every run on one pool is measured
        // against every other's spending rather than its own.
        match self.budget {
            Some(pool) => RunOptions {
                token_budget: Some(pool.turn_bound(self.bounds.token_budget)),
                token_usage: pool.counter(),
                ..options
            },
            None => options,
        }
    }
}
/// Fills in whatever `options` left unset from the run's configured bounds.
///
/// A caller that passes options in order to attach a cancellation token has
/// said nothing about limits, and reading that silence as "no deadline" would
/// unbound a run whose config asked for one.
pub(super) fn bounded(options: TurnOptions, configured: &TurnOptions) -> TurnOptions {
    // Cloned rather than moved out, because taking the field would leave
    // `options` partially moved and the `..options` below could not finish the
    // job. It is an `Arc` either way.
    let budget = options.budget.clone().or_else(|| configured.budget.clone());

    TurnOptions {
        bounds: options.bounds.or(configured.bounds),
        budget,
        ..options
    }
}

/// Refuses a turn whose shared allowance is already spent.
///
/// Checked here — before a header is emitted, before a prompt is sent, before
/// anything is committed — rather than left to mentra, which would take the
/// turn, run no rounds, and report the missing assistant message as a provider
/// error. See [`BudgetPool`] for the whole argument.
pub(super) fn drawable(options: &TurnOptions) -> Result<(), RunError> {
    let Some(pool) = &options.budget else {
        return Ok(());
    };

    if pool.is_exhausted() {
        return Err(RunError::BudgetExhausted {
            limit: pool.limit(),
            spent: pool.spent(),
        });
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::RunUsage;

    /// Builds the mentra options for a turn whose provider connection is
    /// nobody's business here: every test below is about a *bound*, and the
    /// retry schedule beside it is the runtime's
    /// ([`RuntimeBuilder::with_provider_retry`](crate::RuntimeBuilder::with_provider_retry)),
    /// asserted where it is set rather than restated in each of these.
    fn as_mentra_would(options: TurnOptions) -> RunOptions {
        options.into_run_options(ProviderRetry::default(), RunOptions::default().retry_budget)
    }

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

        assert_eq!(merged.bounds.deadline, Some(Duration::from_secs(600)));
        assert_eq!(merged.bounds.tool_budget, Some(12));
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
            bounded(explicit, &configured).bounds.deadline,
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn a_prepared_run_is_unbounded_until_it_is_bounded() {
        let unset = TurnOptions::default();

        let merged = bounded(TurnOptions::default(), &unset);
        assert_eq!(merged.bounds, Bounds::default());
        assert!(bounded(TurnOptions::default(), &unset).budget.is_none());
    }

    #[test]
    fn a_pool_bounds_the_turn_at_the_whole_jobs_allowance() {
        // Not at the run's share of it: the counter is shared, so the figure
        // handed to mentra is the job's and every drawing run is measured
        // against every other's spending.
        let pool = BudgetPool::new(500_000);
        let options = TurnOptions::default().with_budget(pool.clone());

        assert_eq!(as_mentra_would(options).token_budget, Some(500_000));
    }

    #[test]
    fn a_pooled_turn_reports_into_the_pools_own_counter() {
        // The claim the whole design rests on. If mentra were handed a fresh
        // counter, each run would get the pool's limit to itself and the job
        // would cost N times what was asked for.
        let pool = BudgetPool::new(1_000);
        let run_options = as_mentra_would(TurnOptions::default().with_budget(pool.clone()));

        pool.record(RunUsage {
            input_tokens: 300,
            ..RunUsage::default()
        });

        assert_eq!(
            run_options.reported_tokens(),
            300,
            "mentra reads the spending the pool records, and the reverse"
        );
    }

    #[test]
    fn an_unpooled_turn_gets_a_counter_of_its_own() {
        // Two unpooled runs must not accidentally share accounting, which they
        // would if basis reused one handle instead of letting mentra's default
        // mint a fresh one per turn.
        let first = as_mentra_would(TurnOptions::default().with_token_budget(100));
        let second = as_mentra_would(TurnOptions::default().with_token_budget(100));

        assert!(!std::sync::Arc::ptr_eq(
            &first.token_usage,
            &second.token_usage
        ));
    }

    #[test]
    fn a_per_turn_cap_and_a_pool_both_bind() {
        // mentra has one bound per run, so the two have to be resolved into one
        // figure here. A cap of 50k on a pool that has spent 200k of 500k means
        // "stop at 250k of the job's total" — tighter than the pool, and the
        // pool is still the ceiling when the cap is the looser of the two.
        let pool = BudgetPool::new(500_000);
        pool.record(RunUsage {
            input_tokens: 200_000,
            ..RunUsage::default()
        });

        let capped = TurnOptions::default()
            .with_budget(pool.clone())
            .with_token_budget(50_000);
        assert_eq!(as_mentra_would(capped).token_budget, Some(250_000));

        let generous = TurnOptions::default()
            .with_budget(pool)
            .with_token_budget(u64::MAX);
        assert_eq!(as_mentra_would(generous).token_budget, Some(500_000));
    }

    #[test]
    fn attaching_a_token_does_not_detach_the_pool() {
        // The same argument as the deadline above, and the expensive version of
        // it: a stop button that quietly unbounded the shared allowance would
        // let a fan-out spend without limit.
        let pool = BudgetPool::new(1_000);
        let configured = TurnOptions::default().with_budget(pool.clone());
        let (options, _token) = TurnOptions::stoppable();

        assert_eq!(bounded(options, &configured).budget, Some(pool));
    }

    #[test]
    fn an_explicit_pool_wins_over_the_configured_one() {
        let configured = TurnOptions::default().with_budget(BudgetPool::new(1_000));
        let explicit = BudgetPool::new(50);

        let merged = bounded(
            TurnOptions::default().with_budget(explicit.clone()),
            &configured,
        );

        assert_eq!(merged.budget, Some(explicit));
    }

    #[test]
    fn a_turn_on_a_spent_pool_is_refused_rather_than_sent() {
        let pool = BudgetPool::new(100);
        let options = TurnOptions::default().with_budget(pool.clone());

        assert!(drawable(&options).is_ok(), "a full pool draws");

        pool.record(RunUsage {
            input_tokens: 120,
            ..RunUsage::default()
        });

        let refused = drawable(&options).expect_err("a spent pool refuses");
        assert!(matches!(
            refused,
            RunError::BudgetExhausted {
                limit: 100,
                spent: 120
            }
        ));
    }

    #[test]
    fn a_turn_with_no_pool_is_always_drawable() {
        assert!(drawable(&TurnOptions::default().with_token_budget(0)).is_ok());
    }
}
