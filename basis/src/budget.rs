//! One token allowance, spent by several runs at once.
//!
//! ADR-0010 asked for this in a sentence: "a cloneable `BudgetPool` that
//! concurrent runs draw from, on top of the per-run bounds of ADR-0014, so
//! 'this whole review costs ≤ 500k tokens' is one line". The per-run bounds
//! cannot answer that question, and neither of the two ways of faking it with
//! them is what a host meant. Dividing 500k across the twenty runs of a fan-out
//! starves the nineteen that had something to say on behalf of the one that did
//! not; giving each of them 500k buys twenty reviews at the price of one.
//!
//! What a host wants is a single figure that any number of runs spend from,
//! where a run that finishes cheaply leaves its share for the others without
//! anyone rebalancing anything. That is this.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::{
    run::{RunUsage, TurnOptions},
    workspace::RunSpec,
};

/// A token allowance several runs draw on at once.
///
/// # Shared mutable state, deliberately
///
/// Everything else a run is configured with is an immutable value: a
/// [`RunSpec`] derived from another spec leaves the original alone, and two runs
/// minted from one spec cannot influence each other. A pool is the opposite by
/// construction — twenty concurrent runs drawing on one figure *have* to see
/// each other's spending, or it is not one figure. So this is the sanctioned
/// exception: the handle is cheap to clone and every clone is the same pool,
/// because the allowance lives behind an [`Arc`] and moves under an atomic.
/// [`Clone`] here means "another handle", never "another allowance", and
/// [`PartialEq`] says so — two handles compare equal when they meter the same
/// counter against the same limit, not when they happen to name the same
/// number. A capped view shares the counter but carries its own tighter limit,
/// so it is deliberately not equal to its parent.
///
/// # There is no reservation, and none is possible
///
/// A run's token usage is only known once a round has streamed in full, so
/// nothing can be set aside for a run in advance and reconciled later without
/// guessing at the size of the guess. This does not guess. The pool *is* the
/// counter mentra keeps: attaching one to a run hands mentra the same
/// [`AtomicU64`] every other run on this pool reports into
/// ([`RunOptions::token_usage`](mentra::runtime::RunOptions), which mentra
/// documents as the way to alias one run's accounting to another's), together
/// with the pool's limit as that run's `token_budget`. Every round of every
/// drawing run adds its input+output to the one total, and every round boundary
/// in every drawing run compares that total to the limit. So [`remaining`] is
/// not an estimate that a settlement later corrects — it is the number the
/// turns are being stopped against, read live.
///
/// A run that spends nothing therefore costs the pool nothing. Nothing is held
/// back on its behalf and nothing has to be returned.
///
/// # How much it can overshoot
///
/// A pool bounds what a run *starts*, not what it finishes. Usage is known only
/// at a round boundary, so the round that crosses the line always completes —
/// that is the softness [`Bounds::token_budget`](crate::Bounds::token_budget)
/// already documents — and
/// with N runs in flight, N of them can be mid-round when the line is crossed.
/// State it as: **the pool lands at up to `limit` plus one round from each run
/// that was running when the limit was reached.** For a sequential caller that
/// is one round; for a twenty-way fan-out it is twenty. If that tail matters,
/// cap each run with [`RunSpec::with_token_budget`] as well, or fan out less
/// widely — but do not read `limit` as a ceiling.
///
/// # What it sees, and what it does not
///
/// Delegated work is inside the pool, whichever door it came through. mentra's
/// `task` intrinsic and basis's own `spawn` (ADR-0016, the door the model
/// actually holds) both drive the subagent on the parent run's
/// [`RunOptions::child`](mentra::runtime::RunOptions::child), which carries the
/// *same* accounting handle — this pool's counter — and the same bound, so a
/// fan-out whose runs delegate draws on one figure at every depth rather than
/// spending beside it. What the two doors do NOT share is the tally: `task`
/// relays its child's usage reports onto the parent's stream, so [`RunUsage`]
/// agrees with what stopped the turn — but the relay is `pub(crate)` in
/// mentra, `spawn` cannot reach it, and a `spawn`-delegating run's `RunUsage`
/// under-reports what the pool honestly charged. The bound is airtight; the
/// receipt is not. Named as an open upstream candidate in the REDESIGN ledger.
/// Before mentra `0436bae` none of the bounding held either: `task` ran its
/// child on fresh options, and a delegating fan-out spent more than this pool
/// would ever admit to.
///
/// The edge that survives is a refusal rather than an overrun. A delegation
/// issued once the pool is already crossed inherits an allowance with nothing
/// in it, does zero rounds, and fails the tool call visibly instead of
/// returning an empty success — the delegating side of the same round-boundary
/// softness described above.
///
/// What genuinely stays outside is [`RunUsage`]'s caveat and not a structural
/// one: this counts what providers *report*. One that reports nothing spends
/// nothing as far as the pool is concerned.
///
/// # Running out
///
/// A turn that draws on a pool with nothing left is refused —
/// [`RunError::BudgetExhausted`](crate::RunError::BudgetExhausted) — before the
/// prompt is sent, before the header is emitted, and before anything is
/// committed to the conversation. It is a decision, not a failure of the work,
/// and it is stated once at the point where money would be spent, so a run
/// minted while the pool was full and driven after it drained still gets it.
///
/// The alternative was to let it through with a zero budget, and it is worth
/// recording why not. mentra checks `reported >= budget`, so `Some(0)` is
/// already crossed before the first round: the run ends gracefully having done
/// nothing, and because it owes its caller a final assistant message that never
/// arrived, it surfaces as `EmptyAssistantResponse` — a provider-shaped error
/// for an accounting decision, with the user's prompt left committed to the
/// transcript. The report does name
/// [`Bound::TokenBudget`](crate::Bound::TokenBudget) now that mentra records
/// which bound ended a run, so the error is at least not mistaken for a broken
/// provider; the wasted turn and the stranded prompt are what refusing still
/// avoids. `basis/tests/budget.rs` pins that upstream behavior so the
/// reasoning stays checkable.
///
/// A run already *underway* when the pool drains is stopped rather than
/// refused: it ends at its next round boundary, keeps what it committed, and
/// reports [`Bound::TokenBudget`](crate::Bound::TokenBudget). Two answers on
/// purpose — the refusal says nothing was spent, the bound says something was.
///
/// A caller that would rather not mint at all asks first, since both readings
/// are honest live numbers:
///
/// ```
/// # use basis::BudgetPool;
/// let pool = BudgetPool::new(500_000);
/// while pool.remaining() > 20_000 {
///     // mint another run
///     # break;
/// }
/// ```
///
/// # Using one
///
/// ```no_run
/// # async fn example() -> Result<(), basis::RunError> {
/// use basis::{BudgetPool, CollectingSink, Workspace};
///
/// let workspace = Workspace::open("/repo").await?;
/// let pool = BudgetPool::new(500_000);
///
/// // Two runs, one allowance. Neither knows about the other; both stop when
/// // the pair of them has spent 500k.
/// let mut first = workspace.prepare(pool.spec("review the tests"))?;
/// let mut second = workspace.prepare(pool.spec("review the docs"))?;
/// let (a, b) = tokio::join!(
///     first.execute(CollectingSink::default()),
///     second.execute(CollectingSink::default()),
/// );
/// # let _ = (a?, b?);
///
/// println!("the job cost {} of {}", pool.spent(), pool.limit());
/// # Ok(())
/// # }
/// ```
///
/// [`remaining`]: Self::remaining
#[derive(Clone)]
pub struct BudgetPool {
    /// What the whole job may spend. Immutable: a pool's allowance is the one
    /// thing about it that does not move, and a caller who wants a different
    /// figure wants a different pool.
    limit: u64,
    /// Input+output tokens reported by every run drawing on this pool.
    ///
    /// mentra's counter, not a copy of it — this is the `Arc` handed to every
    /// [`RunOptions`](mentra::runtime::RunOptions) the pool bounds, so what
    /// [`spent`](Self::spent) reads and what stops a turn are one number.
    /// `SeqCst` throughout, matching mentra's own accesses, so basis's reads sit
    /// in the same total order as the increments they observe.
    spent: Arc<AtomicU64>,
}

