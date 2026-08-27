//! What changes from one run to the next.
//!
//! The cheap half of the split ADR-0010 asked for. A [`Workspace`] settles the
//! expensive questions once — where the context files are, which provider
//! answers, which MCP servers are connected — and a [`RunSpec`] carries the
//! handful of things that are actually per-run: what to ask, what to call the
//! session, how hard to think, and what the run may spend.
//!
//! Every field a spec *lacks* is deliberate: a change there would mean
//! opening a different workspace.
//!
//! [`Workspace`]: super::Workspace

use std::time::Duration;

use mentra::ModelInfo;

use crate::{
    budget::BudgetPool,
    run::{Bounds, Effort, TurnOptions},
};

use super::RunProfile;

/// Default name for the session a run creates. Sessions are named so a client
/// can tell them apart; the name carries no behavior.
pub(crate) const DEFAULT_SESSION_NAME: &str = "basis run";

/// One run's worth of intent, to be minted against a [`Workspace`].
///
/// Built the way every other config in basis is built — `new` plus `with_*`
/// methods that return new values — so a caller can keep one spec as a template
/// and derive each run's from it without the derivations interfering.
///
/// A bare prompt converts, because that is the common case:
///
/// ```no_run
/// # async fn example(workspace: &basis::Workspace) -> Result<(), basis::RunError> {
/// let run = workspace.prepare("what does this repo do?")?;
/// # let _ = run;
/// # Ok(())
/// # }
/// ```
///
/// [`Workspace`]: super::Workspace
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// What to ask. May be empty: a conversation opened before anything is
    /// typed is a real state, and the check happens where a prompt is actually
    /// sent — see [`Workspace::prepare`](super::Workspace::prepare).
    pub prompt: String,
    pub session_name: String,
    /// How hard the model should think. `None` leaves the provider's default;
    /// unsupported provider/model levels fail instead of being downgraded.
    pub effort: Option<Effort>,
    /// What the run may spend — deadline, tool budget, token budget. All
    /// unset by default, for the attended and the unattended caller alike;
    /// [`Bounds`] carries the argument (ADR-0014).
    pub bounds: Bounds,
    /// An allowance this run shares with the others drawing on it.
    ///
    /// Where [`Bounds::token_budget`] is this run's own ceiling, a
    /// [`BudgetPool`] is the job's — the one figure a fan-out spends from
    /// together. A spec carrying both stops at whichever binds first.
    ///
    /// The one field here that is shared rather than copied: deriving a second
    /// spec from this one gives it a *handle on the same pool*, which is the
    /// point. See [`BudgetPool`] for why that exception exists and what it
    /// costs.
    pub budget: Option<BudgetPool>,
    /// The complete host-owned overrides for this mint. An empty profile
    /// inherits every workspace default.
    pub profile: RunProfile,
}

impl RunSpec {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            session_name: DEFAULT_SESSION_NAME.to_string(),
            effort: None,
            bounds: Bounds::default(),
            budget: None,
            profile: RunProfile::default(),
        }
    }

    pub fn with_prompt(self, prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            ..self
        }
    }

    pub fn with_session_name(self, session_name: impl Into<String>) -> Self {
        Self {
            session_name: session_name.into(),
            ..self
        }
    }

    /// Asks the model to think harder, where the provider supports it.
    ///
    /// This is the compatibility fallback. A [`RunProfile`] carrying complete
    /// provider request options or a dedicated reasoning set/clear overrides
    /// it regardless of which builder method is called later.
    pub fn with_effort(self, effort: Effort) -> Self {
        Self {
            effort: Some(effort),
            ..self
        }
    }

    /// Gives up on the run after `deadline`. Sugar into
    /// [`bounds`](Self::bounds).
    ///
    /// Every bound here is a *graceful* end rather than a discarded run: the
    /// event stream closes the way it always does, and whatever the model
    /// committed before the bound tripped is kept. That is what makes bounding
    /// an unattended run safe to do — the alternative, throwing the work away
    /// for being one round too long, would make callers reluctant to set one.
    pub fn with_deadline(self, deadline: Duration) -> Self {
        Self {
            bounds: self.bounds.with_deadline(deadline),
            ..self
        }
    }

    /// Caps how many tool calls the run may make. Sugar into
    /// [`bounds`](Self::bounds).
    pub fn with_tool_budget(self, tool_budget: usize) -> Self {
        Self {
            bounds: self.bounds.with_tool_budget(tool_budget),
            ..self
        }
    }

    /// Caps the tokens the run may report using, input plus output. Sugar
    /// into [`bounds`](Self::bounds).
    ///
    /// Soft: the round that crosses the line is allowed to finish, because
    /// usage is only known once a round has streamed in full.
    pub fn with_token_budget(self, token_budget: u64) -> Self {
        Self {
            bounds: self.bounds.with_token_budget(token_budget),
            ..self
        }
    }

    /// Draws this run's tokens from an allowance shared with other runs.
    ///
    /// The line a fan-out is written on — [`BudgetPool::spec`] is this call with
    /// the prompt folded in:
    ///
    /// ```no_run
    /// # async fn example(workspace: &basis::Workspace) -> Result<(), basis::RunError> {
    /// # use basis::{BudgetPool, RunSpec};
    /// let pool = BudgetPool::new(500_000);
    /// let run = workspace.prepare(RunSpec::new("review the tests").with_budget(pool.clone()))?;
    /// # let _ = run;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Every turn the minted run performs draws here, not just the first, so a
    /// conversation and a one-shot are bounded the same way.
    pub fn with_budget(self, budget: BudgetPool) -> Self {
        Self {
            budget: Some(budget),
            ..self
        }
    }

    /// Applies a complete immutable host profile to this run.
    #[must_use]
    pub fn with_profile(self, profile: RunProfile) -> Self {
        Self { profile, ..self }
    }

    /// Convenience for changing only the complete resolved model metadata.
    ///
    /// Preserves every other field already present in this spec's profile.
    #[must_use]
    pub fn with_resolved_model(self, model: ModelInfo) -> Self {
        Self {
            profile: self.profile.clone().with_resolved_model(model),
            ..self
        }
    }

    /// The bounds this spec puts on every turn the run performs.
    ///
    /// Limits only. Cancellation and the graceful stop signal are per-call
    /// things a caller holds a token for, not configuration, so they stay at
    /// their defaults here and arrive through
    /// [`send_with_options`](crate::PreparedRun::send_with_options).
    pub fn turn_options(&self) -> TurnOptions {
        TurnOptions {
            bounds: self.bounds,
            budget: self.budget.clone(),
            ..TurnOptions::default()
        }
    }
}

