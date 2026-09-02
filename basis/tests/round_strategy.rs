//! Steering a turn from the outside: a `RoundStrategy` on `TurnOptions`.
//!
//! The unit tests in `src/run/turn.rs` prove the strategy a caller attaches is
//! the object mentra receives. This suite drives real turns and pins what only
//! a driven turn can settle: an injected round reaches the provider as a
//! prompt before the next round, a mid-run adjustment changes what the next
//! request asks for, and — the interplay worth being precise about — a
//! strategy's *stop* reports no [`Bound`] while the bounds a strategy cannot
//! override still report as their own.
//!
//! Everything runs in-process against a scripted provider that records what
//! each request carried: no endpoint, no sockets, and the usage figures are
//! exactly what the test says they are.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use basis::{
    AllowAll, Bound, CollectingSink, ContentBlock, Effort, ModelInfo, ReasoningChange,
    ReasoningOptions, RoundAdjustment, RoundBoundary, RoundContext, RoundDecision, RoundStrategy,
    RunOutcome, TurnOptions, approval::ApprovalGate, run::prepare_with_session,
};
use mentra::{
    BuiltinProvider, Role, Runtime, RuntimePolicy, Session, TokenUsage,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::VolatileRuntimeStore,
};
use serde_json::json;

/// A steered turn must end at a boundary. Exceeding this means a decision was
/// never consulted and the script ran to its end instead.
const PROMPTLY: Duration = Duration::from_secs(10);

/// What every scripted round reports spending, so a budget test has a real
/// figure to cross. Input plus output is what a budget counts.
const INPUT_TOKENS: u64 = 100;
const OUTPUT_TOKENS: u64 = 20;

/// What one provider request carried, kept for assertions: the model asked
/// for, the reasoning options on the wire, and the user-role texts — which is
/// where an injected corrective turn has to show up.
#[derive(Clone, Debug)]
struct SeenRequest {
    reasoning: Option<ReasoningOptions>,
    user_texts: Vec<String>,
}

/// Replays a fixed script of assistant turns, recording each request and
/// reporting usage on each response as a real provider does.
struct ScriptedProvider {
    model: ModelInfo,
    turns: Mutex<VecDeque<Vec<ContentBlock>>>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.seen.lock().expect("not poisoned").push(SeenRequest {
            reasoning: request.provider_request_options.reasoning.clone(),
            user_texts: request
                .messages
                .iter()
                .filter(|message| message.role == Role::User)
                .map(|message| message.text())
                .collect(),
        });

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
            usage: Some(TokenUsage {
                input_tokens: Some(INPUT_TOKENS),
                output_tokens: Some(OUTPUT_TOKENS),
                total_tokens: Some(INPUT_TOKENS + OUTPUT_TOKENS),
                ..TokenUsage::default()
            }),
        }))
    }
}

/// A runtime whose provider replays `turns` and records what it was asked.
fn scripted(
    workspace: &Path,
    turns: Vec<Vec<ContentBlock>>,
) -> (Runtime, ModelInfo, Arc<Mutex<Vec<SeenRequest>>>) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        model: model.clone(),
        turns: Mutex::new(turns.into()),
        seen: Arc::clone(&seen),
    };

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        // Nothing here is read back after the run, so the history has nowhere
        // to be: mentra's in-memory store keeps this suite off the disk apart
        // from the workspace a tool round writes into.
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(ApprovalGate::new())
        .build()
        .expect("runtime builds");

    (runtime, model, seen)
}

/// The round every tool-shaped script opens with: one committed write, so a
/// stop that lands after it has real kept work to point at.
fn write_round() -> Vec<ContentBlock> {
    vec![ContentBlock::ToolUse {
        id: "call-0".to_string(),
        name: "files".to_string(),
        input: json!({
            "operations": [
                { "op": "create", "path": "made.txt", "content": "hi" }
            ]
        }),
    }]
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

fn context() -> basis::ContextConfig {
    basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    }
}