impl BudgetPool {
    /// A pool with `limit` tokens in it, input plus output.
    ///
    /// The same figure [`RunUsage::total_tokens`] reports and mentra enforces:
    /// cache reads and cache writes are counted by neither, because they are
    /// priced differently everywhere and a total that mixed them would answer
    /// no question exactly.
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            spent: Arc::new(AtomicU64::new(0)),
        }
    }

    /// What this pool was created with. Never moves.
    pub const fn limit(&self) -> u64 {
        self.limit
    }

    /// A view that allows at most `additional_tokens` more spending from now.
    ///
    /// The view shares this pool's counter: spending through this pool, the
    /// view, or any sibling handle consumes the same allowance. Its stopping
    /// threshold is the smaller of this pool's limit and the current spend
    /// plus `additional_tokens`, so deriving a view can tighten an allowance
    /// but can never extend its parent. Addition saturates at [`u64::MAX`].
    ///
    /// This is a live bound, not a reservation. Concurrent sibling spending
    /// therefore leaves fewer tokens for work using the view.
    pub fn with_token_allowance(&self, additional_tokens: u64) -> Self {
        Self {
            limit: self
                .limit
                .min(self.spent().saturating_add(additional_tokens)),
            spent: Arc::clone(&self.spent),
        }
    }

    /// What every run drawing on this pool has reported spending so far.
    ///
    /// Live: a fan-out still in flight moves this between two reads.
    pub fn spent(&self) -> u64 {
        self.spent.load(Ordering::SeqCst)
    }

    /// What is left, saturating at zero.
    ///
    /// Zero rather than a negative number or a wrap, because a pool *is*
    /// overspendable — the round that crosses the line finishes, and a caller
    /// can [`record`](Self::record) more than was ever granted. "Nothing left"
    /// is the honest answer to all of those; [`spent`](Self::spent) against
    /// [`limit`](Self::limit) is where the size of the overshoot is legible.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.spent())
    }

    /// Whether a turn drawing on this pool would be refused.
    ///
    /// True the moment reported spending reaches the limit — the same
    /// comparison mentra makes at a round boundary, so this and the thing that
    /// stops a turn cannot disagree.
    pub fn is_exhausted(&self) -> bool {
        self.remaining() == 0
    }

    /// One run's worth of intent, bounded by this pool.
    ///
    /// The shorthand the fan-out is written in: `workspace.prepare(pool.spec(p))`
    /// mints a run every turn of which draws here.
    pub fn spec(&self, prompt: impl Into<String>) -> RunSpec {
        RunSpec::new(prompt).with_budget(self.clone())
    }

    /// Turn options that bound a single call to this pool and say nothing else.
    ///
    /// For the turns a spec does not cover: a second prompt on a conversation,
    /// or a call that also carries a stop token —
    /// [`TurnOptions::with_budget`] composes with the rest.
    pub fn bounds(&self) -> TurnOptions {
        TurnOptions::default().with_budget(self.clone())
    }

    /// Charges the pool for spending it did not meter itself, returning the new
    /// total.
    ///
    /// **Not a settlement.** A run this pool bounded has already been counted,
    /// round by round, as it ran — passing its [`RunReport::usage`] here would
    /// bill the job twice, and so would passing a delegated subagent's usage,
    /// which now reports into this same counter on its own. This is for work
    /// that spent against the same allowance without ever drawing on the pool:
    /// a run bounded some other way, or a call the host made itself.
    ///
    /// Saturates rather than wrapping, so an absurd figure cannot roll the
    /// counter over and hand the job a fresh allowance.
    ///
    /// [`RunReport::usage`]: crate::RunReport::usage
    pub fn record(&self, usage: RunUsage) -> u64 {
        let tokens = usage.total_tokens();
        let previous = self
            .spent
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |spent| {
                Some(spent.saturating_add(tokens))
            })
            // The closure returns `Some` unconditionally, so the update cannot
            // fail; `fetch_update` retries on contention rather than giving up.
            .unwrap_or_else(|spent| spent);

        previous.saturating_add(tokens)
    }

    /// The accounting handle to give mentra, so a run reports into this pool.
    pub(crate) fn counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.spent)
    }

    /// The `token_budget` a turn drawing on this pool carries, given whatever
    /// per-turn cap the caller also asked for.
    ///
    /// mentra has one counter and one bound per run, and with the pool's counter
    /// installed that bound is read against the *job's* cumulative total. So a
    /// bare pool bound is the limit itself, and a per-turn cap of `n` becomes
    /// "stop once the job total reaches what it was when this turn started, plus
    /// `n`" — which is the caller's cap measured in the pool's terms.
    ///
    /// One consequence, and it is the conservative direction: siblings spending
    /// concurrently push the job total up too, so a per-turn cap under a busy
    /// pool trips at or before the point it would have alone, never after.
    /// Whichever of the two bounds is tighter is the one that binds, which is
    /// what a caller who set both meant.
    pub(crate) fn turn_bound(&self, per_turn: Option<u64>) -> u64 {
        match per_turn {
            Some(cap) => self.limit.min(self.spent().saturating_add(cap)),
            None => self.limit,
        }
    }
}

