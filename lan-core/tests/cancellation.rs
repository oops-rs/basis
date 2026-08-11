//! Stopping a turn that is already running.
//!
//! ADR-0010 asked for cancellation on the public API, and the property that
//! matters is not that a token exists — it is that a *turn in flight* reacts to
//! one, and that the run says afterwards what happened without anyone reading
//! an error message. A stop button that only works between turns is not a stop
//! button.
//!
//! Cancelling mid-flight is made deterministic here by an approver: mentra
//! blocks the turn until a consequential call is answered, so an approver that
//! trips the token before answering has provably cancelled a turn that was
//! underway. That is also the real scenario — a person hits stop while the
//! permission dialog is on screen.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use lan_core::{
    ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, CancellationToken, CollectingSink,
    Event, RunConfig, RunOutcome, TurnOptions, approval::ApprovalGate, run::prepare_with_session,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::SqliteRuntimeStore,
};
use serde_json::json;

mod common;

/// A cancelled turn must end promptly. Exceeding this means the token was never
/// noticed and the turn ran to completion instead.
const PROMPTLY: Duration = Duration::from_secs(10);

/// Replays a fixed script of assistant turns.
struct ScriptedProvider {
    model: ModelInfo,
    turns: Mutex<VecDeque<Vec<ContentBlock>>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let content = self
            .turns
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| vec![ContentBlock::text("done")]);

        Ok(provider_event_stream_from_response(Response {
            id: "scripted".to_string(),
            model: self.model.id.clone(),
            role: Role::Assistant,
            content,
            stop_reason: None,
            usage: None,
        }))
    }
}

/// A run whose first round writes a file — one call the gate must put to the
/// approver — and whose second would answer in prose, if it is ever reached.
///
/// Two rounds is what makes a mid-flight cancellation observable at all: mentra
/// checks the token at each round boundary, so a single-round turn would finish
/// before the question could be asked.
fn scripted_write(workspace: &Path) -> (Runtime, ModelInfo) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider {
        model: model.clone(),
        turns: Mutex::new(VecDeque::from(vec![
            vec![ContentBlock::ToolUse {
                id: "call-0".to_string(),
                name: "files".to_string(),
                input: json!({
                    "operations": [
                        { "op": "create", "path": "made.txt", "content": "hi" }
                    ]
                }),
            }],
            vec![ContentBlock::text("all done")],
        ])),
    };

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(SqliteRuntimeStore::new(
            common::scratch_store().join("runtime.sqlite"),
        ))
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(ApprovalGate::new())
        .build()
        .expect("runtime builds");

    (runtime, model)
}

