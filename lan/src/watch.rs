//! `lan watch "<prompt>" --every 30m` — the same run, on a schedule, skipped
//! when there is nothing new to look at.
//!
//! # What this is
//!
//! A scheduler and nothing else. It contains no idea of what the prompt is
//! for: a periodic code-health check, a nightly dependency bump, and a
//! reminder to water the plants are the same code path, because task-specific
//! behaviour arrives as data — the prompt, the workspace, config — and never
//! as code here (PROPOSAL.md Bet 4). The only question this module answers is
//! *when to run the thing the caller gave us*.
//!
//! # The three decisions
//!
//! **Each iteration is a fresh run**, not another turn on one long
//! conversation — see [`schedule`](self) for why that is what makes skipping
//! defensible at all.
//!
//! **Skip when the workspace has not moved since the last successful run.**
//! [`change`] explains the fingerprint and, more importantly, why every
//! uncertain case resolves to running: a false "changed" costs tokens, a false
//! "unchanged" silently stops the feature working.
//!
//! **A failed iteration does not end the watch.** No credential this minute, a
//! workspace mid-checkout, a turn the model could not finish: each is reported
//! on the stream and the next interval comes around anyway. Surviving those is
//! the reason to run a supervisor instead of a cron line. Only a broken output
//! stream stops the loop.
//!
//! # Embedding
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use lan::{RunConfig, watch::{CollectingWatchSink, Interval, WatchConfig, watch}};
//!
//! let config = WatchConfig::new(
//!     RunConfig::new("/repo", "check for anything that regressed"),
//!     "30m".parse::<Interval>()?,
//! );
//!
//! let summary = watch(config, CollectingWatchSink::new()).await?;
//! println!("{} run(s), {} skipped", summary.ran, summary.skipped);
//! # Ok(())
//! # }
//! ```

mod change;
mod event;
mod interval;
mod schedule;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    approval::{AllowAll, ApprovalPolicy, Approver},
    run::{PreparedRun, RunConfig, RunError},
};

pub use change::{Fingerprint, Snapshot, snapshot};
pub use event::{
    CollectingWatchSink, IterationOutcome, RunReason, StopReason, WATCH_SCHEMA_VERSION, WatchEvent,
    WatchJsonlWriter, WatchRecord, WatchSink,
};
pub use interval::{Interval, IntervalError};
pub use schedule::{Shutdown, Sleeper, TokioSleeper, WatchSummary};

use event::SharedSink;
use schedule::{Collaborators, Plan};

/// Where each iteration's run comes from.
///
/// The same seam as [`acp::SessionSource`](crate::acp::SessionSource), for the
/// same two reasons: a host that already owns a mentra runtime keeps it, and a
/// test drives the whole loop against a scripted one with no network call.
///
/// The default builds a fresh runtime and session from a [`RunConfig`] every
/// iteration.
#[async_trait]
pub trait RunSource: Send + Sync + 'static {
    /// Prepares the run for `iteration`, counting from 1.
    ///
    /// Called only for an iteration that is going to run — a skipped iteration
    /// never reaches here, which is the point of skipping.
    ///
    /// An `Err` ends the iteration, never the watch.
    async fn prepare(&self, iteration: u64) -> Result<PreparedRun, RunError>;
}

/// The default source: one fresh run per iteration, from a [`RunConfig`].
struct ConfiguredRuns {
    config: RunConfig,
}

#[async_trait]
impl RunSource for ConfiguredRuns {
    async fn prepare(&self, iteration: u64) -> Result<PreparedRun, RunError> {
        // The iteration number is deliberately not woven into the run. A
        // scheduler that told the agent "this is attempt 4" would be handing
        // it scheduling vocabulary it has no use for.
        let _ = iteration;

        crate::run::prepare(self.config.clone()).await
    }
}

/// Answers approval requests, one approver per iteration.
///
/// A source rather than a single approver because [`Approver`] is consumed by
/// a run and a watch performs many; nothing in the trait promises one can be
/// reused.
pub(crate) trait ApproverSource: Send + Sync + 'static {
    fn approver(&self) -> Box<dyn Approver>;
}

/// Builds an approver by calling a closure. What [`WatchConfig::with_approver`]
/// installs.
struct FnApprovers<F>(F);

impl<F, A> ApproverSource for FnApprovers<F>
where
    F: Fn() -> A + Send + Sync + 'static,
    A: Approver,
{
    fn approver(&self) -> Box<dyn Approver> {
        Box::new((self.0)())
    }
}