/// Prints the figures rather than the handle, so a `RunSpec` or `TurnOptions`
/// carrying a pool debugs into something a caller can read.
impl std::fmt::Debug for BudgetPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BudgetPool")
            .field("limit", &self.limit)
            .field("spent", &self.spent())
            .field("remaining", &self.remaining())
            .finish()
    }
}

/// Accounting identity plus bound: two handles are equal when they share one
/// counter and stop it at the same limit.
///
/// Comparing the numbers instead would make two independent 500k allowances
/// equal while they are both untouched and unequal a moment later, which
/// describes no useful question. Comparing only the counter would make a
/// tighter [`BudgetPool::with_token_allowance`] view equal to its parent even
/// though the two stop at different thresholds. This definition lets
/// [`RunSpec`] keep its derived `PartialEq`: two specs are equal only when their
/// accounting and bound are both the same.
impl PartialEq for BudgetPool {
    fn eq(&self, other: &Self) -> bool {
        self.limit == other.limit && Arc::ptr_eq(&self.spent, &other.spent)
    }
}

impl Eq for BudgetPool {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool that has reported spending `tokens`.
    fn spent(tokens: u64) -> RunUsage {
        RunUsage {
            input_tokens: tokens,
            output_tokens: 0,
            // Counted by nobody, and here to prove it: a pool that drew these
            // down would be enforcing a figure mentra never checks.
            cache_read_tokens: 1_000,
            cache_creation_tokens: 1_000,
            reasoning_tokens: 1_000,
            thoughts_tokens: 1_000,
            model_responses: 1,
        }
    }