/// One scripted decision per boundary; an exhausted script proceeds.
enum Play {
    /// Demand another round, appending the text as a corrective user turn.
    Inject(&'static str),
    /// Proceed, raising reasoning effort for the rounds that follow.
    RaiseEffort,
    /// End the turn gracefully at this boundary.
    Stop,
}

/// A [`RoundStrategy`] that records every boundary it is shown and replays a
/// scripted sequence of decisions — the same shape a real host's policy has,
/// minus the judgment.
struct Scripted {
    plays: Mutex<VecDeque<Play>>,
    boundaries: Mutex<Vec<RoundBoundary>>,
}

impl Scripted {
    fn new(plays: Vec<Play>) -> Arc<Self> {
        Arc::new(Self {
            plays: Mutex::new(plays.into()),
            boundaries: Mutex::new(Vec::new()),
        })
    }

    fn boundaries(&self) -> Vec<RoundBoundary> {
        self.boundaries.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl RoundStrategy for Scripted {
    async fn on_round(&self, ctx: RoundContext<'_>) -> RoundDecision {
        self.boundaries
            .lock()
            .expect("not poisoned")
            .push(ctx.boundary());
        match self.plays.lock().expect("not poisoned").pop_front() {
            None => RoundDecision::proceed(),
            Some(Play::Inject(text)) => RoundDecision::inject(vec![ContentBlock::text(text)]),
            Some(Play::RaiseEffort) => {
                RoundDecision::Continue(RoundAdjustment::new().with_reasoning(
                    ReasoningChange::Set(ReasoningOptions {
                        // basis's own level converts in, so a host switching
                        // effort mid-run never names mentra's enum.
                        effort: Some(Effort::High.into()),
                        summary: None,
                    }),
                ))
            }
            Some(Play::Stop) => RoundDecision::stop(),
        }
    }
}

#[tokio::test]
async fn a_strategy_can_demand_another_round_and_then_stop_satisfied() {
    // The "prompt before the next turn" case: the model answers, the strategy
    // is not satisfied, and its injected text arrives at the provider as a
    // user turn ahead of the round it demanded.
    let dir = workspace();
    let (runtime, model, seen) = scripted(
        dir.path(),
        vec![
            vec![ContentBlock::text("first answer")],
            vec![ContentBlock::text("second answer")],
        ],
    );
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "answer twice",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let strategy = Scripted::new(vec![Play::Inject("not enough — go again"), Play::Stop]);
    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_approver_and_options(
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default().with_round_strategy(strategy.clone()),
        ),
    )
    .await
    .expect("a steered turn must end at a boundary")
    .expect("an injected round is a normal round");

    assert!(report.succeeded());
    assert_eq!(
        report.final_message.as_deref(),
        Some("second answer"),
        "the stop at the second boundary keeps the committed answer"
    );
    assert_eq!(
        report.stopped_by, None,
        "a strategy's stop is an instruction, not a bound"
    );

    let seen = seen.lock().expect("not poisoned");
    assert_eq!(seen.len(), 2, "the injection forced exactly one more round");
    assert!(
        seen[1]
            .user_texts
            .iter()
            .any(|text| text.contains("not enough — go again")),
        "the injected correction must reach the next provider request: {:?}",
        seen[1].user_texts
    );

    // Both decisions were made at the tool-free boundary: an answer was
    // committed each time, and the strategy is what kept the run going.
    assert_eq!(
        strategy.boundaries(),
        vec![
            RoundBoundary::AssistantMessageCommitted,
            RoundBoundary::AssistantMessageCommitted,
        ]
    );
}

#[tokio::test]
async fn a_strategy_stop_after_a_tool_round_keeps_the_work_and_names_no_bound() {
    // The interplay this suite exists to pin. A strategy's Stop ends the turn
    // exactly as `TurnOptions::stop` does — gracefully, at the boundary, work
    // kept — and it inherits that path's honest caveat: after a *tool* round
    // the turn still owes a final assistant message, so it comes back failed.
    // What it must never do is claim a [`Bound`]: a bound is an allowance the
    // run outgrew and a script is right to retry one bigger, while this end
    // was somebody's decision — so `stopped_by` stays empty, deliberately,
    // rather than growing a variant for it.
    let dir = workspace();
    let (runtime, model, seen) = scripted(
        dir.path(),
        vec![write_round(), vec![ContentBlock::text("must not run")]],
    );
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let strategy = Scripted::new(vec![Play::Stop]);
    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_approver_and_options(
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default().with_round_strategy(strategy.clone()),
        ),
    )
    .await
    .expect("a steered turn must end at a boundary")
    .expect("a graceful stop ends the run, it does not break it");

