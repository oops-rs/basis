//! The loop: when to run the thing the user gave us, and when not to.
//!
//! Everything here is scheduling and nothing here is about the task. The
//! scheduler does not know whether the prompt asks for a code-health check, a
//! dependency bump, or a poem; it knows only that the workspace either moved
//! since the last successful run or did not (Bet 4).
//!
//! Two decisions are worth stating up front.
//!
//! **The first iteration runs immediately, and the interval is the gap
//! afterwards.** Waiting first would mean an operator cannot tell a working
//! watch from a broken one for half an hour. Measuring the gap from the end of
//! a run rather than from its start means a run that outlasts its own interval
//! cannot stampede: there is always a full interval of quiet between two runs.
//!
//! **An iteration is a tick, not a run.** A tick either runs or skips, and
//! [`WatchSummary`] reports both, so a bound on iterations always terminates
//! however the change detector answers.

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use mentra::runtime::CancellationToken;
use tokio::sync::watch as watch_channel;

use super::IterationBounds;
use super::{
    ApproverSource, RunSource, WatchError,
    change::{self, Fingerprint, Snapshot},
    event::{
        IterationOutcome, RunReason, SharedSink, StopReason, WATCH_SCHEMA_VERSION, WatchEvent, lock,
    },
    interval::Interval,
};
use crate::{
    approval::{ApprovalDecision, ApprovalRequest, Approver},
    event::{Event, RunOutcome},
    run::{EventSink, RunError, TurnOptions},
};

/// The stop signal for a watch.
///
/// One trip does two things, which is why it is a type rather than a flag: it
/// wakes the scheduler out of its wait, and it abandons a turn that is already
/// in flight. mentra's [`CancellationToken`] can only do the second — it is an
/// `AtomicBool` with no way to await it, and a process that spends almost all
/// of its life asleep cannot poll one — so this pairs the token a turn needs
/// with a channel the loop can wait on.
///
/// Signal handling is deliberately not done here. A library that installs a
/// SIGINT handler steals it from its host, so the binary owns that and trips
/// this; the same seam is what lets a test stop a watch without a signal at
/// all.
#[derive(Debug, Clone)]
pub struct Shutdown {
    token: CancellationToken,
    sender: Arc<watch_channel::Sender<bool>>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        let (sender, _) = watch_channel::channel(false);

        Self {
            token: CancellationToken::default(),
            sender: Arc::new(sender),
        }
    }

    /// Trips the signal. Idempotent, so a second Ctrl-C is not an error.
    pub fn stop(&self) {
        self.token.cancel();
        // `send` refuses when no receiver exists and leaves the value alone,
        // which would lose a stop that arrives before anyone waits — the
        // ordinary case for a watch interrupted during its first run.
        // `send_replace` always writes.
        self.sender.send_replace(true);
    }

    pub fn is_stopped(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Resolves when the signal is tripped, immediately if it already was.
    ///
    /// The already-tripped case is checked by the channel itself rather than
    /// by a separate test here, so there is no window in which a trip lands
    /// between the check and the wait.
    pub async fn stopped(&self) {
        let mut receiver = self.sender.subscribe();
        let _ = receiver.wait_for(|stopped| *stopped).await;
    }

    /// The token an in-flight turn is given, so tripping this abandons it.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

/// Waits between iterations.
///
/// A seam rather than a bare `tokio::time::sleep` because a scheduler whose
/// only observable behaviour is *when* it acts cannot be tested by a suite
/// that has to wait for it. A test supplies a sleeper that returns at once and
/// records what it was asked to wait for, and then asserts on the durations —
/// which checks more than a real sleep would.
#[async_trait]
pub trait Sleeper: Send + Sync + 'static {
    async fn sleep(&self, duration: Duration);
}

/// The real one.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// What one tick of the loop does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Run(RunReason),
    Skip,
}

/// The whole scheduling policy, as a pure function.
///
/// `baseline` is the fingerprint the last *successful* run left behind, and
/// `observed` is the workspace now — or `None` when change detection is off.
/// Keeping this free of clocks, models, and filesystems is what makes the
/// policy testable exhaustively rather than by inference from a running loop.
pub(crate) fn decide(baseline: Option<Fingerprint>, observed: Option<&Snapshot>) -> Step {
    let Some(observed) = observed else {
        return Step::Run(RunReason::Always);
    };

    let Some(current) = observed.fingerprint() else {
        // Cannot claim unchanged, so must not skip: a false "unchanged" is how
        // a watch silently stops working.
        return Step::Run(RunReason::Unknown);
    };

    match baseline {
        None => Step::Run(RunReason::First),
        Some(baseline) if baseline == current => Step::Skip,
        Some(_) => Step::Run(RunReason::Changed),
    }
}