    #[test]
    fn a_new_pool_holds_its_whole_allowance() {
        let pool = BudgetPool::new(500_000);

        assert_eq!(pool.limit(), 500_000);
        assert_eq!(pool.spent(), 0);
        assert_eq!(pool.remaining(), 500_000);
        assert!(!pool.is_exhausted());
    }

    #[test]
    fn recording_usage_draws_the_pool_down() {
        let pool = BudgetPool::new(1_000);

        assert_eq!(pool.record(spent(300)), 300, "the new total comes back");
        assert_eq!(pool.remaining(), 700);
        assert_eq!(
            pool.spent(),
            300,
            "cache tokens are counted by neither mentra nor the pool"
        );
    }

    #[test]
    fn a_pool_that_is_overspent_reports_nothing_left_rather_than_wrapping() {
        // Overspending is normal, not exceptional: the round that crosses the
        // line finishes, so the last report is routinely larger than what was
        // left. A `remaining()` that wrapped here would hand the job a fresh
        // allowance at exactly the moment it ran out of one.
        let pool = BudgetPool::new(1_000);
        pool.record(spent(1_500));

        assert_eq!(pool.remaining(), 0);
        assert!(pool.is_exhausted());
        assert_eq!(pool.spent(), 1_500, "the overshoot stays legible");
    }

    #[test]
    fn recording_saturates_instead_of_rolling_over() {
        let pool = BudgetPool::new(1_000);
        pool.record(spent(u64::MAX));

        assert_eq!(pool.record(spent(u64::MAX)), u64::MAX);
        assert_eq!(pool.remaining(), 0, "still out, not suddenly flush");
    }

    #[test]
    fn a_pool_is_exhausted_the_moment_its_limit_is_reached() {
        // mentra's comparison is `reported >= budget`, and basis's has to be the
        // same one or a turn would be refused at a total mentra would have let
        // through — or worse, let through at one mentra refuses.
        let pool = BudgetPool::new(100);
        pool.record(spent(100));

        assert!(pool.is_exhausted());
        assert_eq!(pool.remaining(), 0);
    }

    #[test]
    fn a_pool_with_no_allowance_is_exhausted_from_the_start() {
        assert!(BudgetPool::new(0).is_exhausted());
    }

    #[test]
    fn a_clone_is_another_handle_on_one_allowance() {
        // The whole point of the type, and the one place the immutable-builder
        // style is deliberately not followed.
        let pool = BudgetPool::new(1_000);
        let handle = pool.clone();

        handle.record(spent(400));

        assert_eq!(pool.spent(), 400, "one pool, seen through two handles");
        assert_eq!(pool.remaining(), handle.remaining());
        assert_eq!(pool, handle);
    }

