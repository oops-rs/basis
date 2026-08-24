//! What a run says when its token budget is what ended it.
//!
//! The unit tests in `src/run/prepared.rs` check the classification in
//! isolation. This checks the claim only a driven turn can settle: that a real
//! run, stopped by mentra at a real round boundary, arrives back in basis
//! carrying [`Bound::TokenBudget`] — and therefore exit `3` from the CLI, where
//! before it was indistinguishable from a provider failure.
//!
//! Two rounds is what makes the bound observable at all. mentra compares
//! cumulative usage against the budget only when it is about to start another
//! round, so a turn whose single round answers in prose is already done before
//! any comparison happens, however far over the line it went. The script here
//! spends its whole allowance on a *tool* round, which is the round that has to
//! be followed by another one.
//!
//! Everything runs in-process against a scripted provider: no endpoint, no
//! sockets, and the usage figures are exactly what the test says they are.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use basis::{
    Bound, CollectingSink, TurnOptions, approval::ApprovalGate, run::prepare_with_session,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session, TokenUsage,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::VolatileRuntimeStore,
};
use serde_json::json;

/// A bounded turn must end at its boundary. Exceeding this means the budget was
/// never consulted and the script ran to its end instead.
const PROMPTLY: Duration = Duration::from_secs(10);

/// What the scripted tool round reports spending. Input plus output is what a
/// budget counts, so one round of this costs 120 tokens.
const INPUT_TOKENS: u64 = 100;
const OUTPUT_TOKENS: u64 = 20;
const ROUND_COST: u64 = INPUT_TOKENS + OUTPUT_TOKENS;

/// Replays a fixed script of assistant turns, reporting usage on each as a real
/// provider does — without which no budget is ever crossed.
struct ScriptedProvider {
    model: ModelInfo,
    turns: Mutex<VecDeque<Vec<ContentBlock>>>,
    rounds: Arc<Mutex<usize>>,
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
        *self.rounds.lock().expect("not poisoned") += 1;

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
                total_tokens: Some(ROUND_COST),
                ..TokenUsage::default()
            }),
        }))
    }
}

/// A run whose first round writes a file and whose second answers in prose.
///
/// The shape a budget can stop: the tool round commits work and reports its
/// cost, and the prose round is the one mentra has to decide whether to start.
/// Returns the counter of provider requests, which is how "the second round
/// never happened" is checked.
fn scripted_write(workspace: &Path) -> (Runtime, ModelInfo, Arc<Mutex<usize>>) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let rounds = Arc::new(Mutex::new(0));
    let provider = ScriptedProvider {
        model: model.clone(),
        rounds: Arc::clone(&rounds),
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
        // Nothing here is read back after the run, so the history has nowhere
        // to be: mentra's in-memory store keeps this file off the disk apart
        // from the workspace the tool writes into.
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(ApprovalGate::new())
        .build()
        .expect("runtime builds");

    (runtime, model, rounds)
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

#[tokio::test]
async fn a_run_stopped_by_its_token_budget_names_the_budget() {
    // The whole point of the fix. mentra ends this run gracefully at the round
    // boundary, and because the last committed message is a tool result rather
    // than prose it owes its caller a final message it never got — so the turn
    // surfaces as "run completed without a final assistant message", a
    // provider-shaped failure for an accounting decision. Classifying by that
    // error alone gives exit 1 and sends someone after a broken model; the
    // runner's own record gives exit 3 and the true reason.
    let dir = workspace();
    let (runtime, model, rounds) = scripted_write(dir.path());
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_options(
            CollectingSink::new(),
            // One token: crossed by the first round's report, and by nothing
            // before it, so the boundary after that round is where this stops.
            TurnOptions::default().with_token_budget(1),
        ),
    )
    .await
    .expect("a bounded turn must end at its boundary")
    .expect("a tripped bound ends the run, it does not break it");

    assert_eq!(
        report.stopped_by,
        Some(Bound::TokenBudget),
        "the run must say the allowance is what stopped it"
    );

    // The failure this rescues, stated rather than implied: the outcome reads
    // like a provider that misbehaved, and `stopped_by` is the only thing that
    // says otherwise.
    let basis::RunOutcome::Error { message } = &report.outcome else {
        panic!("a budget crossed after a tool round leaves no final message");
    };
    assert!(
        message.contains("without a final assistant message"),
        "the error names no bound of its own: {message}"
    );

    assert_eq!(
        *rounds.lock().expect("not poisoned"),
        1,
        "the round the budget refused must never have reached the provider"
    );
    assert!(
        dir.path().join("made.txt").exists(),
        "and a graceful bound keeps the work the run had already committed"
    );
    assert_eq!(
        report.usage.total_tokens(),
        ROUND_COST,
        "the run still reports what it spent getting there"
    );
}

#[tokio::test]
async fn a_run_that_finishes_inside_its_budget_names_no_bound() {
    // The control, and the thing a false positive would break: a budget nobody
    // reached must leave `stopped_by` empty, or every bounded run in a script
    // exits 3 and every retry loop spins.
    let dir = workspace();
    let (runtime, model, rounds) = scripted_write(dir.path());
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_options(
            CollectingSink::new(),
            TurnOptions::default().with_token_budget(10 * ROUND_COST),
        ),
    )
    .await
    .expect("an unbounded-in-practice turn completes")
    .expect("run completes");

    assert!(report.succeeded());
    assert_eq!(report.stopped_by, None);
    assert_eq!(report.final_message.as_deref(), Some("all done"));
    assert_eq!(
        *rounds.lock().expect("not poisoned"),
        2,
        "both scripted rounds ran"
    );
}

#[tokio::test]
async fn a_shared_allowance_drawn_dry_stops_the_run_the_same_way() {
    // A pool is the same bound with a shared counter, so it must arrive as the
    // same answer. Worth pinning separately because the figure mentra is handed
    // is computed by basis (`turn_bound`) rather than passed through, and a pool
    // spent by a *sibling* run stops this one at its next boundary — which is
    // exactly the case where re-deriving "was the budget crossed" afterwards
    // and reading the runner's record could disagree.
    let dir = workspace();
    let (runtime, model, _rounds) = scripted_write(dir.path());
    let mut prepared = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    // Enough to take the turn — an already-spent pool is refused before the
    // prompt goes out, which is a different answer, pinned in `budget.rs`.
    let pool = basis::BudgetPool::new(1);

    let report = tokio::time::timeout(
        PROMPTLY,
        prepared.execute_with_options(
            CollectingSink::new(),
            TurnOptions::default().with_budget(pool.clone()),
        ),
    )
    .await
    .expect("a bounded turn must end at its boundary")
    .expect("a tripped bound ends the run, it does not break it");

    assert_eq!(report.stopped_by, Some(Bound::TokenBudget));
    assert_eq!(
        pool.spent(),
        ROUND_COST,
        "the round that crossed the line still drew on the pool"
    );
}
