//! Compacting a conversation because someone asked, not because it grew.
//!
//! Auto-compaction is mentra's and fires on a threshold; this is the other
//! entry — a person deciding *now*, with an instruction about what to keep.
//! Both are the same summarizing pass, which is a model call, so everything
//! here runs against a loopback endpoint that answers one and records what it
//! was asked.
//!
//! The interesting claim is about the *stream*. mentra installs its
//! agent-event forwarder for the duration of the pass (`Session::compact`),
//! so the compaction's announcement pair — and, since mentra 0.26, one usage
//! report per provider sample — reach the session's stream exactly as they do
//! when a threshold fires; basis subscribes first and drains after — see
//! `PreparedRun::compact`.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
};

use basis::{
    AllowAll, Bound, CancellationToken, CollectingSink, Compaction, ContextConfig, Event,
    MemoryConfig, RunError, RunFailure, Runtime, TurnOptions, Workspace, WorkspaceBuilder,
    hooks::HooksConfig, skills::SkillsConfig, store, templates::TemplatesConfig,
    tools::declared::ToolsConfig,
};
use mentra::{BuiltinProvider, ModelSelector};

#[tokio::test]
async fn compacting_a_conversation_reports_what_it_replaced_on_the_stream() {
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the scripted turn runs");

    let mut sink = CollectingSink::default();
    let compacted = run
        .compact(Some("hold on to the migration plan"), &mut sink)
        .await
        .expect("the compacting pass runs")
        .expect("a conversation with a turn in it has something to compact");

    assert!(
        compacted.replaced_items > 0,
        "a pass that replaced nothing is not a pass: {compacted:?}"
    );
    assert_eq!(
        compacted.transcript_len,
        run.history().len(),
        "the reported length has to be the transcript the next turn will send"
    );

    // Both events, in mentra's own order — the same pair its in-turn mapping
    // produces from one `ContextCompacted`, so a client cannot tell an
    // on-demand pass from an automatic one.
    let agent_id = run.agent_id().to_string();
    let events = sink.into_events();
    assert!(
        matches!(&events[0], Event::CompactionStarted { agent_id: id } if *id == agent_id),
        "{events:?}"
    );
    match &events[1] {
        Event::CompactionCompleted {
            agent_id: id,
            replaced_items,
            transcript_len,
            ..
        } => {
            assert_eq!(*id, agent_id);
            assert_eq!(*replaced_items, compacted.replaced_items);
            assert_eq!(*transcript_len, compacted.transcript_len);
        }
        other => panic!("expected a completed compaction, got {other:?}"),
    }
    // This endpoint's summarizing response carries no `usage` field, and a
    // provider that reports no usage emits no usage line: absence stays
    // absence, never a guessed zero. The reporting half of the contract is
    // `a_summarizing_pass_reports_its_provider_usage_on_the_stream`.
    assert_eq!(
        events.len(),
        2,
        "no usage was reported, so nothing else may appear: {events:?}"
    );
}

#[tokio::test]
async fn a_summarizing_pass_reports_its_provider_usage_on_the_stream() {
    // The pass is a billed model call, and since mentra 0.26 its exact
    // provider usage follows the announcement pair as an ordinary usage
    // event — one per provider sample, which on this local summarizing path
    // is exactly one. The stream is the account for a standalone pass: there
    // is no RunReport on this verb to carry a figure, so a caller metering
    // `/compact` sums these lines.
    let endpoint = ScriptedEndpoint::start_reporting_usage();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the scripted turn runs");

    let mut sink = CollectingSink::default();
    run.compact(None, &mut sink)
        .await
        .expect("the compacting pass runs")
        .expect("there is older history to summarize");

    let events = sink.into_events();
    assert!(
        matches!(&events[0], Event::CompactionStarted { .. }),
        "{events:?}"
    );
    assert!(
        matches!(&events[1], Event::CompactionCompleted { .. }),
        "{events:?}"
    );
    match &events[2] {
        // No agent-id assertion: mentra's usage report deliberately carries
        // none to attribute by (see `RunUsage`'s aggregate-accounting note).
        Event::Usage {
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(
                (*input_tokens, *output_tokens),
                (SUMMARY_PROMPT_TOKENS, SUMMARY_COMPLETION_TOKENS),
                "the usage line carries what the provider said, verbatim"
            );
        }
        other => panic!("expected the pass's own usage after its pair, got {other:?}"),
    }
    assert_eq!(
        events.len(),
        3,
        "one sample was reported, so exactly one usage line: {events:?}"
    );
}