/// What a finished watch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchSummary {
    /// Ticks attempted, which is always `ran + skipped`.
    pub iterations: u64,
    /// Ticks that performed a run, successful or not.
    pub ran: u64,
    pub skipped: u64,
    /// Ticks that ran and did not succeed. Always a subset of `ran`.
    pub failed: u64,
    pub stop: StopReason,
}

/// The running tallies, kept apart from [`WatchSummary`] so the public shape
/// never exists without a reason for stopping.
#[derive(Debug, Default, Clone, Copy)]
struct Counters {
    iterations: u64,
    ran: u64,
    skipped: u64,
    failed: u64,
}

impl Counters {
    const fn finish(self, stop: StopReason) -> WatchSummary {
        WatchSummary {
            iterations: self.iterations,
            ran: self.ran,
            skipped: self.skipped,
            failed: self.failed,
            stop,
        }
    }
}

/// Everything the loop needs that is not a collaborator.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub workspace: PathBuf,
    pub every: Interval,
    /// Skip change detection entirely, running every tick. Costs nothing —
    /// the workspace is not looked at at all.
    pub always: bool,
    pub max_iterations: Option<u64>,
    pub bounds: IterationBounds,
}

/// The collaborators, so `drive`'s signature stays readable.
pub(crate) struct Collaborators {
    pub source: Arc<dyn RunSource>,
    pub approver: Arc<dyn ApproverSource>,
    pub sleeper: Arc<dyn Sleeper>,
    pub shutdown: Shutdown,
}

/// Runs the loop until it is stopped or reaches its bound.
///
/// Only a broken output stream ends this with an `Err`. A failed iteration —
/// a missing credential, a workspace that went away, a turn the model could
/// not finish — is reported and the loop continues, because surviving those is
/// the entire reason a supervisor exists rather than a cron line.
pub(crate) async fn drive(
    plan: Plan,
    sink: SharedSink,
    parts: Collaborators,
) -> Result<WatchSummary, WatchError> {
    emit(
        &sink,
        WatchEvent::WatchStarted {
            schema: WATCH_SCHEMA_VERSION,
            lan: env!("CARGO_PKG_VERSION").to_string(),
            workspace: plan.workspace.clone(),
            every_ms: plan.every.as_millis(),
            change_detection: !plan.always,
            max_iterations: plan.max_iterations,
        },
    )?;

    let mut baseline: Option<Fingerprint> = None;
    let mut counters = Counters::default();

    let stop = loop {
        if let Some(reason) = stop_now(&plan, &parts.shutdown, counters) {
            break reason;
        }

        counters.iterations += 1;
        let iteration = counters.iterations;

        let observed = observe(&plan).await;

        match decide(baseline, observed.as_ref()) {
            Step::Skip => {
                counters.skipped += 1;
                emit(
                    &sink,
                    WatchEvent::IterationSkipped {
                        iteration,
                        fingerprint: baseline.map(Fingerprint::hex).unwrap_or_default(),
                    },
                )?;
            }
            Step::Run(reason) => {
                counters.ran += 1;
                emit(&sink, WatchEvent::IterationStarted { iteration, reason })?;

                let outcome = run_once(
                    &sink,
                    iteration,
                    &parts,
                    plan.bounds.turn_options(plan.every),
                )
                .await?;

                if outcome.succeeded() {
                    // Fingerprint *after* the run, and only after one that
                    // worked. Two reasons, both load-bearing:
                    //
                    // After, because a run that edits the workspace would
                    // otherwise see its own edits next tick and run again
                    // forever — a loop that never skips at all.
                    //
                    // Only after success, because the baseline answers "what
                    // did the last completed run see?". Recording it for a
                    // failed run would mean an unchanged workspace never gets
                    // retried, so one transient failure would silence the
                    // watch until a person happened to touch a file.
                    baseline = observe(&plan)
                        .await
                        .and_then(|snapshot| snapshot.fingerprint());
                } else {
                    counters.failed += 1;
                }

                emit(&sink, WatchEvent::IterationFinished { iteration, outcome })?;
            }
        }

        if let Some(reason) = stop_now(&plan, &parts.shutdown, counters) {
            break reason;
        }

        // Biased so a signal that arrives during an instant sleep still wins:
        // "stop" must never lose a coin toss.
        tokio::select! {
            biased;
            () = parts.shutdown.stopped() => break StopReason::Interrupted,
            () = parts.sleeper.sleep(plan.every.duration()) => {}
        }
    };

    emit(
        &sink,
        WatchEvent::WatchStopped {
            reason: stop,
            iterations: counters.iterations,
            ran: counters.ran,
            skipped: counters.skipped,
            failed: counters.failed,
        },
    )?;

    Ok(counters.finish(stop))
}