/// Everything a watch needs.
///
/// Task-specific behaviour lives in `run.prompt` and in the workspace, never
/// in this struct.
pub struct WatchConfig {
    /// The run each iteration performs. Cloned per iteration, because every
    /// iteration is a fresh run.
    pub run: RunConfig,
    /// The gap between the end of one iteration and the start of the next.
    pub every: Interval,
    /// Run every iteration, without looking at the workspace at all.
    ///
    /// The escape hatch for a prompt whose answer depends on something the
    /// workspace cannot show — the clock, an upstream repository, a service.
    pub always: bool,
    /// Stop after this many iterations. Counts ticks, not runs: a skipped
    /// iteration is still an iteration, so a bound always terminates.
    pub max_iterations: Option<u64>,
    /// Trips to stop the watch. See [`Shutdown`].
    pub shutdown: Shutdown,

    source: Option<Arc<dyn RunSource>>,
    approver: Option<Arc<dyn ApproverSource>>,
    sleeper: Arc<dyn Sleeper>,
}

/// Hand-written because none of the collaborators are `Debug`, and because
/// `RunConfig` carries a whole prompt.
impl std::fmt::Debug for WatchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchConfig")
            .field("workspace", &self.run.workspace)
            .field("every", &self.every)
            .field("always", &self.always)
            .field("max_iterations", &self.max_iterations)
            .finish_non_exhaustive()
    }
}

impl WatchConfig {
    pub fn new(run: RunConfig, every: Interval) -> Self {
        Self {
            run,
            every,
            always: false,
            max_iterations: None,
            shutdown: Shutdown::new(),
            source: None,
            approver: None,
            sleeper: Arc::new(TokioSleeper),
        }
    }

    /// Runs every iteration, skipping change detection entirely.
    pub fn with_always(self, always: bool) -> Self {
        Self { always, ..self }
    }

    pub fn with_max_iterations(self, max_iterations: u64) -> Self {
        Self {
            max_iterations: Some(max_iterations),
            ..self
        }
    }

    pub fn with_shutdown(self, shutdown: Shutdown) -> Self {
        Self { shutdown, ..self }
    }

    /// Takes each iteration's run from `source` instead of building one from
    /// [`run`](Self::run).
    pub fn with_source(self, source: impl RunSource) -> Self {
        Self {
            source: Some(Arc::new(source)),
            ..self
        }
    }

    /// Answers approval requests with a fresh approver per iteration.
    ///
    /// Needed only under [`ApprovalPolicy::Prompt`]; under the other policies
    /// nothing is ever asked. Without one, a watch refuses to start under
    /// `Prompt` rather than approving on nobody's behalf — see
    /// [`WatchError::NoApprover`].
    pub fn with_approver<F, A>(self, approver: F) -> Self
    where
        F: Fn() -> A + Send + Sync + 'static,
        A: Approver,
    {
        Self {
            approver: Some(Arc::new(FnApprovers(approver))),
            ..self
        }
    }

    /// Replaces the clock the loop waits on.
    ///
    /// A scheduler's only observable behaviour is *when* it acts, so this is
    /// what makes the loop testable in milliseconds instead of hours.
    pub fn with_sleeper(self, sleeper: impl Sleeper) -> Self {
        Self {
            sleeper: Arc::new(sleeper),
            ..self
        }
    }