    let RunOutcome::Error { message } = &report.outcome else {
        panic!("a stop after a tool round leaves no final message");
    };
    assert!(
        message.contains("without a final assistant message"),
        "the same report shape as TurnOptions::stop at this boundary: {message}"
    );
    assert_eq!(
        report.stopped_by, None,
        "a strategy's stop must not be dressed as a bound"
    );

    assert_eq!(
        seen.lock().expect("not poisoned").len(),
        1,
        "the stop halted the run before a second provider request"
    );
    assert!(
        dir.path().join("made.txt").exists(),
        "and the committed tool round is kept, not rolled back"
    );
    assert_eq!(
        strategy.boundaries(),
        vec![RoundBoundary::ToolResultsCommitted]
    );
}

#[tokio::test]
async fn a_strategy_that_proceeds_does_not_unbind_the_budget() {
    // The other half of the interplay: a strategy steers between the bounds,
    // never around them. One that waves every boundary through leaves a spent
    // token budget to refuse the next round exactly as it would with no
    // strategy attached — and the report still names the budget, because the
    // runner, not the strategy, is what decided.
    let dir = workspace();
    let (runtime, model, seen) = scripted(
        dir.path(),
        vec![write_round(), vec![ContentBlock::text("all done")]],
    );
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let strategy = Scripted::new(vec![]);
    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_approver_and_options(
            CollectingSink::new(),
            AllowAll,
            // One token: crossed by the first round's report, so the boundary
            // after the tool round is where the runner refuses to continue.
            TurnOptions::default()
                .with_round_strategy(strategy.clone())
                .with_token_budget(1),
        ),
    )
    .await
    .expect("a bounded turn must end at its boundary")
    .expect("a tripped bound ends the run, it does not break it");

    assert_eq!(
        report.stopped_by,
        Some(Bound::TokenBudget),
        "the bound still reports as its own when the strategy declined to stop"
    );
    assert_eq!(
        seen.lock().expect("not poisoned").len(),
        1,
        "the round the budget refused must never have reached the provider"
    );
    assert_eq!(
        strategy.boundaries(),
        vec![RoundBoundary::ToolResultsCommitted],
        "the strategy was consulted at the boundary and its proceed held"
    );
}

#[tokio::test]
async fn a_strategy_can_raise_the_effort_mid_run() {
    // Steering the *how* rather than the *whether*: after the tool round the
    // strategy raises reasoning effort, and the next provider request — the
    // one that answers — carries it, while the round already made stays as it
    // was asked.
    let dir = workspace();
    let (runtime, model, seen) = scripted(
        dir.path(),
        vec![write_round(), vec![ContentBlock::text("all done")]],
    );
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "make a file, then think hard",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let strategy = Scripted::new(vec![Play::RaiseEffort]);
    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_approver_and_options(
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default().with_round_strategy(strategy.clone()),
        ),
    )
    .await
    .expect("a steered turn must end at a boundary")
    .expect("an adjusted round is a normal round");

    assert!(report.succeeded());
    assert_eq!(report.final_message.as_deref(), Some("all done"));

    let seen = seen.lock().expect("not poisoned");
    assert_eq!(seen.len(), 2, "both scripted rounds ran");
    assert_eq!(
        seen[0].reasoning, None,
        "the round before the switch ran as originally asked"
    );
    assert_eq!(
        seen[1].reasoning,
        Some(ReasoningOptions {
            effort: Some(Effort::High.into()),
            summary: None,
        }),
        "the round after the switch carries the raised effort"
    );
}