/// Whether the loop is finished, checked both before a tick and before a wait
/// so a bounded watch never sleeps out an interval it will not use.
fn stop_now(plan: &Plan, shutdown: &Shutdown, counters: Counters) -> Option<StopReason> {
    if shutdown.is_stopped() {
        return Some(StopReason::Interrupted);
    }

    plan.max_iterations
        .is_some_and(|max| counters.iterations >= max)
        .then_some(StopReason::Completed)
}

/// Fingerprints the workspace, unless change detection is off.
///
/// On a blocking thread because it spawns `git` and stats files, neither of
/// which belongs on a runtime worker.
async fn observe(plan: &Plan) -> Option<Snapshot> {
    if plan.always {
        return None;
    }

    let workspace = plan.workspace.clone();

    Some(
        match tokio::task::spawn_blocking(move || change::snapshot(&workspace)).await {
            Ok(snapshot) => snapshot,
            // The task can only fail by panicking or being aborted. Either way
            // the workspace is unknown, which is not the same as unchanged.
            Err(error) => Snapshot::Unknown {
                reason: format!("could not read the workspace: {error}"),
            },
        },
    )
}

/// Performs one iteration's run.
///
/// Every iteration is a **fresh** run rather than another turn on one long
/// conversation. Three reasons, and the third is what makes skip-if-unchanged
/// mean anything at all:
///
/// - a watch has no end, so carried context grows until compaction is
///   thrashing on every iteration;
/// - one iteration's wrong turn would follow every later iteration forever;
/// - a fresh run is a function of the prompt and the workspace alone. That is
///   precisely the premise the change detector rests on — "the workspace has
///   not moved, so there is nothing new to say". With history carried forward
///   the same workspace would produce different behaviour each time, and
///   skipping would be unjustifiable.
async fn run_once(
    sink: &SharedSink,
    iteration: u64,
    parts: &Collaborators,
    bounds: TurnOptions,
) -> Result<IterationOutcome, WatchError> {
    let mut prepared = match parts.source.prepare(iteration).await {
        Ok(prepared) => prepared,
        // Setup failures are the ordinary transient case — no credential yet,
        // a workspace mid-checkout, a model the provider is not serving this
        // minute. They end an iteration, never the watch.
        Err(error) => {
            return Ok(IterationOutcome::SetupFailed {
                message: error.to_string(),
            });
        }
    };

    let prompt = prepared.context().prompt.clone();
    let options = TurnOptions {
        // Tripping the signal abandons the turn rather than waiting for it, so
        // Ctrl-C during a run does not leave a process nobody can stop. The
        // turn still ends through the normal path, so the iteration's stream
        // closes with its `run_finished` line before the watch reports.
        cancel: Some(parts.shutdown.token()),
        ..bounds
    };

    let turn = prepared
        .send_with_options(
            prompt,
            IterationSink {
                sink: sink.clone(),
                iteration,
            },
            BoxedApprover(parts.approver.approver()),
            options,
        )
        .await;

    Ok(match turn {
        Ok(report) => match report.outcome {
            RunOutcome::Ok => IterationOutcome::Ok,
            RunOutcome::Error { message } => IterationOutcome::Error { message },
        },
        // A stream that cannot be written to, or a forwarding task that
        // panicked, is not something the next iteration would survive either —
        // these are the only failures that end the watch.
        Err(RunError::Sink(error)) => return Err(WatchError::Sink(error)),
        Err(RunError::Forwarder(error)) => return Err(WatchError::Forwarder(error)),
        Err(error) => IterationOutcome::SetupFailed {
            message: error.to_string(),
        },
    })
}

fn emit(sink: &SharedSink, event: WatchEvent) -> Result<(), WatchError> {
    lock(sink).watch_event(event).map_err(WatchError::Sink)
}

/// The [`EventSink`] one iteration hands to `run`, tagging every line with the
/// iteration it belongs to.
struct IterationSink {
    sink: SharedSink,
    iteration: u64,
}