    #[test]
    fn a_token_allowance_view_shares_spending_without_extending_its_parent() {
        let pool = BudgetPool::new(1_000);
        pool.record(spent(200));

        let view = pool.with_token_allowance(300);
        let sibling = pool.clone();
        sibling.record(spent(250));

        assert_eq!(view.limit(), 500, "the allowance starts at current spend");
        assert_eq!(view.spent(), 450, "the view reads the shared counter");
        assert_eq!(view.remaining(), 50, "sibling usage consumes the view");
        assert_eq!(pool.remaining(), 550, "the parent keeps its outer bound");
        assert_ne!(pool, view, "a tighter bound is not the same pool view");

        let view = pool.with_token_allowance(u64::MAX);
        assert_eq!(view.limit(), 1_000);
        assert_eq!(
            view.remaining(),
            550,
            "a large allowance cannot extend the parent"
        );

        let saturated = BudgetPool::new(u64::MAX);
        saturated.record(spent(u64::MAX - 10));
        let view = saturated.with_token_allowance(100);

        assert_eq!(view.limit(), u64::MAX, "addition cannot wrap the bound");
        assert_eq!(view.remaining(), 10);
    }

    #[test]
    fn two_pools_of_the_same_size_are_not_the_same_pool() {
        let one = BudgetPool::new(1_000);
        let two = BudgetPool::new(1_000);

        one.record(spent(400));

        assert_ne!(one, two, "equal figures are not one allowance");
        assert_eq!(
            two.remaining(),
            1_000,
            "and spending one does not spend two"
        );
    }

    #[test]
    fn a_bare_pool_bounds_a_turn_at_its_limit() {
        // No per-turn cap: the turn stops when the *job* reaches the limit,
        // which is the whole reason the counter is shared.
        let pool = BudgetPool::new(500_000);
        pool.record(spent(200_000));

        assert_eq!(pool.turn_bound(None), 500_000);
    }

    #[test]
    fn a_per_turn_cap_is_measured_from_what_the_job_has_already_spent() {
        // mentra compares the cap against the shared cumulative total, so a cap
        // of 50k on a pool that has already spent 200k has to arrive as 250k or
        // it would be crossed before the turn began.
        let pool = BudgetPool::new(500_000);
        pool.record(spent(200_000));

        assert_eq!(pool.turn_bound(Some(50_000)), 250_000);
    }

    #[test]
    fn a_per_turn_cap_can_only_tighten_the_pool_bound() {
        // A cap larger than what the pool has left must not raise the job's
        // allowance — the pool is the outer bound and stays the outer bound.
        let pool = BudgetPool::new(1_000);
        pool.record(spent(900));

        assert_eq!(pool.turn_bound(Some(10_000)), 1_000);
    }

    #[test]
    fn a_pool_hands_out_specs_and_options_that_draw_on_it() {
        // The two shapes a caller attaches a pool with: a whole spec for a run
        // being minted, options for a turn on a run that already exists. Both
        // have to name *this* pool, or a fan-out would quietly meter nothing.
        let pool = BudgetPool::new(1_000);

        let spec = pool.spec("review the diff");
        assert_eq!(spec.prompt, "review the diff");
        assert_eq!(spec.budget, Some(pool.clone()));
        assert_eq!(pool.bounds().budget, Some(pool.clone()));

        // And say nothing else, so they compose with whatever the caller sets.
        assert_eq!(spec.bounds.deadline, None);
        assert_eq!(spec.bounds.token_budget, None);
        assert!(pool.bounds().cancel.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_draws_and_records_lose_nothing() {
        // The claim the type exists to make: N runs reporting into one figure
        // while N others read it produce an exact total, not an approximate
        // one. A non-atomic tally would drop increments here.
        let pool = BudgetPool::new(100_000);

        let writers = (0..32).map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                for _ in 0..100 {
                    pool.record(spent(10));
                    // Readers race the writers, which is what a caller's
                    // `while pool.remaining() > threshold` loop is doing.
                    assert!(pool.spent() <= 32_000);
                }
            })
        });

        for writer in writers.collect::<Vec<_>>() {
            writer.await.expect("a writer finishes");
        }

        assert_eq!(pool.spent(), 32_000);
        assert_eq!(pool.remaining(), 68_000);
    }

    #[test]
    fn a_pool_can_be_shared_across_tasks() {
        // Asserted at compile time: a pool that was not `Send + Sync` could not
        // reach the concurrent runs it exists to bound.
        const fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<BudgetPool>();
    }
}
