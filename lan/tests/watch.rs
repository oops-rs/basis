//! Assembly-level tests for the scheduler.
//!
//! Two things are checked here that a unit test cannot reach.
//!
//! **Change detection against real git.** The fingerprint's whole value rests
//! on git behaving the way this crate assumes — that `ls-files --cached
//! --others --exclude-standard` sees a new file, ignores an ignored one, and
//! that `HEAD` moves when a commit lands. Assuming any of that would be
//! exactly the kind of unverified convention `AGENTS.md` forbids, so these
//! tests run the real thing against real repositories.
//!
//! **The loop, without waiting for it.** The sleeper and the run source are
//! both injected, so a watch that is configured to wait half an hour runs its
//! whole life in microseconds against `mentra::test::MockRuntime` — no network
//! call, no cost, and the interval it *would* have waited is asserted on
//! rather than endured.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use lan::{
    Event, PreparedRun, RunConfig, RunError, RunOutcome,
    run::prepare_with_session,
    watch::{
        CollectingWatchSink, Fingerprint, Interval, IterationOutcome, RunReason, RunSource,
        Shutdown, Sleeper, Snapshot, StopReason, WatchConfig, WatchEvent, WatchJsonlWriter,
        WatchRecord, WatchSink, WatchSummary, snapshot, watch,
    },
};
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockTurn},
};
use serde_json::Value;

// ---------------------------------------------------------------- fixtures

/// Every watch here runs on an injected sleeper against a scripted runtime, so
/// no real time should pass at all — the whole suite finishes in under a
/// second. Exceeding this means the loop is stuck.
///
/// A hang is the worst failure a scheduler can have, because it reads as
/// slowness rather than as breakage: `cargo test` simply never comes back, and
/// nothing says which test is at fault. This turns that into a named failure
/// in ten seconds.
const NOT_STUCK: Duration = Duration::from_secs(10);

/// Runs a watch under the guard.
///
/// Every test goes through here rather than calling [`watch`] directly, so a
/// change that reintroduces a real wait — or a loop that stops terminating —
/// fails loudly instead of hanging.
///
/// The timeout is a backstop, not the termination argument: each test bounds
/// its own watch with `max_iterations` or a stop signal, because a loop that
/// never awaits anything pending would spin without giving the timer a chance
/// to fire.
async fn run_watch<S: WatchSink>(config: WatchConfig, sink: S) -> WatchSummary {
    tokio::time::timeout(NOT_STUCK, watch(config, sink))
        .await
        .expect("a watch on an injected clock must not hang")
        .expect("the watch runs")
}

/// A workspace with one file in it, so a fingerprint has something to see.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("README.md"), "hello").expect("write");
    dir
}