#[tokio::test]
async fn the_instruction_is_added_to_what_the_summarizer_is_already_told() {
    // The knob's whole point, and the half a caller cannot see from the return
    // value: "keep the migration plan" has to reach the summarizing request
    // *alongside* mentra's standing continuity requirements rather than in
    // place of them.
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the scripted turn runs");

    run.compact(
        Some("hold on to the migration plan"),
        &mut CollectingSink::default(),
    )
    .await
    .expect("the compacting pass runs");

    let asked = endpoint.requests().join("\n");
    assert!(
        asked.contains("hold on to the migration plan"),
        "the caller's instruction never reached the summarizer: {asked}"
    );
    assert!(
        asked.contains("compaction"),
        "and it must arrive inside mentra's own compaction instructions: {asked}"
    );
}

#[tokio::test]
async fn a_compaction_that_fails_says_so_on_the_stream() {
    // The dual of the test above, and the case that was silent. A summarizing
    // pass is a model call, so it can be refused; the caller learns that from
    // the `Err`, and a client watching the stream learned nothing at all —
    // neither mentra's `Session::compact` nor basis said a word, so a person
    // who pressed "compact" saw a conversation that did not shrink and no
    // reason why.
    let endpoint = ScriptedEndpoint::start_refusing_compaction();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the ordinary turn is still answered");

    let mut sink = CollectingSink::default();
    let failure = run
        .compact(None, &mut sink)
        .await
        .expect_err("the summarizing call is refused");

    let events = sink.into_events();
    match &events[..] {
        [Event::Error { message, .. }] => assert!(
            failure.to_string().contains(message.as_str()),
            "the stream must carry the failure the caller was handed: \
             {message:?} against {failure}"
        ),
        other => panic!("expected one error on the stream, got {other:?}"),
    }
}

#[tokio::test]
async fn a_compaction_past_its_deadline_never_reaches_the_summarizer() {
    // Bounding the pass is what makes a `/compact` behind a UI abandonable:
    // it is a full provider round trip over the longest transcript the
    // conversation has ever had, which is exactly when someone reaches for
    // stop. The deadline is checked before the request goes out, so the proof
    // is on the wire — the endpoint is never asked a second time.
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the scripted turn runs");

    let transcript_before = run.history().len();
    let asked_before = endpoint.requests().len();

    let mut sink = CollectingSink::default();
    let failure = run
        .compact_with_options(
            None,
            &mut sink,
            TurnOptions::default()
                .with_absolute_deadline(SystemTime::now() - Duration::from_secs(1)),
        )
        .await
        .expect_err("a pass past its deadline does not run");

    assert!(
        matches!(
            failure,
            RunError::Runtime(mentra::error::RuntimeError::DeadlineExceeded)
        ),
        "the bound must reach the caller as the bound, not as a summarizer \
         failure: {failure:?}"
    );
    assert_eq!(
        run.history().len(),
        transcript_before,
        "an abandoned pass must leave the conversation exactly as it found it"
    );
    assert_eq!(
        endpoint.requests().len(),
        asked_before,
        "and must not have spent a model call on its way to giving up"
    );

    // One line, and it reads as the bound rather than as a refused
    // summarizer: `recoverable` is false because waiting and trying again is
    // not what a deadline calls for.
    match &sink.into_events()[..] {
        [
            Event::Error {
                recoverable,
                message,
            },
        ] => {
            assert!(!recoverable, "a bound is not something to retry into");
            assert_eq!(message, "deadline exceeded");
        }
        other => panic!("expected one bound on the stream, got {other:?}"),
    }
}

