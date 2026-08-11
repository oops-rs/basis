//! Many runs, one stream — end to end.
//!
//! The unit tests beside `EventFanIn` check the channel in isolation. This
//! checks the composition the fan-in exists for: real [`PreparedRun`]s driven
//! together through tagged sinks, with a scripted provider so the whole thing
//! runs offline.
//!
//! Two claims are pinned here, and the second is the sharp edge of the design:
//!
//! 1. Each run's events reach the merged stream in the run's own order, under
//!    its own tag, bookends included.
//! 2. A finished run hands its sink back inside its [`RunReport`], so a report
//!    held is a branch of the stream held open. The rustdoc says so; this makes
//!    a change that quietly altered it fail out loud.
//!
//! [`PreparedRun`]: lan_core::PreparedRun
//! [`RunReport`]: lan_core::RunReport

use std::{path::Path, time::Duration};

use lan_core::{
    ContextConfig, Event, EventFanIn, PreparedRun, RunConfig, RunOutcome, TaggedEvent,
    run::prepare_with_session,
};
use mentra::{RuntimePolicy, test::MockRuntime};

/// Long enough that a slow machine is never the reason this fails, short enough
/// that a deadlock is reported as a failure rather than as a hung CI job — which
/// is the failure mode a fan-in gets wrong.
const PATIENCE: Duration = Duration::from_secs(10);

fn workspace_with_context(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), body).expect("write AGENTS.md");
    dir
}

/// Config pinned to the given workspace, with the parent walk and the global
/// file switched off so a real `AGENTS.md` above the temp dir cannot leak in.
fn config(workspace: &Path, prompt: &str) -> RunConfig {
    RunConfig::new(workspace, prompt).with_context(ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

/// A run of its own, with its own scripted runtime.
///
/// One runtime per run rather than one shared: the mock's turn queue is popped
/// per request, so two concurrent runs against one mock would take each other's
/// script and the test would be asserting on a race. The runtime is returned
/// alongside the run because the session needs it to outlive the turn.
fn scripted_run(workspace: &Path, says: &[&str]) -> (MockRuntime, PreparedRun) {
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .stream_text(says.to_vec())
        .build()
        .expect("mock runtime builds");
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");
    let run = prepare_with_session(session, &config(workspace, "go"), "openai", "mock-model")
        .expect("prepared");

    (mock, run)
}

/// Every event one tag contributed, in the order it arrived.
fn events_tagged<'a>(seen: &'a [TaggedEvent<&str>], tag: &str) -> Vec<&'a Event> {
    seen.iter()
        .filter(|tagged| tagged.tag == tag)
        .map(|tagged| &tagged.event)
        .collect()
}

fn deltas(events: &[&Event]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            Event::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn two_runs_merge_into_one_stream_and_keep_their_own_order() {
    let workspace = workspace_with_context("Be brief.");
    let (_survey_runtime, mut survey) = scripted_run(workspace.path(), &["the ", "repo ", "works"]);
    let (_coverage_runtime, mut coverage) =
        scripted_run(workspace.path(), &["the ", "tests ", "do not"]);

    let fan = EventFanIn::new();
    let (survey_sink, coverage_sink) = (fan.sink("survey"), fan.sink("coverage"));
    let mut merged = fan.into_events();

    let runs = async move {
        let (survey, coverage) =
            tokio::join!(survey.execute(survey_sink), coverage.execute(coverage_sink));

        // Taking the answers out of the reports drops the sinks with them,
        // which is what lets the consumer below finish. A version of this that
        // returned the reports whole would deadlock against its own `join!`.
        (
            survey.expect("survey runs").final_message,
            coverage.expect("coverage runs").final_message,
        )
    };
    let watch = async {
        let mut seen = Vec::new();
        while let Some(tagged) = merged.recv().await {
            seen.push(tagged);
        }
        seen
    };

    let ((survey_answer, coverage_answer), seen) =
        tokio::time::timeout(PATIENCE, async { tokio::join!(runs, watch) })
            .await
            .expect("the merged stream ends when both runs let go of their sinks");

    assert_eq!(survey_answer.as_deref(), Some("the repo works"));
    assert_eq!(coverage_answer.as_deref(), Some("the tests do not"));

    for (tag, said) in [
        ("survey", "the repo works"),
        ("coverage", "the tests do not"),
    ] {
        let events = events_tagged(&seen, tag);

        assert!(
            matches!(events.first(), Some(Event::RunStarted { .. })),
            "{tag} must open with its own header"
        );
        assert!(
            matches!(
                events.last(),
                Some(Event::RunFinished {
                    outcome: RunOutcome::Ok,
                    ..
                })
            ),
            "{tag} must close with its own outcome"
        );
        assert_eq!(
            deltas(&events),
            said,
            "{tag}'s deltas must arrive in the order it emitted them"
        );
    }
}

#[tokio::test]
async fn a_held_report_holds_its_branch_of_the_stream_open() {
    let workspace = workspace_with_context("Be brief.");
    let (_runtime, mut run) = scripted_run(workspace.path(), &["done"]);

    let fan = EventFanIn::new();
    let sink = fan.sink("only");
    let mut merged = fan.into_events();

    let report = tokio::time::timeout(PATIENCE, run.execute(sink))
        .await
        .expect("the run completes")
        .expect("the run succeeds");

    // Everything the run said is already queued — an unbounded queue loses
    // nothing while nobody is reading.
    assert!(
        !merged.drain().is_empty(),
        "a consumer that reads after the run still gets the transcript"
    );

    // But the stream is not over, because the report handed the sink back and
    // the test is still holding it. This is the edge `MergedEvents` documents:
    // nothing can complete this `recv`, so the timeout bounds the test rather
    // than racing it.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), merged.recv())
            .await
            .is_err(),
        "a report kept alive keeps its sink, and its sink keeps the stream open"
    );

    drop(report);

    assert!(
        tokio::time::timeout(PATIENCE, merged.recv())
            .await
            .expect("the stream ends once the report is gone")
            .is_none(),
        "letting the report go is what ends the stream"
    );
}