    fn validate(&self) -> Result<(), WatchError> {
        // With a custom source the prompt lives wherever that source keeps it,
        // so this check only applies to runs built from `run`.
        if self.source.is_none() && self.run.prompt.trim().is_empty() {
            return Err(WatchError::EmptyPrompt);
        }

        if self.run.approval == ApprovalPolicy::Prompt && self.approver.is_none() {
            return Err(WatchError::NoApprover);
        }

        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum WatchError {
    #[error("prompt is empty")]
    EmptyPrompt,

    /// A watch is unattended by construction: it may spend hours asleep, and
    /// the default approver would say yes to everything on nobody's behalf.
    /// Refusing is the only honest answer — either supply an approver with
    /// [`WatchConfig::with_approver`], or choose
    /// [`ApprovalPolicy::Always`] or [`ApprovalPolicy::Never`] and mean it.
    #[error("approval policy `prompt` needs an approver, and a watch has nobody to ask by default")]
    NoApprover,

    #[error("failed to write an event: {0}")]
    Sink(#[from] std::io::Error),

    #[error("event forwarding task failed: {0}")]
    Forwarder(#[from] tokio::task::JoinError),
}

/// Runs `config`'s prompt on a schedule until the watch is stopped or reaches
/// its bound, streaming into `sink`.
///
/// The stream always opens with [`WatchEvent::WatchStarted`] and always closes
/// with [`WatchEvent::WatchStopped`]. Between them, each iteration that runs
/// emits a complete run stream of its own, bookended exactly as `lan run
/// --json` is.
///
/// The sink is not handed back the way [`RunReport`](crate::RunReport) hands
/// back a run's. A watch has no moment at which it is finished with its
/// output, so a caller that needs to read what was written holds a second view
/// of it — which is what [`CollectingWatchSink`] is.
pub async fn watch<S: WatchSink>(config: WatchConfig, sink: S) -> Result<WatchSummary, WatchError> {
    config.validate()?;

    let plan = Plan {
        workspace: config.run.workspace.clone(),
        every: config.every,
        always: config.always,
        max_iterations: config.max_iterations,
    };

    let parts = Collaborators {
        source: config.source.clone().unwrap_or_else(|| {
            Arc::new(ConfiguredRuns {
                config: config.run.clone(),
            })
        }),
        approver: config
            .approver
            .clone()
            .unwrap_or_else(|| Arc::new(FnApprovers(|| AllowAll))),
        sleeper: config.sleeper.clone(),
        shutdown: config.shutdown.clone(),
    };

    let shared: SharedSink = Arc::new(Mutex::new(sink));

    schedule::drive(plan, shared, parts).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::DenyAll;

    /// No test here reaches a model or a real clock, so anything taking this
    /// long is stuck rather than slow.
    const NOT_STUCK: std::time::Duration = std::time::Duration::from_secs(10);

    fn config() -> WatchConfig {
        WatchConfig::new(
            RunConfig::new("/repo", "do the thing"),
            "30m".parse().expect("an interval"),
        )
    }

    #[test]
    fn a_config_carries_no_task_specific_defaults() {
        let config = config();

        assert!(!config.always);
        assert_eq!(config.max_iterations, None);
        assert_eq!(config.every.to_string(), "30m");
    }

    #[test]
    fn builders_return_new_values() {
        let base = config();
        let derived = config().with_always(true).with_max_iterations(3);

        assert!(!base.always, "the original must be untouched");
        assert!(derived.always);
        assert_eq!(derived.max_iterations, Some(3));
    }

    #[test]
    fn an_empty_prompt_is_refused_before_the_loop_starts() {
        let config = WatchConfig::new(
            RunConfig::new("/repo", "  \n "),
            "30m".parse().expect("an interval"),
        );

        assert!(matches!(config.validate(), Err(WatchError::EmptyPrompt)));
    }

    #[test]
    fn asking_for_approval_with_nobody_to_ask_is_refused() {
        let mut config = config();
        config.run = config.run.with_approval(ApprovalPolicy::Prompt);

        assert!(matches!(config.validate(), Err(WatchError::NoApprover)));
    }

    #[test]
    fn an_approver_makes_the_prompt_policy_usable() {
        let mut config = config();
        config.run = config.run.with_approval(ApprovalPolicy::Prompt);

        assert!(config.with_approver(|| DenyAll).validate().is_ok());
    }

    #[test]
    fn the_other_policies_need_no_approver() {
        for policy in [ApprovalPolicy::Always, ApprovalPolicy::Never] {
            let mut config = config();
            config.run = config.run.with_approval(policy);

            assert!(config.validate().is_ok(), "{policy:?} asks nobody anything");
        }
    }

    #[tokio::test]
    async fn a_source_supplies_the_prompt_when_the_config_has_none() {
        struct NeverPrepares;

        #[async_trait]
        impl RunSource for NeverPrepares {
            async fn prepare(&self, _iteration: u64) -> Result<PreparedRun, RunError> {
                Err(RunError::NoSuchSession)
            }
        }

        let config = WatchConfig::new(RunConfig::new("/repo", ""), "1s".parse().unwrap())
            .with_source(NeverPrepares);

        assert!(config.validate().is_ok());
    }

    #[tokio::test]
    async fn a_watch_refuses_to_start_rather_than_reporting_an_empty_stream() {
        let config = WatchConfig::new(RunConfig::new("/repo", ""), "1s".parse().unwrap());
        let sink = CollectingWatchSink::new();

        // Guarded because the failure being checked for is a refusal: a watch
        // that started anyway would sit on a real one-second interval forever,
        // and a hang looks like slowness rather than breakage.
        let error = tokio::time::timeout(NOT_STUCK, watch(config, sink.clone()))
            .await
            .expect("a refused watch must not start waiting")
            .expect_err("refused");

        assert!(matches!(error, WatchError::EmptyPrompt));
        assert!(
            sink.records().is_empty(),
            "a refused watch must not open a stream it never closes"
        );
    }
}