fn session(runtime: &Runtime, workspace: &Path, model: ModelInfo) -> Session {
    runtime
        .create_session_with_config(
            "test",
            model,
            mentra::agent::AgentConfig {
                workspace: mentra::agent::WorkspaceConfig {
                    base_dir: workspace.to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session")
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules").expect("write AGENTS.md");
    dir
}

fn config(workspace: &Path) -> RunConfig {
    RunConfig::new(workspace, "make a file").with_context(lan_core::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

/// Trips the token the moment it is consulted, then allows the call — a person
/// pressing stop with the permission prompt in front of them.
struct CancelsWhenAsked {
    token: CancellationToken,
    asked: Arc<Mutex<usize>>,
}

#[async_trait]
impl Approver for CancelsWhenAsked {
    async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
        *self.asked.lock().expect("not poisoned") += 1;
        self.token.cancel();
        ApprovalAnswer::new(ApprovalDecision::Allow)
    }
}

#[tokio::test]
async fn a_turn_cancelled_mid_flight_reports_a_failed_run() {
    let dir = workspace();
    let (runtime, model) = scripted_write(dir.path());
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        &config(dir.path()),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let (options, token) = TurnOptions::cancellable();
    let asked = Arc::new(Mutex::new(0));

    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_approver_and_options(
            CollectingSink::new(),
            CancelsWhenAsked {
                token,
                asked: Arc::clone(&asked),
            },
            options,
        ),
    )
    .await
    .expect("a cancelled turn must not run to completion")
    .expect("cancelling ends the run, it does not break it");

    assert_eq!(
        *asked.lock().expect("not poisoned"),
        1,
        "the token was tripped while the turn was blocked on the approver, \
         which is what makes this a mid-flight cancellation"
    );
    assert!(!report.succeeded());
    assert_eq!(report.final_message, None);

    // Deliberately *not* a `Bound`. A deadline or a tool budget is an allowance
    // the run was given and used up, and a script that retried on one would be
    // right to; a cancelled run was told to stop by whoever asked for it, and
    // retrying it would undo their decision. See `run::Bound`.
    assert_eq!(report.stopped_by, None);

    let events = report.sink.into_events();
    assert!(matches!(events.first(), Some(Event::RunStarted { .. })));
    assert!(
        matches!(
            events.last(),
            Some(Event::RunFinished {
                outcome: RunOutcome::Error { .. }
            })
        ),
        "a cancelled turn must still close the stream a client is reading"
    );
}

#[tokio::test]
async fn a_token_already_tripped_stops_the_turn_before_it_starts() {
    // What ACP does when `session/cancel` lands between arming the token and
    // sending the prompt: the turn must not go out to the provider at all.
    let dir = workspace();
    let (runtime, model) = scripted_write(dir.path());
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        &config(dir.path()),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let (options, token) = TurnOptions::cancellable();
    token.cancel();

    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_options(CollectingSink::new(), options),
    )
    .await
    .expect("an already-cancelled turn must return at once")
    .expect("cancelling ends the run, it does not break it");

    assert!(!report.succeeded());
    assert_eq!(report.stopped_by, None);
    assert!(
        !dir.path().join("made.txt").exists(),
        "nothing the scripted turn would have done may happen"
    );
}

#[tokio::test]
async fn a_second_turn_is_unaffected_by_the_first_turns_token() {
    // A token belongs to one call, which is why it never lived on `RunConfig`.
    // If it leaked onto the run, the follow-up prompt would die on arrival.
    let dir = workspace();
    let (runtime, model) = scripted_write(dir.path());
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        &config(dir.path()),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let (options, token) = TurnOptions::cancellable();
    token.cancel();
    let cancelled = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_options(CollectingSink::new(), options),
    )
    .await
    .expect("returns at once")
    .expect("reports rather than erroring");
    assert!(!cancelled.succeeded());

    let second = tokio::time::timeout(
        PROMPTLY,
        prepared.send("try again", CollectingSink::new(), lan_core::AllowAll),
    )
    .await
    .expect("the second turn must not inherit the first turn's stop button")
    .expect("run completes");

    assert!(second.succeeded());
}

/// Trips the graceful-stop token when consulted: a caller deciding, from what
/// it has read on the stream, that the run has done enough.
struct StopsWhenAsked {
    token: CancellationToken,
}

#[async_trait]
impl Approver for StopsWhenAsked {
    async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
        self.token.cancel();
        ApprovalAnswer::new(ApprovalDecision::Allow)
    }
}

/// Pins upstream behavior lan does not currently get to choose, so that a
/// change to it is noticed here rather than in someone's workflow.
///
/// A graceful stop is supposed to be the opposite of a cancellation: end at the
/// next round boundary, keep what was committed, report success. It keeps the
/// work — the file is written and stays written, and the transcript is not
/// rolled back. But when the stop lands after a *tool* round, mentra's turn ends
/// with a tool result as its last committed message, and `Agent::run` requires
/// an assistant message to hand back; it returns `EmptyAssistantResponse`, and
/// lan reports the run as failed.
///
/// So `stop` today means "graceful" only when the round it stops after produced
/// prose. This is an upstream candidate under ADR-0005, not a lan defect to work
/// around: papering over it here would mean lan deciding, from the outside, that
/// some of mentra's failures are really successes.
#[tokio::test]
async fn a_graceful_stop_after_a_tool_round_keeps_its_work_but_reports_failure() {
    let dir = workspace();
    let (runtime, model) = scripted_write(dir.path());
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        &config(dir.path()),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let (options, token) = TurnOptions::stoppable();
    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_approver_and_options(
            CollectingSink::new(),
            StopsWhenAsked { token },
            options,
        ),
    )
    .await
    .expect("a stopped turn must not run to completion")
    .expect("stopping ends the run, it does not break it");

    assert!(
        dir.path().join("made.txt").exists(),
        "a graceful stop keeps the work the run had already committed"
    );
    assert!(
        !report.succeeded(),
        "and today reports it as a failure anyway — see this test's docs"
    );
    assert_eq!(
        report.stopped_by, None,
        "stopping is not one of the run's own bounds"
    );
}