/// Config pinned to `dir`, with the parent walk and the global context file
/// switched off so a real `AGENTS.md` above the temp dir cannot leak in.
fn run_config(dir: &Path, prompt: &str) -> RunConfig {
    RunConfig::new(dir, prompt).with_context(lan::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

fn every(text: &str) -> Interval {
    text.parse().expect("an interval")
}

fn known(workspace: &Path) -> Fingerprint {
    match snapshot(workspace) {
        Snapshot::Known(fingerprint) => fingerprint,
        Snapshot::Unknown { reason } => panic!("expected a fingerprint, got: {reason}"),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        // Identity and hooks are supplied here so the test does not depend on
        // whatever the machine's global git config happens to say.
        .args(["-c", "user.email=test@example.invalid"])
        .args(["-c", "user.name=lan tests"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .expect("git runs");

    assert!(status.success(), "git {args:?} failed");
}

/// A repository with one committed file.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "--quiet"]);
    std::fs::write(dir.path().join("tracked.txt"), "one").expect("write");
    git(dir.path(), &["add", "tracked.txt"]);
    git(dir.path(), &["commit", "--quiet", "-m", "first"]);
    dir
}

/// Records what it was asked to wait for and returns at once, so a watch
/// configured for `30m` finishes in microseconds.
#[derive(Debug, Default, Clone)]
struct InstantSleeper {
    waits: Arc<Mutex<Vec<Duration>>>,
}

impl InstantSleeper {
    fn waits(&self) -> Vec<Duration> {
        self.waits.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl Sleeper for InstantSleeper {
    async fn sleep(&self, duration: Duration) {
        self.waits.lock().expect("not poisoned").push(duration);
    }
}

/// A run source backed by a scripted runtime: real sessions, real event
/// forwarding, no provider on the other end of a socket.
struct MockRuns {
    mock: MockRuntime,
    config: RunConfig,
    /// Written into the workspace before each run, to stand in for an agent
    /// that edits the files it was pointed at.
    edits: bool,
    prepared: Arc<Mutex<u64>>,
}

impl MockRuns {
    /// `turns` scripted replies, one per iteration that will run.
    fn new(config: RunConfig, turns: usize) -> Self {
        let mut builder = MockRuntime::builder()
            .model("mock-model", "openai")
            .with_policy(RuntimePolicy::permissive());

        for turn in 0..turns {
            builder = builder.push_turn(MockTurn::Text(format!("reply {turn}")));
        }

        Self {
            mock: builder.build().expect("mock runtime builds"),
            config,
            edits: false,
            prepared: Arc::new(Mutex::new(0)),
        }
    }

    fn editing_the_workspace(mut self) -> Self {
        self.edits = true;
        self
    }

    fn count(&self) -> Arc<Mutex<u64>> {
        self.prepared.clone()
    }
}

#[async_trait]
impl RunSource for MockRuns {
    async fn prepare(&self, iteration: u64) -> Result<PreparedRun, RunError> {
        *self.prepared.lock().expect("not poisoned") += 1;

        if self.edits {
            std::fs::write(
                self.config.workspace.join(format!("edit-{iteration}.txt")),
                "written by the run",
            )
            .expect("write");
        }

        // A new session each iteration, which is what the real source does:
        // an iteration is a fresh run, never a continued conversation.
        let session = self
            .mock
            .runtime()
            .create_session("watch test", self.mock.model())
            .expect("session");

        prepare_with_session(session, &self.config, "openai", "mock-model")
    }
}

/// A source that never manages to prepare anything, standing in for a missing
/// credential or a workspace that went away.
struct AlwaysFails;

#[async_trait]
impl RunSource for AlwaysFails {
    async fn prepare(&self, _iteration: u64) -> Result<PreparedRun, RunError> {
        Err(RunError::NoSuchSession)
    }
}

// ------------------------------------------------- change detection vs git

#[test]
fn a_repository_fingerprints_at_all() {
    let repo = repository();

    assert_eq!(known(repo.path()), known(repo.path()));
}

#[test]
fn an_ignored_file_does_not_count_as_a_change() {
    // The reason to ask git rather than to walk: `target/` churning must not
    // make every iteration look like work.
    let repo = repository();
    std::fs::write(repo.path().join(".gitignore"), "build/\n").expect("write");
    git(repo.path(), &["add", ".gitignore"]);
    git(repo.path(), &["commit", "--quiet", "-m", "ignore build"]);
    let before = known(repo.path());

    std::fs::create_dir(repo.path().join("build")).expect("mkdir");
    std::fs::write(repo.path().join("build/artifact.bin"), "noise").expect("write");

    assert_eq!(
        known(repo.path()),
        before,
        "an ignored file is not workspace content"
    );
}

#[test]
fn an_untracked_file_does_count_as_a_change() {
    // The mirror of the above, and the one that would silently break a watch:
    // a brand new source file is exactly what it should wake up for.
    let repo = repository();
    let before = known(repo.path());

    std::fs::write(repo.path().join("new.rs"), "fn main() {}").expect("write");

    assert_ne!(known(repo.path()), before);
}

#[test]
fn a_commit_alone_changes_the_fingerprint() {
    // `git commit` leaves the working tree's mtimes alone, so a scheme built
    // only on stat would call this unchanged. HEAD is in the digest for
    // exactly this case.
    let repo = repository();
    std::fs::write(repo.path().join("tracked.txt"), "two").expect("write");
    git(repo.path(), &["add", "tracked.txt"]);
    let before = known(repo.path());

    git(repo.path(), &["commit", "--quiet", "-m", "second"]);

    assert_ne!(known(repo.path()), before);
}

#[test]
fn a_tracked_file_deleted_changes_the_fingerprint() {
    let repo = repository();
    let before = known(repo.path());

    std::fs::remove_file(repo.path().join("tracked.txt")).expect("remove");

    assert_ne!(known(repo.path()), before);
}

#[test]
fn a_repository_and_a_plain_directory_are_told_apart() {
    // Not a nicety: if `git` were missing at runtime the walk would take over,
    // and a fingerprint from each scheme must not be able to collide.
    let repo = repository();
    let before = known(repo.path());

    std::fs::remove_dir_all(repo.path().join(".git")).expect("remove .git");

    assert_ne!(
        known(repo.path()),
        before,
        "losing the repository is a change in how the workspace is read"
    );
}

// ------------------------------------------------------------- the loop

#[tokio::test]
async fn the_first_iteration_runs_and_the_rest_are_skipped_when_nothing_moves() {
    let dir = workspace();
    let sleeper = InstantSleeper::default();
    let sink = CollectingWatchSink::new();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(MockRuns::new(run_config(dir.path(), "check"), 1))
        .with_sleeper(sleeper.clone())
        .with_max_iterations(3);

    let summary = run_watch(config, sink.clone()).await;

    assert_eq!(
        summary,
        WatchSummary {
            iterations: 3,
            ran: 1,
            skipped: 2,
            failed: 0,
            stop: StopReason::Completed,
        }
    );

    // One scripted turn was enough, which is the strongest form of the claim:
    // a second run would have panicked the mock for want of a reply.
    assert_eq!(
        reasons(&sink),
        vec![RunReason::First],
        "only the first iteration had anything to look at"
    );
}

#[tokio::test]
async fn a_changed_workspace_wakes_the_watch_up_again() {
    let dir = workspace();
    let sleeper = Bumping::new(dir.path().to_path_buf());
    let sink = CollectingWatchSink::new();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(MockRuns::new(run_config(dir.path(), "check"), 3))
        .with_sleeper(sleeper)
        .with_max_iterations(3);

    let summary = run_watch(config, sink.clone()).await;

    assert_eq!(summary.ran, 3);
    assert_eq!(summary.skipped, 0);
    assert_eq!(
        reasons(&sink),
        vec![RunReason::First, RunReason::Changed, RunReason::Changed]
    );
}

#[tokio::test]
async fn a_runs_own_edits_do_not_wake_it_up_again() {
    // The failure this guards against is a watch that never stops: the agent
    // edits the workspace, the next tick sees its own edits as a change, and
    // it runs forever. Fingerprinting *after* the run is what prevents it.
    let dir = workspace();
    let sink = CollectingWatchSink::new();
    let source = MockRuns::new(run_config(dir.path(), "check"), 1).editing_the_workspace();
    let prepared = source.count();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(source)
        .with_sleeper(InstantSleeper::default())
        .with_max_iterations(4);

    let summary = run_watch(config, sink.clone()).await;

    assert_eq!(
        summary.ran, 1,
        "the run's own edits are not a reason to run"
    );
    assert_eq!(summary.skipped, 3);
    assert_eq!(*prepared.lock().expect("not poisoned"), 1);
}

#[tokio::test]
async fn a_failed_iteration_does_not_end_the_watch_and_is_never_skipped_over() {
    // Two properties at once. The loop survives a failure, and a failure does
    // not update the baseline — so an unchanged workspace is retried rather
    // than being silently written off as "already done".
    let dir = workspace();
    let sink = CollectingWatchSink::new();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(AlwaysFails)
        .with_sleeper(InstantSleeper::default())
        .with_max_iterations(3);

    let summary = run_watch(config, sink.clone()).await;

    assert_eq!(
        summary,
        WatchSummary {
            iterations: 3,
            ran: 3,
            skipped: 0,
            failed: 3,
            stop: StopReason::Completed,
        }
    );

    let reported = sink
        .watch_events()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                WatchEvent::IterationFinished {
                    outcome: IterationOutcome::SetupFailed { .. },
                    ..
                }
            )
        })
        .count();

    assert_eq!(reported, 3, "every failure is visible on the stream");
}

#[tokio::test]
async fn change_detection_can_be_switched_off() {
    let dir = workspace();
    let sink = CollectingWatchSink::new();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(MockRuns::new(run_config(dir.path(), "check"), 3))
        .with_sleeper(InstantSleeper::default())
        .with_always(true)
        .with_max_iterations(3);

    let summary = run_watch(config, sink.clone()).await;

    assert_eq!(summary.ran, 3);
    assert_eq!(summary.skipped, 0);
    assert!(reasons(&sink).iter().all(|r| *r == RunReason::Always));

    let Some(WatchEvent::WatchStarted {
        change_detection, ..
    }) = sink.watch_events().into_iter().next()
    else {
        panic!("the stream must open with watch_started");
    };
    assert!(!change_detection, "the header must say detection is off");
}

#[tokio::test]
async fn an_unfingerprintable_workspace_runs_rather_than_skipping() {
    // A workspace that is not there is the transient case that must never be
    // mistaken for "nothing to do".
    let missing = PathBuf::from("/definitely/not/a/real/path");
    let sink = CollectingWatchSink::new();

    let config = WatchConfig::new(run_config(&missing, "check"), every("30m"))
        .with_source(AlwaysFails)
        .with_sleeper(InstantSleeper::default())
        .with_max_iterations(2);

    let summary = run_watch(config, sink.clone()).await;

    assert_eq!(summary.skipped, 0);
    assert_eq!(reasons(&sink), vec![RunReason::Unknown, RunReason::Unknown]);
}

#[tokio::test]
async fn the_scheduler_waits_the_configured_interval() {
    let dir = workspace();
    let sleeper = InstantSleeper::default();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(MockRuns::new(run_config(dir.path(), "check"), 1))
        .with_sleeper(sleeper.clone())
        .with_max_iterations(3);

    run_watch(config, CollectingWatchSink::new()).await;

    assert_eq!(
        sleeper.waits(),
        vec![Duration::from_secs(1_800); 2],
        "one wait between ticks, and none after the last"
    );
}

#[tokio::test]
async fn stopping_ends_the_watch_without_waiting_out_the_interval() {
    let dir = workspace();
    let shutdown = Shutdown::new();
    let sink = CollectingWatchSink::new();

    // Trips the signal from inside the wait, which is where a watch spends
    // almost all of its life.
    struct StopWhileWaiting(Shutdown);

    #[async_trait]
    impl Sleeper for StopWhileWaiting {
        async fn sleep(&self, _duration: Duration) {
            self.0.stop();
            // Never returns: only the shutdown arm of the select can win, so
            // a scheduler that ignored the signal would hang the test rather
            // than pass it.
            std::future::pending::<()>().await;
        }
    }

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(MockRuns::new(run_config(dir.path(), "check"), 1))
        .with_sleeper(StopWhileWaiting(shutdown.clone()))
        .with_shutdown(shutdown);

    let summary = run_watch(config, sink.clone()).await;

    assert_eq!(summary.stop, StopReason::Interrupted);
    assert_eq!(summary.ran, 1);
}

#[tokio::test]
async fn a_stop_that_lands_before_anybody_waits_is_not_lost() {
    // Regression: the underlying channel refuses a send when no receiver
    // exists yet and leaves the value untouched, so a signal tripped before
    // the loop reached its wait would be dropped and the watch would sleep
    // through its own shutdown. That is the ordinary case for a Ctrl-C during
    // the first run.
    let shutdown = Shutdown::new();
    shutdown.stop();

    tokio::time::timeout(NOT_STUCK, shutdown.stopped())
        .await
        .expect("an already-tripped signal must resolve at once");
}

#[tokio::test]
async fn a_watch_stopped_before_it_starts_does_nothing() {
    let dir = workspace();
    let shutdown = Shutdown::new();
    shutdown.stop();

    let sink = CollectingWatchSink::new();
    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(AlwaysFails)
        .with_sleeper(InstantSleeper::default())
        .with_shutdown(shutdown);

    let summary = run_watch(config, sink.clone()).await;

    assert_eq!(summary.iterations, 0);
    assert_eq!(summary.stop, StopReason::Interrupted);
    // The bookends still have to be there: a consumer reading the stream must
    // be able to finish reading it.
    assert!(matches!(
        sink.watch_events().first(),
        Some(WatchEvent::WatchStarted { .. })
    ));
    assert!(matches!(
        sink.watch_events().last(),
        Some(WatchEvent::WatchStopped { .. })
    ));
}

// ------------------------------------------------------------ the stream

#[tokio::test]
async fn each_iteration_carries_a_complete_run_stream() {
    let dir = workspace();
    let sink = CollectingWatchSink::new();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(MockRuns::new(run_config(dir.path(), "check"), 1))
        .with_sleeper(InstantSleeper::default())
        .with_max_iterations(2);

    run_watch(config, sink.clone()).await;

    let first = sink.run_events(1);
    assert!(
        matches!(first.first(), Some(Event::RunStarted { .. })),
        "an iteration opens with the run header"
    );
    assert!(
        matches!(
            first.last(),
            Some(Event::RunFinished {
                outcome: RunOutcome::Ok
            })
        ),
        "and closes with the run outcome"
    );

    assert!(
        sink.run_events(2).is_empty(),
        "a skipped iteration produces no run stream, which is why the marker is needed"
    );
}

#[tokio::test]
async fn the_jsonl_stream_is_bookended_and_tagged() {
    let dir = workspace();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(MockRuns::new(run_config(dir.path(), "check"), 1))
        .with_sleeper(InstantSleeper::default())
        .with_max_iterations(2);

    let buffer = Arc::new(Mutex::new(Vec::new()));
    run_watch(config, WatchJsonlWriter::new(SharedBuffer(buffer.clone()))).await;

    let text = String::from_utf8(buffer.lock().expect("not poisoned").clone()).expect("utf-8");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is json"))
        .collect();

    assert_eq!(
        lines.first().expect("a first line")["type"],
        "watch_started"
    );
    assert_eq!(lines.last().expect("a last line")["type"], "watch_stopped");

    let seqs: Vec<u64> = lines
        .iter()
        .map(|line| line["seq"].as_u64().expect("a sequence number"))
        .collect();
    assert_eq!(
        seqs,
        (0..lines.len() as u64).collect::<Vec<_>>(),
        "one gapless counter across the whole watch"
    );

    // Every run line says which iteration it belongs to; no scheduler line
    // pretends to be one.
    for line in &lines {
        let kind = line["type"].as_str().expect("a type");
        if kind.starts_with("run_") || kind.starts_with("assistant_") {
            assert_eq!(line["iteration"], 1, "run lines are tagged: {line}");
        }
    }

    let skipped = lines
        .iter()
        .find(|line| line["type"] == "iteration_skipped")
        .expect("the second iteration was skipped");
    assert_eq!(skipped["iteration"], 2);
    assert!(
        skipped["fingerprint"]
            .as_str()
            .is_some_and(|f| !f.is_empty()),
        "a skip says what it compared"
    );
}

#[tokio::test]
async fn the_records_interleave_in_the_order_they_happened() {
    let dir = workspace();
    let sink = CollectingWatchSink::new();

    let config = WatchConfig::new(run_config(dir.path(), "check"), every("30m"))
        .with_source(MockRuns::new(run_config(dir.path(), "check"), 1))
        .with_sleeper(InstantSleeper::default())
        .with_max_iterations(1);

    run_watch(config, sink.clone()).await;

    let shape: Vec<&'static str> = sink
        .records()
        .iter()
        .map(|record| match record {
            WatchRecord::Watch(WatchEvent::WatchStarted { .. }) => "watch_started",
            WatchRecord::Watch(WatchEvent::IterationStarted { .. }) => "iteration_started",
            WatchRecord::Watch(WatchEvent::IterationFinished { .. }) => "iteration_finished",
            WatchRecord::Watch(WatchEvent::WatchStopped { .. }) => "watch_stopped",
            WatchRecord::Watch(WatchEvent::IterationSkipped { .. }) => "iteration_skipped",
            WatchRecord::Run { .. } => "run",
        })
        .collect();

    assert_eq!(shape.first(), Some(&"watch_started"));
    assert_eq!(shape.last(), Some(&"watch_stopped"));

    let started = shape
        .iter()
        .position(|kind| *kind == "iteration_started")
        .expect("an iteration started");
    let finished = shape
        .iter()
        .position(|kind| *kind == "iteration_finished")
        .expect("an iteration finished");
    assert!(
        shape[started..finished].contains(&"run"),
        "the run stream sits inside its iteration's brackets: {shape:?}"
    );
}

// ---------------------------------------------------------------- helpers

/// The reasons the scheduler gave for each iteration it ran.
fn reasons(sink: &CollectingWatchSink) -> Vec<RunReason> {
    sink.watch_events()
        .into_iter()
        .filter_map(|event| match event {
            WatchEvent::IterationStarted { reason, .. } => Some(reason),
            _ => None,
        })
        .collect()
}

/// A sleeper that touches the workspace while it waits, standing in for
/// somebody else committing between two iterations.
struct Bumping {
    workspace: PathBuf,
    waits: Mutex<u64>,
}

impl Bumping {
    fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            waits: Mutex::new(0),
        }
    }
}

#[async_trait]
impl Sleeper for Bumping {
    async fn sleep(&self, _duration: Duration) {
        let mut waits = self.waits.lock().expect("not poisoned");
        *waits += 1;
        std::fs::write(
            self.workspace.join(format!("outside-{waits}.txt")),
            "somebody else",
        )
        .expect("write");
    }
}

/// A `Write` that a test can read back afterwards.
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuffer {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("not poisoned")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