impl EventSink for IterationSink {
    fn emit(&mut self, event: Event) -> std::io::Result<()> {
        lock(&self.sink).run_event(self.iteration, event)
    }
}

/// Lets a boxed approver be passed where `run` wants an owned one.
///
/// A newtype rather than `impl Approver for Box<dyn Approver>`, so watch's
/// need for a trait object does not add an impl to lan's public approval
/// surface.
struct BoxedApprover(Box<dyn Approver>);

#[async_trait]
impl Approver for BoxedApprover {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalDecision {
        self.0.approve(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Nothing under test here talks to a network, a model, or a real clock,
    /// so anything that takes this long is stuck rather than slow.
    const NOT_STUCK: Duration = Duration::from_secs(10);

    /// Awaits under the guard, so a signal that stops resolving fails by name
    /// in seconds instead of wedging the whole suite.
    async fn not_stuck<F: std::future::Future>(future: F) -> F::Output {
        tokio::time::timeout(NOT_STUCK, future)
            .await
            .expect("must not hang")
    }

    /// Two fingerprints that are definitely different, without needing a
    /// filesystem to produce them.
    fn snapshots() -> (Snapshot, Snapshot) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a"), "one").expect("write");
        let first = change::snapshot(dir.path());

        std::fs::write(dir.path().join("b"), "two").expect("write");
        let second = change::snapshot(dir.path());

        assert_ne!(first, second, "the fixture must produce two states");
        (first, second)
    }

    fn unknown() -> Snapshot {
        Snapshot::Unknown {
            reason: "gone".to_string(),
        }
    }

    #[test]
    fn the_first_iteration_always_runs() {
        let (first, _) = snapshots();

        assert_eq!(decide(None, Some(&first)), Step::Run(RunReason::First));
    }

    #[test]
    fn an_unchanged_workspace_is_skipped() {
        let (first, _) = snapshots();
        let baseline = first.fingerprint();

        assert_eq!(decide(baseline, Some(&first)), Step::Skip);
    }

    #[test]
    fn a_changed_workspace_runs() {
        let (first, second) = snapshots();

        assert_eq!(
            decide(first.fingerprint(), Some(&second)),
            Step::Run(RunReason::Changed)
        );
    }

    #[test]
    fn an_unreadable_workspace_runs_rather_than_claiming_unchanged() {
        let (first, _) = snapshots();

        assert_eq!(
            decide(first.fingerprint(), Some(&unknown())),
            Step::Run(RunReason::Unknown)
        );
    }

    #[test]
    fn detection_switched_off_never_skips() {
        let (first, _) = snapshots();

        assert_eq!(decide(None, None), Step::Run(RunReason::Always));
        assert_eq!(
            decide(first.fingerprint(), None),
            Step::Run(RunReason::Always)
        );
    }

    #[tokio::test]
    async fn a_shutdown_already_tripped_does_not_wait() {
        let shutdown = Shutdown::new();
        shutdown.stop();

        // Guarded, not bare: this hung indefinitely once already, when the
        // trip was being dropped for want of a subscriber. A hang reads as a
        // slow suite rather than a broken one, so it has to be made loud.
        not_stuck(shutdown.stopped()).await;

        assert!(shutdown.is_stopped());
        assert!(shutdown.token().is_cancelled());
    }

    #[tokio::test]
    async fn a_shutdown_tripped_later_wakes_a_waiter() {
        let shutdown = Shutdown::new();
        let waiting = shutdown.clone();
        let task = tokio::spawn(async move { waiting.stopped().await });

        shutdown.stop();

        not_stuck(task).await.expect("the waiter wakes");
    }

    #[tokio::test]
    async fn stopping_twice_is_not_an_error() {
        let shutdown = Shutdown::new();
        shutdown.stop();
        shutdown.stop();

        assert!(shutdown.is_stopped());
        not_stuck(shutdown.stopped()).await;
    }

    /// Records what it was asked to wait for and returns at once.
    #[derive(Debug, Default)]
    struct RecordingSleeper {
        waits: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl Sleeper for RecordingSleeper {
        async fn sleep(&self, duration: Duration) {
            self.waits.lock().expect("not poisoned").push(duration);
        }
    }

    #[tokio::test]
    async fn a_recording_sleeper_observes_the_interval() {
        let sleeper = RecordingSleeper::default();

        sleeper.sleep(Duration::from_secs(1_800)).await;

        assert_eq!(
            *sleeper.waits.lock().expect("not poisoned"),
            vec![Duration::from_secs(1_800)]
        );
    }
}