/// A run with nothing said yet — what a protocol server opens on `session/new`.
impl Default for RunSpec {
    fn default() -> Self {
        Self::new("")
    }
}

impl From<&str> for RunSpec {
    fn from(prompt: &str) -> Self {
        Self::new(prompt)
    }
}

impl From<String> for RunSpec {
    fn from(prompt: String) -> Self {
        Self::new(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_prompt_is_a_whole_spec() {
        let spec: RunSpec = "review this diff".into();

        assert_eq!(spec.prompt, "review this diff");
        assert_eq!(spec.session_name, DEFAULT_SESSION_NAME);
        assert_eq!(spec.effort, None);
    }

    #[test]
    fn a_spec_with_nothing_said_yet_is_the_default() {
        // ACP's `session/new` opens a conversation before the user has typed
        // anything, so an empty prompt has to be expressible.
        assert_eq!(RunSpec::default().prompt, "");
    }

    #[test]
    fn builders_return_new_values() {
        let base = RunSpec::new("prompt");
        let derived = base
            .clone()
            .with_session_name("named")
            .with_effort(Effort::High);

        assert_eq!(base.session_name, DEFAULT_SESSION_NAME);
        assert_eq!(base.effort, None, "the original must be untouched");
        assert_eq!(derived.session_name, "named");
        assert_eq!(derived.effort, Some(Effort::High));
    }

    #[test]
    fn resolved_model_convenience_preserves_the_rest_of_the_profile() {
        use crate::context::SystemPrompt;

        let spec = RunSpec::new("prompt")
            .with_profile(
                RunProfile::new().with_system_prompt(SystemPrompt::Replace("host".to_string())),
            )
            .with_resolved_model(ModelInfo::new("model", "provider"));

        let debug = format!("{:?}", spec.profile);
        assert!(debug.contains("model"));
        assert!(debug.contains("<replace redacted>"));
    }

    #[test]
    fn a_run_is_unbounded_unless_the_spec_asks_for_a_bound() {
        // ADR-0014: with no scheduler shipped there is no period to default a
        // deadline from, so bounding is explicit everywhere.
        let options = RunSpec::new("prompt").turn_options();

        assert_eq!(options.bounds, Bounds::default());
    }

    #[test]
    fn every_bound_reaches_the_turn_as_configured() {
        let options = RunSpec::new("prompt")
            .with_deadline(Duration::from_secs(3_600))
            .with_tool_budget(12)
            .with_token_budget(50_000)
            .turn_options();

        assert_eq!(options.bounds.deadline, Some(Duration::from_secs(3_600)));
        assert_eq!(options.bounds.tool_budget, Some(12));
        assert_eq!(options.bounds.token_budget, Some(50_000));
    }

    #[test]
    fn a_spec_carries_no_stop_signal_of_its_own() {
        // Cancellation belongs to whoever holds the token for one call, so a
        // spec that could carry one would be handing every turn minted from it
        // the same stop button.
        let options = RunSpec::new("prompt")
            .with_deadline(Duration::from_secs(60))
            .turn_options();

        assert!(options.cancel.is_none());
        assert!(options.stop.is_none());
    }

    #[test]
    fn a_shared_allowance_reaches_the_turn_through_the_spec() {
        use crate::budget::BudgetPool;

        let pool = BudgetPool::new(500_000);
        let options = RunSpec::new("prompt")
            .with_budget(pool.clone())
            .turn_options();

        assert_eq!(options.budget, Some(pool));
    }

    #[test]
    fn deriving_a_spec_shares_the_pool_but_copies_everything_else() {
        use crate::budget::BudgetPool;

        // The exception stated as a test: a fan-out derives one spec per run and
        // every one of them has to spend from the *same* allowance, while the
        // prompts stay independent.
        let pool = BudgetPool::new(1_000);
        let template = RunSpec::new("").with_budget(pool.clone());

        let first = template.clone().with_prompt("review the tests");
        let second = template.with_prompt("review the docs");

        assert_eq!(first.budget, second.budget, "one allowance, two runs");
        assert_eq!(first.budget, Some(pool));
        assert_ne!(first.prompt, second.prompt);
    }
}
