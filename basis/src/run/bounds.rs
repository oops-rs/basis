//! The three limits a run may carry, said once.
//!
//! [`RunSpec`](crate::RunSpec) bounds a run, [`TurnOptions`](super::TurnOptions)
//! bounds one call, and before this type each spelled the same three fields
//! out again. One value means the pair cannot drift, and the merge in
//! [`bounded`](super::turn::bounded) — explicit wins, configured fills in —
//! is written against one type instead of three fields at a time.

use std::time::Duration;

/// Limits on what a run, or one turn of it, may spend.
///
/// Every bound is unset by default and stays unset for an unattended caller
/// too: an attended run has a person watching, who can tell "thinking hard"
/// from "stuck" in a way no timer can, and with no scheduler shipped there is
/// no period for basis to guess a bound from (ADR-0014). Every bound here is a
/// *graceful* end rather than a discarded run — whatever the model committed
/// before the bound tripped is kept.
///
/// Limits only, deliberately: cancellation and the graceful stop signal are
/// per-call things a caller holds a token for, and they stay on
/// [`TurnOptions`](crate::TurnOptions). A [`BudgetPool`](crate::BudgetPool) is
/// an *allowance* shared across runs rather than a bound on one, so it is not
/// here either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bounds {
    /// Gives up after this long.
    pub deadline: Option<Duration>,
    /// Caps how many tool calls may be made.
    pub tool_budget: Option<usize>,
    /// Caps the tokens reported as used, input plus output.
    ///
    /// Soft by construction: usage is only known once a round has streamed in
    /// full, so the round that crosses the line is allowed to finish, and the
    /// report names [`Bound::TokenBudget`](crate::Bound::TokenBudget) as what
    /// ended the run. This is the bound that maps to money.
    pub token_budget: Option<u64>,
}

impl Bounds {
    pub fn new() -> Self {
        Self::default()
    }

    /// Gives up after `deadline`.
    pub fn with_deadline(self, deadline: Duration) -> Self {
        Self {
            deadline: Some(deadline),
            ..self
        }
    }

    /// Caps how many tool calls may be made.
    pub fn with_tool_budget(self, tool_budget: usize) -> Self {
        Self {
            tool_budget: Some(tool_budget),
            ..self
        }
    }

    /// Caps the tokens reported as used, input plus output.
    pub fn with_token_budget(self, token_budget: u64) -> Self {
        Self {
            token_budget: Some(token_budget),
            ..self
        }
    }

    /// Fills in whatever `self` leaves unset from `fallback`.
    ///
    /// The merge `bounded` (in `run::turn`) resolves a turn with: an
    /// explicit bound wins, a configured one fills the silence, and unset
    /// stays unset. Reading a caller's silence as "no deadline" would unbound
    /// a run whose config asked for one.
    #[must_use]
    pub fn or(self, fallback: Self) -> Self {
        Self {
            deadline: self.deadline.or(fallback.deadline),
            tool_budget: self.tool_budget.or(fallback.tool_budget),
            token_budget: self.token_budget.or(fallback.token_budget),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_return_new_values() {
        let base = Bounds::new();
        let bounded = base.with_deadline(Duration::from_secs(60));

        assert_eq!(base.deadline, None, "the original must be untouched");
        assert_eq!(bounded.deadline, Some(Duration::from_secs(60)));
    }

    #[test]
    fn an_explicit_bound_wins_and_a_configured_one_fills_in() {
        let explicit = Bounds::new().with_deadline(Duration::from_secs(30));
        let configured = Bounds::new()
            .with_deadline(Duration::from_secs(600))
            .with_tool_budget(12);

        let merged = explicit.or(configured);

        assert_eq!(merged.deadline, Some(Duration::from_secs(30)));
        assert_eq!(merged.tool_budget, Some(12));
        assert_eq!(merged.token_budget, None, "unset stays unset");
    }
}