#[tokio::test]
async fn a_cancelled_compaction_reports_the_cancel_rather_than_a_summarizer_failure() {
    // The other bound, and the one a person actually trips. What a client
    // needs to be able to tell apart is "the summarizer refused you" from "you
    // asked for this to stop" — the first is worth surfacing as a problem, the
    // second is the stop button working.
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the scripted turn runs");

    let transcript_before = run.history().len();
    let asked_before = endpoint.requests().len();

    let (options, token) = TurnOptions::cancellable();
    token.cancel();

    let mut sink = CollectingSink::default();
    let failure = run
        .compact_with_options(Some("keep the migration plan"), &mut sink, options)
        .await
        .expect_err("an already-cancelled pass does not run");

    assert!(
        matches!(
            failure,
            RunError::Runtime(mentra::error::RuntimeError::Cancelled)
        ),
        "{failure:?}"
    );
    assert_eq!(run.history().len(), transcript_before);
    assert_eq!(endpoint.requests().len(), asked_before);

    match &sink.into_events()[..] {
        [
            Event::Error {
                recoverable,
                message,
            },
        ] => {
            assert!(!recoverable);
            assert_eq!(message, "operation cancelled");
        }
        other => panic!("expected one cancellation on the stream, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unbounded_pass_is_what_compact_still_asks_for() {
    // `compact_with_options` is additive: the older verb is it with nothing
    // attached, and a conversation on a run whose config names no deadline
    // compacts exactly as it always has.
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the scripted turn runs");

    let compacted = run
        .compact_with_options(None, &mut CollectingSink::default(), TurnOptions::default())
        .await
        .expect("an unbounded pass runs")
        .expect("there is older history to summarize");

    assert!(compacted.replaced_items > 0, "{compacted:?}");
}

#[tokio::test]
async fn the_older_verb_inherits_the_deadline_the_run_was_configured_with() {
    // The other half of "additive", and the half that is a behaviour change:
    // `compact` is `compact_with_options` with nothing attached, and nothing
    // attached is filled in from `with_bounds` — so a run configured with a
    // deadline now has one on its manual passes too, where before 0.24 they
    // ran to completion whatever the run was allowed. Asserted through the old
    // verb on purpose: a refactor that stopped merging `self.bounds` would
    // leave every `compact_with_options` test above passing.
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    // Configured *after* the turn, because a deadline already in the past
    // would have refused the turn that gives the pass something to summarize.
    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the scripted turn runs");
    let mut run = run.with_bounds(
        TurnOptions::default().with_absolute_deadline(SystemTime::now() - Duration::from_secs(1)),
    );

    let transcript_before = run.history().len();
    let asked_before = endpoint.requests().len();

    let mut sink = CollectingSink::default();
    let failure = run
        .compact(Some("keep the migration plan"), &mut sink)
        .await
        .expect_err("a run already past its deadline does not get a summarizing pass");

    assert!(
        matches!(
            failure,
            RunError::Runtime(mentra::error::RuntimeError::DeadlineExceeded)
        ),
        "{failure:?}"
    );
    assert_eq!(run.history().len(), transcript_before);
    assert_eq!(
        endpoint.requests().len(),
        asked_before,
        "a pass refused by a bound is a pass the provider never hears about"
    );

    match &sink.into_events()[..] {
        [
            Event::Error {
                recoverable,
                message,
            },
        ] => {
            assert!(!recoverable);
            assert_eq!(message, "deadline exceeded");
        }
        other => panic!("expected one deadline on the stream, got {other:?}"),
    }
}

#[tokio::test]
async fn a_deadline_reached_inside_an_automatic_pass_is_the_runs_own_bound() {
    // The half of 0.24 basis does not call: auto-compaction happens *inside* a
    // turn and now inherits that turn's bounds, so a run can end on a bound it
    // reached while summarizing rather than while answering. What has to hold
    // is that basis reports it as the same bound it reports anywhere else — a
    // script that reads `stopped_by` to decide whether to retry with more time
    // must not be told "the provider broke" because the time ran out during a
    // summary.
    let endpoint = ScriptedEndpoint::start_stalling_compaction();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_compaction(eager_compaction())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the first turn has nothing older to summarize and is answered");

    let report = tokio::time::timeout(
        PROMPTLY,
        run.send_with_options(
            "again",
            CollectingSink::default(),
            basis::AllowAll,
            TurnOptions::default().with_deadline(DEADLINE_INSIDE_THE_STALL),
        ),
    )
    .await
    .expect("a bounded pass must not wait for a summarizer that never answers")
    .expect("a bound ends the run, it does not break it");

    assert_eq!(
        report.stopped_by,
        Some(Bound::Deadline),
        "the run's own bound, named — not a summarizer failure: {report:?}"
    );
    assert!(matches!(
        report.failure.as_ref(),
        Some(RunFailure::DeadlineExceeded)
    ));
    assert_eq!(
        endpoint.summarizing_requests(),
        1,
        "the bound has to have landed inside the pass, not before it"
    );
    assert_eq!(
        endpoint.turn_requests(),
        1,
        "and the second turn never got as far as its own model request"
    );
}

#[tokio::test]
async fn a_cancel_during_an_automatic_pass_ends_the_run_as_a_cancellation() {
    // The other bound, and the one 0.23 swallowed: a cancel inside
    // auto-compaction was degraded past as though the summarizer had merely
    // been unavailable, and the turn carried on. It now ends the run, and
    // basis reports it exactly as it reports any other cancelled turn —
    // `RunFailure::Cancelled` and deliberately *no* `Bound`, because a stop
    // somebody asked for is not an allowance the run outgrew (see `Bound`).
    let (options, token) = TurnOptions::cancellable();
    let endpoint = ScriptedEndpoint::start_cancelling_on_compaction(token);
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_compaction(eager_compaction())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the first turn has nothing older to summarize and is answered");

    let report = tokio::time::timeout(
        PROMPTLY,
        run.send_with_options("again", CollectingSink::default(), basis::AllowAll, options),
    )
    .await
    .expect("a cancelled pass must not run to completion")
    .expect("cancelling ends the run, it does not break it");

    assert!(matches!(
        report.failure.as_ref(),
        Some(RunFailure::Cancelled)
    ));
    assert_eq!(report.stopped_by, None);
    assert!(!report.succeeded());
    assert_eq!(endpoint.summarizing_requests(), 1);
    assert_eq!(
        endpoint.turn_requests(),
        1,
        "the cancelled pass ends the run rather than being degraded past, so \
         the second turn's own model request never goes out"
    );
}

#[tokio::test]
async fn a_conversation_with_nothing_to_compact_says_so_and_emits_nothing() {
    // The answer a caller gets either way. A `/compact` on a session that has
    // not spoken yet must not report a compaction that did not happen, and
    // must not leave a lone "compacting…" on a client's stream.
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(closed_port(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut sink = CollectingSink::default();
    let compacted = workspace
        .prepare("go")
        .expect("mints")
        .compact(None, &mut sink)
        .await
        .expect("an empty conversation is not an error");

    assert_eq!(compacted, None);
    assert!(
        sink.into_events().is_empty(),
        "nothing happened, so nothing is announced"
    );
}

#[tokio::test]
async fn renaming_a_session_is_what_a_later_listing_reports() {
    // mentra fixes a session's name at creation otherwise, so every ACP
    // session basis opened listed under the same placeholder.
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(closed_port(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    let agent_id = run.agent_id().to_string();
    run.set_name("the parser fix").expect("renames");

    let listed = store::list_in(store_dir.path(), dir.path()).expect("lists");
    let named = listed
        .iter()
        .find(|session| session.agent_id == agent_id)
        .expect("the conversation this workspace minted");

    assert_eq!(named.name, "the parser fix");
}

#[tokio::test]
async fn a_forgotten_conversation_is_neither_listed_nor_resumable() {
    // Both halves, because either alone would be a deletion that did not
    // delete: a row `list` still offers is one a person can pick, and a row
    // `resume` still opens is one that was never gone.
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(closed_port(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let kept = workspace
        .prepare("keep me")
        .expect("mints")
        .agent_id()
        .to_string();
    // Scoped: a live run holds the agent, and mentra's delete removes rows
    // rather than stopping anything in memory — a run still held would write
    // its row back on its next persist.
    let deleted = {
        let run = workspace.prepare("forget me").expect("mints");
        run.agent_id().to_string()
    };

    store::forget_in(store_dir.path(), &deleted).expect("deletes");

    assert_eq!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .into_iter()
            .map(|session| session.agent_id)
            .collect::<Vec<_>>(),
        vec![kept],
        "the one that was forgotten must be gone and the other must not"
    );
    assert!(
        workspace.resume(&deleted, "again").is_err(),
        "and there is nothing left to pick back up"
    );
}

#[tokio::test]
async fn forgetting_a_conversation_that_was_never_there_is_not_an_error() {
    // A caller deleting by an id it read from a list is racing anyone else
    // holding the same store, and "it is gone" is the outcome both wanted.
    let store_dir = tempfile::tempdir().expect("tempdir");

    store::forget_in(store_dir.path(), "agent-nobody-ever-minted")
        .expect("deleting nothing deletes nothing");
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A workspace whose every turn compacts first.
///
/// One token is below any real estimate, so `auto_compact_if_needed` fires
/// before every model request rather than after a transcript this endpoint
/// would have to script its way to. The *first* turn still summarizes nothing
/// — mentra protects the trailing turn, and a bare user message is the whole
/// transcript at that point, so its engine returns without asking the provider
/// anything — which is what makes the second turn's pass the only summarizing
/// request the endpoint ever sees.
fn eager_compaction() -> Compaction {
    Compaction::default()
        .with_auto_threshold_tokens(Some(1))
        // Off, so the trigger is the absolute number above and not a share of
        // a window this endpoint's wire never reports anyway.
        .with_auto_threshold_percent(None)
}

/// A bounded pass must end promptly. Exceeding this means the bound was never
/// read and the run is waiting on a summarizer that will never answer.
const PROMPTLY: Duration = Duration::from_secs(10);

/// The deadline a turn is given when the claim is that its bound was read
/// *inside* the summarizing pass.
///
/// The interval has to be long enough that everything between
/// `send_with_options` and the request landing on the endpoint — projecting
/// the history, estimating its tokens, persisting the transcript, opening a
/// TCP connection — is comfortably inside it, because a deadline that expires
/// first trips at `bounds.check()` *before* the provider call and the pass
/// never happens: the same `Bound::Deadline` for the wrong reason, and a
/// summarizing-request count of zero. Two seconds is roughly an order of
/// magnitude of headroom over that work, and still far inside both `STALL`
/// and `PROMPTLY`, so the stalled pass is provably where the bound landed.
const DEADLINE_INSIDE_THE_STALL: Duration = Duration::from_secs(2);

/// A builder that discovers nothing it was not shown.
fn offline(workspace: &Path) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        // A malformed file in the developer's own ~/.config/basis/memory must
        // never be able to fail this suite (G1); this test is not about
        // memory at all.
        .with_memory(MemoryConfig::disabled())
}

/// A runtime whose history — and whose compaction snapshots — go where the
/// test put them, rather than under the developer's own data directory.
fn runtime_at(store_dir: &Path, base_url: &str) -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_provider(BuiltinProvider::OpenAI)
            .with_api_key("test-key")
            .with_base_url(base_url)
            .with_model(ModelSelector::Id("test-model".to_string()))
            .with_store_dir(store_dir)
            .build()
            .expect("the runtime builds without contacting anything"),
    )
}

/// For the tests that must not reach a provider at all.
fn closed_port(store_dir: &Path) -> Arc<Runtime> {
    runtime_at(store_dir, "http://127.0.0.1:1/v1")
}

/// The smallest endpoint that is a finished turn, with every request kept.
struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        Self::start_with(WhenSummarizing::default())
    }

    /// Answers ordinary turns and refuses the summarizing call.
    ///
    /// Picked out by what mentra tells the summarizer it is rather than by
    /// counting requests: a turn is not guaranteed to be exactly one call to
    /// this endpoint, and a count that guessed wrong would refuse the wrong
    /// one and still look like a passing test.
    fn start_refusing_compaction() -> Self {
        Self::start_with(WhenSummarizing {
            refuse: true,
            ..WhenSummarizing::default()
        })
    }

    /// Answers the summarizing call with a `usage`-carrying final chunk, the
    /// way a real chat/completions endpoint asked for `include_usage` does.
    fn start_reporting_usage() -> Self {
        Self::start_with(WhenSummarizing {
            report_usage: true,
            ..WhenSummarizing::default()
        })
    }

    /// Never answers the summarizing call, so a pass is provably still in
    /// flight when whatever is supposed to stop it does.
    fn start_stalling_compaction() -> Self {
        Self::start_with(WhenSummarizing {
            stall: true,
            ..WhenSummarizing::default()
        })
    }

    /// Trips `token` the instant a summarizing request arrives, and then
    /// stalls — a person pressing stop with the pass underway, without the
    /// test having to guess when that is.
    fn start_cancelling_on_compaction(token: CancellationToken) -> Self {
        Self::start_with(WhenSummarizing {
            stall: true,
            cancels: Some(token),
            ..WhenSummarizing::default()
        })
    }

    fn start_with(when_summarizing: WhenSummarizing) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&requests);
        let behaviour = Arc::new(when_summarizing);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let recorded = Arc::clone(&recorded);
                let behaviour = Arc::clone(&behaviour);
                thread::spawn(move || answer(stream, &recorded, &behaviour));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            requests,
        }
    }

    /// How many summarizing calls this endpoint has been asked for.
    fn summarizing_requests(&self) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.contains(COMPACTION_SYSTEM_PROMPT))
            .count()
    }

    /// How many *turns* this endpoint has answered — completions that are not
    /// summaries, so the model listing a runtime opens with does not count.
    fn turn_requests(&self) -> usize {
        self.requests()
            .iter()
            .filter(|request| {
                request.starts_with("POST") && !request.contains(COMPACTION_SYSTEM_PROMPT)
            })
            .count()
    }

    fn runtime(&self, store_dir: &Path) -> Arc<Runtime> {
        runtime_at(store_dir, &self.base_url)
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// What this endpoint does when it is asked for a summary, as opposed to for
/// an ordinary turn. Every field is off by default, which is a plain endpoint.
#[derive(Default)]
struct WhenSummarizing {
    /// Answer with a 400 instead of a summary.
    refuse: bool,
    /// Never answer at all: the connection is held open until the test is over.
    stall: bool,
    /// Trip this the moment the request lands, before stalling on it.
    cancels: Option<CancellationToken>,
    /// Carry a `usage` object on the summary's final chunk.
    report_usage: bool,
}

/// How long a stalled summarizing request is held before the thread lets go.
///
/// Long enough that nothing under test can outwait it, and finite so a
/// listener thread is not parked forever if a test process lingers.
const STALL: Duration = Duration::from_secs(30);

/// The opening of the system prompt mentra sends its summarizer, which is what
/// makes a summarizing request recognizable from the wire.
const COMPACTION_SYSTEM_PROMPT: &str = "You are a coding-session compaction engine";

fn answer(mut stream: TcpStream, recorded: &Mutex<Vec<String>>, when: &WhenSummarizing) {
    let request = read_http_request(&mut stream);
    let summarizing = request.contains(COMPACTION_SYSTEM_PROMPT);
    let refused = summarizing && when.refuse;
    recorded
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(request);

    if summarizing {
        // Recorded first: a test asserting the pass was underway reads the
        // request log, and the token below is what ends the pass.
        if let Some(token) = &when.cancels {
            token.cancel();
        }
        if when.stall {
            thread::sleep(STALL);
            return;
        }
    }

    let response = if refused {
        let body = r#"{"error":{"message":"summarizing is not available","type":"invalid_request_error"}}"#;
        format!(
            "HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    } else {
        let body = sse_body(summarizing && when.report_usage);
        format!(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let _ = stream.write_all(response.as_bytes());
}

/// What the usage-reporting endpoint says the summary cost, pinned so the
/// stream assertion reads the same numbers off `Event::Usage`.
const SUMMARY_PROMPT_TOKENS: u64 = 1200;
const SUMMARY_COMPLETION_TOKENS: u64 = 60;

/// The smallest chat/completions stream that is a finished assistant turn.
///
/// A custom `base_url` speaks chat/completions, which is also why compaction
/// here takes mentra's *local* summarizing path: the wire declares
/// `supports_history_compaction: false`, so there is no remote `compact` call
/// to answer and the summary is asked for as an ordinary completion.
///
/// `with_usage` appends the final `usage`-carrying chunk `include_usage` asks
/// a real endpoint for; without it the stream reports no usage at all, which
/// is its own case worth pinning.
fn sse_body(with_usage: bool) -> String {
    let usage_chunk = format!(
        r#"{{"id":"chatcmpl_1","choices":[],"usage":{{"prompt_tokens":{SUMMARY_PROMPT_TOKENS},"completion_tokens":{SUMMARY_COMPLETION_TOKENS},"total_tokens":{}}}}}"#,
        SUMMARY_PROMPT_TOKENS + SUMMARY_COMPLETION_TOKENS
    );
    let mut events = vec![
        r#"{"id":"chatcmpl_1","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","content":"done"}}]}"#.to_string(),
        r#"{"id":"chatcmpl_1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#
            .to_string(),
    ];
    if with_usage {
        events.push(usage_chunk);
    }
    events.push("[DONE]".to_string());

    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// Reads a request up to the end of its declared body. Reading to
/// end-of-stream would deadlock: the client is waiting for the response.
fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    while let Ok(read) = stream.read(&mut buffer) {
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if header_end.is_none()
            && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = index + 4;
            header_end = Some(end);
            content_length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap_or_default())
                })
                .unwrap_or_default();
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            break;
        }
    }

    String::from_utf8_lossy(&bytes).into_owned()
}
