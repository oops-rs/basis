//! Asking a run for a value, end to end.
//!
//! ADR-0010 calls structured output the primitive workflows live on, so what
//! these check is not that a JSON payload survives a round trip — it is the
//! three things a workflow depends on. The value arrives typed; the stream a
//! client reads is the same stream any other turn produces; and a run that
//! answers in the wrong shape is told apart from a run that failed, because a
//! workflow retries those differently.
//!
//! The provider here answers whatever tool the request forces, which is how a
//! test avoids having to know the tool name mentra generates per call. Refusing
//! to call it — the model that answers in prose instead — is scripted too,
//! because that is the failure a schema-shaped ask actually meets in the field.
//!
//! A turn that keeps its tools (`OutputSpec::with_tools`) forces nothing, so
//! the second provider below cannot be driven by the forced choice. It finds
//! the terminal tool by the description the *caller* wrote, which basis owns and
//! mentra passes through untouched.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use basis::{
    AllowAll, Bound, CollectingSink, Event, OutputAttempt, OutputDecision, OutputSpec, PromptPart,
    RunError, RunOutcome, TurnOptions, run::prepare_with_session,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session, TokenUsage,
    ToolChoice,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::VolatileRuntimeStore,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The shape the caller asks for.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Review {
    verdict: String,
    findings: Vec<String>,
}

/// The description the caller puts on the answering tool — and, for the working
/// turn below, the only handle a test has on that tool. mentra mints the name
/// per call and a working turn forces no choice to name it in, but the
/// description is the caller's own and travels untouched.
const SUBMIT_REVIEW: &str = "call this once you have read every changed file";

/// A file in the workspace, for the round that proves a working turn can reach
/// one.
const WORKSPACE_FILE: &str = "AGENTS.md";

/// Plays a model that honours a forced tool choice.
///
/// When the request forces one tool it calls exactly that tool, so no test has
/// to know the name `run_to_output` generated. With `payload: None` it ignores
/// the forced choice and answers in prose, which is the model that never
/// produces a value at all.
struct ForcedToolProvider {
    model: ModelInfo,
    payload: Option<Value>,
    /// Reported per response, as a real provider reports it — one round's
    /// worth, not a running total.
    usage: Option<TokenUsage>,
    calls: Arc<AtomicUsize>,
}

impl ForcedToolProvider {
    fn answering(payload: Value) -> Self {
        Self {
            model: ModelInfo::new("typed-model", BuiltinProvider::Anthropic),
            payload: Some(payload),
            usage: None,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn ignoring_the_forced_tool() -> Self {
        Self {
            payload: None,
            ..Self::answering(json!({}))
        }
    }

    fn reporting_usage(self, input: u64, output: u64) -> Self {
        Self {
            usage: Some(TokenUsage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                cache_read_input_tokens: Some(1),
                cache_creation_input_tokens: Some(2),
                ..TokenUsage::default()
            }),
            ..self
        }
    }
}

#[async_trait]
impl Provider for ForcedToolProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let forced = match request.tool_choice.clone() {
            Some(ToolChoice::Tool { name }) => Some(name),
            _ => None,
        };

        let (content, stop_reason) = match (forced, self.payload.clone()) {
            (Some(name), Some(payload)) => (
                vec![ContentBlock::ToolUse {
                    id: format!("terminal-{call}"),
                    name,
                    input: payload,
                }],
                Some("tool_use".to_string()),
            ),
            _ => (vec![ContentBlock::text("I would rather explain")], None),
        };

        Ok(provider_event_stream_from_response(Response {
            id: format!("typed-{call}"),
            model: self.model.id.clone(),
            role: Role::Assistant,
            content,
            stop_reason,
            usage: self.usage.clone(),
        }))
    }
}

/// One scripted round: what the model does when the turn asks it for one.
#[derive(Clone)]
enum Say {
    /// Reads a real file through the ordinary toolset — the round a shaping
    /// turn has no tool for.
    Read,
    /// Calls the terminal tool with this payload, ending the turn.
    Answer(Value),
    /// Talks instead. A working turn is allowed to, which is exactly what it
    /// trades away for the rounds it gets.
    Prose,
}

/// What one request put in front of the model: the two things a typed turn
/// changes about a round.
#[derive(Clone, Debug)]
struct Offer {
    tools: Vec<String>,
    /// The generated answering tool, picked out by the caller's description
    /// because its name is minted per call.
    terminal: Option<String>,
    choice: Option<ToolChoice>,
}

impl Offer {
    /// Everything on the request that was not the answering tool.
    fn ordinary(&self) -> Vec<&String> {
        self.tools
            .iter()
            .filter(|name| Some(*name) != self.terminal.as_ref())
            .collect()
    }
}

/// A model that plays one scripted [`Say`] per round and records what each
/// request offered it.
///
/// Cloned before it is handed to the runtime, so a test can read the offers
/// back afterwards — the counterpart of `ForcedToolProvider`, for the turns
/// where nothing is forced.
#[derive(Clone)]
struct ScriptedModel {
    model: ModelInfo,
    rounds: Arc<Mutex<VecDeque<Say>>>,
    offers: Arc<Mutex<Vec<Offer>>>,
    /// Reported per round, as a real provider reports it.
    usage: Option<TokenUsage>,
}

impl ScriptedModel {
    fn new(rounds: Vec<Say>) -> Self {
        Self {
            model: ModelInfo::new("typed-model", BuiltinProvider::Anthropic),
            rounds: Arc::new(Mutex::new(VecDeque::from(rounds))),
            offers: Arc::new(Mutex::new(Vec::new())),
            usage: None,
        }
    }

    fn spending(self, input: u64, output: u64) -> Self {
        Self {
            usage: Some(TokenUsage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                ..TokenUsage::default()
            }),
            ..self
        }
    }

    fn offers(&self) -> Vec<Offer> {
        self.offers.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl Provider for ScriptedModel {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let terminal = request
            .tools
            .iter()
            .find(|tool| tool.description.as_deref() == Some(SUBMIT_REVIEW))
            .map(|tool| tool.name.clone());
        let round = {
            let mut offers = self.offers.lock().expect("not poisoned");
            offers.push(Offer {
                tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
                terminal: terminal.clone(),
                choice: request.tool_choice.clone(),
            });
            offers.len()
        };
        let say = self
            .rounds
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| panic!("the model was asked for an unscripted round {round}"));

        let content = match say {
            Say::Read => vec![ContentBlock::ToolUse {
                id: format!("read-{round}"),
                name: "files".to_string(),
                input: json!({ "operations": [{ "op": "read", "path": WORKSPACE_FILE }] }),
            }],
            Say::Answer(payload) => vec![ContentBlock::ToolUse {
                id: format!("answer-{round}"),
                name: terminal.expect("a typed turn's request carries the terminal tool"),
                input: payload,
            }],
            Say::Prose => vec![ContentBlock::text("I read it, and it looks fine to me")],
        };
        let calls_a_tool = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));

        Ok(provider_event_stream_from_response(Response {
            id: format!("scripted-{round}"),
            model: self.model.id.clone(),
            role: Role::Assistant,
            content,
            stop_reason: calls_a_tool.then(|| "tool_use".to_string()),
            usage: self.usage.clone(),
        }))
    }
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

/// A workspace and a run wired to `provider`, ready to be sent a prompt.
fn prepared(
    dir: &tempfile::TempDir,
    provider: ForcedToolProvider,
) -> (Runtime, basis::PreparedRun) {
    let model = provider.model.clone();
    prepared_with(dir, provider, model)
}

/// The same, for a provider that is not a [`ForcedToolProvider`].
fn prepared_with<P: Provider + 'static>(
    dir: &tempfile::TempDir,
    provider: P,
    model: ModelInfo,
) -> (Runtime, basis::PreparedRun) {
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        // Nothing here reads a conversation back, so the history has nowhere
        // to be: mentra's in-memory store keeps this suite off the disk
        // entirely rather than leaving a temp database per test behind.
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(dir.path()))
        .build()
        .expect("runtime builds");

    let run = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "review this diff",
        &context(),
        "anthropic",
        "typed-model",
    )
    .expect("prepared");

    // The runtime is handed back because dropping it would take the session's
    // provider with it.
    (runtime, run)
}

/// What the caller writes by hand. basis derives no schema (see `OutputSpec`), so
/// the descriptions here are the caller's prompt to the model, not a by-product
/// of the type above.
fn review_spec() -> OutputSpec {
    OutputSpec::new(
        "submit_review",
        SUBMIT_REVIEW,
        json!({
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "description": "ship or hold" },
                "findings": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "one line per problem worth fixing"
                }
            },
            "required": ["verdict", "findings"]
        }),
    )
}

#[tokio::test]
async fn a_validated_multipart_output_rejects_then_accepts_a_transformed_value() {
    let dir = workspace();
    let provider = ScriptedModel::new(vec![
        Say::Answer(json!({ "verdict": "draft", "findings": [] })),
        Say::Answer(json!({ "verdict": "hold", "findings": [] })),
    ])
    .spending(10, 2);
    let handle = provider.clone();
    let model = provider.model.clone();
    let (_runtime, mut run) = prepared_with(&dir, provider, model);
    let reservation = review_spec().with_tools().reserve();
    let terminal_name = reservation.tool_name().to_string();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_validator = Arc::clone(&attempts);

    let attempted = run
        .output_parts_validated_with_options::<Review, _, _, _>(
            vec![
                PromptPart::text("review this diff"),
                PromptPart::text("preserve this second prompt part"),
            ],
            reservation,
            move |candidate| {
                if attempts_for_validator.fetch_add(1, Ordering::SeqCst) == 0 {
                    assert_eq!(candidate["verdict"], "draft");
                    OutputDecision::Reject("draft is not final".to_string())
                } else {
                    assert_eq!(candidate["verdict"], "hold");
                    OutputDecision::Accept(json!({ "verdict": "ship", "findings": ["normalized"] }))
                }
            },
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default(),
        )
        .await
        .expect("the validated turn is reportable");

    let OutputAttempt::Accepted(review) = attempted.output else {
        panic!("the corrected output should be accepted");
    };
    assert_eq!(review.verdict, "ship");
    assert_eq!(review.findings, vec!["normalized"]);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(attempted.report.usage.model_responses, 2);
    assert!(attempted.report.succeeded());
    assert!(
        handle
            .offers()
            .iter()
            .all(|offer| offer.terminal.as_deref() == Some(terminal_name.as_str()))
    );
}

#[tokio::test]
async fn a_validated_output_mismatch_keeps_its_report_and_sink() {
    let dir = workspace();
    let (_runtime, mut run) = prepared(
        &dir,
        ForcedToolProvider::answering(json!({ "verdict": "hold", "findings": "many" }))
            .reporting_usage(12, 3),
    );

    let attempted = run
        .output_parts_validated_with_options::<Review, _, _, _>(
            vec![PromptPart::text("review this diff")],
            review_spec().reserve(),
            |candidate| OutputDecision::Accept(candidate.clone()),
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default(),
        )
        .await
        .expect("a mismatch still returns its report");

    assert!(matches!(attempted.output, OutputAttempt::Mismatch(_)));
    assert!(matches!(attempted.report.outcome, RunOutcome::Error { .. }));
    assert_eq!(attempted.report.usage.total_tokens(), 15);
    assert!(matches!(
        attempted.report.sink.into_events().last(),
        Some(Event::RunFinished {
            outcome: RunOutcome::Error { .. },
            ..
        })
    ));
}

#[tokio::test]
async fn a_bounded_validated_output_keeps_missing_usage_and_bound_in_its_report() {
    let dir = workspace();
    let provider = ScriptedModel::new(vec![
        Say::Read,
        Say::Answer(json!({ "verdict": "ship", "findings": [] })),
    ])
    .spending(60, 40);
    let model = provider.model.clone();
    let (_runtime, mut run) = prepared_with(&dir, provider, model);

    let attempted = run
        .output_parts_validated_with_options::<Review, _, _, _>(
            vec![PromptPart::text("read, then review")],
            review_spec().with_tools().reserve(),
            |candidate| OutputDecision::Accept(candidate.clone()),
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default().with_token_budget(100),
        )
        .await
        .expect("the bounded turn still returns its report");

    assert!(matches!(attempted.output, OutputAttempt::Missing));
    assert_eq!(attempted.report.stopped_by, Some(Bound::TokenBudget));
    assert_eq!(attempted.report.usage.total_tokens(), 100);
    assert!(attempted.report.failure.is_some());
}

#[tokio::test]
async fn a_typed_turn_hands_back_the_value_the_model_committed() {
    let dir = workspace();
    let (_runtime, mut run) = prepared(
        &dir,
        ForcedToolProvider::answering(json!({
            "verdict": "hold",
            "findings": ["the retry loop never gives up"]
        })),
    );

    let output = run
        .output::<Review, _, _>(
            "review this diff",
            review_spec(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect("the run produces a value");

    assert_eq!(
        output.value,
        Review {
            verdict: "hold".to_string(),
            findings: vec!["the retry loop never gives up".to_string()],
        }
    );
    assert!(output.report.succeeded());
}

#[tokio::test]
async fn a_typed_turn_streams_the_same_bookends_as_any_other() {
    let dir = workspace();
    let (_runtime, mut run) = prepared(
        &dir,
        ForcedToolProvider::answering(json!({ "verdict": "ship", "findings": [] })),
    );

    let output = run
        .output::<Review, _, _>(
            "review this diff",
            review_spec(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect("the run produces a value");

    // The stream contract does not bend for a typed turn: a client reading
    // events must not have to know which kind of turn it is watching.
    let events = output.report.sink.into_events();
    assert!(matches!(events.first(), Some(Event::RunStarted { .. })));
    assert!(matches!(
        events.last(),
        Some(Event::RunFinished {
            outcome: RunOutcome::Ok,
            ..
        })
    ));

    // The answer reaches the stream as the terminal tool's call, which is why
    // it is deliberately absent from `final_message`.
    assert_eq!(
        output.report.final_message, None,
        "a typed turn's answer is the value, not prose"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ToolQueued { input, .. } if input["verdict"] == "ship"
        )),
        "the payload is on the stream as the terminal call's input"
    );
}

#[tokio::test]
async fn an_answer_in_the_wrong_shape_is_told_apart_from_a_failed_run() {
    let dir = workspace();
    let (_runtime, mut run) = prepared(
        &dir,
        // Answers the forced tool, but `findings` is a string where the type
        // wants a list — the everyday way a schema-shaped ask goes wrong.
        ForcedToolProvider::answering(json!({ "verdict": "hold", "findings": "lots" })),
    );

    let failure = run
        .output::<Review, _, _>(
            "review this diff",
            review_spec(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect_err("the value does not fit the type");

    // A workflow reacts to these differently — a mismatch is worth re-asking
    // with a clearer schema, a provider failure is worth backing off — so the
    // distinction has to be in the type rather than in the message text.
    assert!(
        matches!(failure.error, RunError::OutputMismatch(_)),
        "expected a mismatch, got {:?}",
        failure.error
    );

    // And the label survives the way a host that only wants the error gets it:
    // `?` through `From`, which must not re-word what it was handed.
    let error = RunError::from(failure);
    assert!(
        matches!(error, RunError::OutputMismatch(_)),
        "the conversion re-labelled the failure: {error:?}"
    );
}

#[tokio::test]
async fn a_typed_mismatch_hands_back_the_report_the_turn_earned() {
    // The turn ran, spent tokens, wrote a whole stream, and then answered in a
    // shape `T` refused. All of that is what the run *did*, and a caller
    // charging a shared budget or deciding whether to re-ask needs it as much
    // here as on the turn that succeeded (ADR-0003).
    let dir = workspace();
    let (_runtime, mut run) = prepared(
        &dir,
        ForcedToolProvider::answering(json!({ "verdict": "hold", "findings": "lots" }))
            .reporting_usage(12, 3),
    );

    let failure = run
        .output::<Review, _, _>(
            "review this diff",
            review_spec(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect_err("the value does not fit the type");

    let report = failure.report.expect("a turn that ran has a report");
    assert_eq!(
        report.usage.total_tokens(),
        15,
        "what the failed turn spent"
    );
    assert_eq!(
        report.stopped_by, None,
        "nothing bounded this one, and the report says so rather than staying silent"
    );
    assert!(matches!(report.outcome, RunOutcome::Error { .. }));
    assert!(
        report.failure.is_none(),
        "a mismatch is basis's own verdict, not a retained mentra failure"
    );

    // The sink comes back too, so a `CollectingSink` on a failed typed turn is
    // not a stream the caller watched being written and can never read.
    assert!(matches!(
        report.sink.into_events().last(),
        Some(Event::RunFinished {
            outcome: RunOutcome::Error { .. },
            ..
        })
    ));
}

#[tokio::test]
async fn a_run_that_never_calls_the_terminal_tool_produces_no_value() {
    let dir = workspace();
    let (_runtime, mut run) = prepared(&dir, ForcedToolProvider::ignoring_the_forced_tool());

    let error = run
        .output::<Review, _, _>(
            "review this diff",
            review_spec(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect_err("prose is not an answer to a typed ask");

    // mentra reports "never called the terminal tool" and "the provider stream
    // was malformed" as the same `MalformedProviderEvent`, and basis will not
    // read error prose to separate them — so both land here. Narrowing this
    // needs an upstream variant, not a string match (ADR-0005).
    assert!(
        matches!(error.error, RunError::Runtime(_)),
        "expected a runtime failure, got {:?}",
        error.error
    );
}

#[tokio::test]
async fn a_typed_turn_reports_what_it_spent() {
    let dir = workspace();
    let (_runtime, mut run) = prepared(
        &dir,
        ForcedToolProvider::answering(json!({ "verdict": "ship", "findings": [] }))
            .reporting_usage(120, 34),
    );

    let output = run
        .output::<Review, _, _>(
            "review this diff",
            review_spec(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect("the run produces a value");

    // Usage is what a shared budget is charged against, so the typed path has
    // to report it exactly as the plain one does — a workflow that only ever
    // asks for values would otherwise be a workflow whose spending is invisible.
    assert_eq!(output.report.usage.input_tokens, 120);
    assert_eq!(output.report.usage.output_tokens, 34);
    assert_eq!(output.report.usage.total_tokens(), 154);
    assert_eq!(output.report.usage.model_responses, 1);
}

#[tokio::test]
async fn usage_is_summed_across_every_round_of_a_turn() {
    let dir = workspace();
    let calls = Arc::new(Mutex::new(0_usize));

    // Two rounds: the model calls a workspace tool, then answers the forced
    // terminal tool. Each round reports its own usage, so a report that showed
    // the last round's numbers would show 120/34 instead of twice that.
    struct TwoRounds {
        inner: ForcedToolProvider,
        rounds: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl Provider for TwoRounds {
        fn descriptor(&self) -> ProviderDescriptor {
            self.inner.descriptor()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            self.inner.list_models().await
        }

        async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
            let round = {
                let mut rounds = self.rounds.lock().expect("not poisoned");
                *rounds += 1;
                *rounds
            };

            if round == 1 {
                return Ok(provider_event_stream_from_response(Response {
                    id: "round-1".to_string(),
                    model: self.inner.model.id.clone(),
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "call-0".to_string(),
                        name: "files".to_string(),
                        input: json!({ "operations": [{ "op": "list", "path": "." }] }),
                    }],
                    stop_reason: Some("tool_use".to_string()),
                    usage: self.inner.usage.clone(),
                }));
            }

            self.inner.stream(request).await
        }
    }

    let inner = ForcedToolProvider::answering(json!({ "verdict": "ship", "findings": [] }))
        .reporting_usage(120, 34);
    let model = inner.model.clone();
    let runtime = Runtime::builder()
        .with_provider_instance(TwoRounds {
            inner,
            rounds: Arc::clone(&calls),
        })
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(dir.path()))
        .build()
        .expect("runtime builds");

    let mut run = prepare_with_session(
        session(&runtime, dir.path(), model),
        dir.path(),
        "review this diff",
        &context(),
        "anthropic",
        "typed-model",
    )
    .expect("prepared");

    let output = run
        .output::<Review, _, _>(
            "review this diff",
            review_spec(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect("the run produces a value");

    assert_eq!(*calls.lock().expect("not poisoned"), 2, "two rounds ran");
    assert_eq!(output.report.usage.input_tokens, 240);
    assert_eq!(output.report.usage.output_tokens, 68);
    assert_eq!(output.report.usage.cache_read_tokens, 2);
    assert_eq!(output.report.usage.cache_creation_tokens, 4);
    assert_eq!(output.report.usage.model_responses, 2);
}

#[tokio::test]
async fn a_plain_turn_reports_what_it_spent_too() {
    let dir = workspace();
    let (_runtime, mut run) = prepared(
        &dir,
        ForcedToolProvider::ignoring_the_forced_tool().reporting_usage(90, 10),
    );

    // No forced tool, so this provider answers in prose — an ordinary turn.
    let report = run
        .execute(CollectingSink::new())
        .await
        .expect("run completes");

    assert!(report.succeeded());
    assert_eq!(report.usage.total_tokens(), 100);

    // And the same figure closes the stream, because a consumer that only
    // ever sees JSONL — `basis spawn --json`, `basis watch` — has no report to
    // read it off. Asserted on the serialized line rather than the enum: the
    // wire shape is the contract, and `usage` is the field a host prices a
    // run from.
    let finish = serde_json::to_value(
        report
            .sink
            .into_events()
            .pop()
            .expect("the stream closes with a finish line"),
    )
    .expect("serializes");
    assert_eq!(finish["type"], "run_finished");
    assert_eq!(finish["usage"]["input_tokens"], 90);
    assert_eq!(finish["usage"]["output_tokens"], 10);
}

#[tokio::test]
async fn a_working_typed_turn_reads_a_file_and_answers_in_the_same_call() {
    // What `with_tools` is for, end to end: the ask that used to need two
    // turns — read, then shape — done in one, with the reading proved by the
    // file that actually opened rather than by the roster alone.
    let dir = workspace();
    let provider = ScriptedModel::new(vec![
        Say::Read,
        Say::Answer(json!({ "verdict": "hold", "findings": ["the house rules are unenforced"] })),
    ]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let (_runtime, mut run) = prepared_with(&dir, provider, model);

    let output = run
        .output::<Review, _, _>(
            "read AGENTS.md, then review this diff",
            review_spec().with_tools(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect("a working turn answers");

    assert_eq!(
        output.value,
        Review {
            verdict: "hold".to_string(),
            findings: vec!["the house rules are unenforced".to_string()],
        }
    );

    let offers = handle.offers();
    assert_eq!(offers.len(), 2, "the turn worked a round, then answered");
    for (round, offer) in offers.iter().enumerate() {
        assert!(
            offer.terminal.is_some(),
            "round {round} can still end the turn: {:?}",
            offer.tools
        );
        assert!(
            offer.ordinary().iter().any(|name| *name == "files"),
            "round {round} keeps the ordinary toolset: {:?}",
            offer.tools
        );
        assert!(
            !matches!(offer.choice, Some(ToolChoice::Tool { .. })),
            "round {round} forces nothing — a forced choice would preclude \
             either the working rounds or the call that ends them, got {:?}",
            offer.choice
        );
    }

    // The roster is not the point; the reading is. A turn that was offered the
    // file tool and never opened anything would pass every assertion above.
    let events = output.report.sink.into_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ToolCompleted { tool_name, is_error: false, .. } if tool_name == "files"
        )),
        "the file was read on the turn that answered: {events:#?}"
    );
}

#[tokio::test]
async fn a_shaping_turn_is_still_handed_one_tool_and_told_to_call_it() {
    // The control for the test above, and the promise `with_tools` had to keep
    // to be addable at all: a spec that does not ask for tools does not get
    // them, and is still made to answer on the first round.
    let dir = workspace();
    let provider = ScriptedModel::new(vec![Say::Answer(
        json!({ "verdict": "ship", "findings": [] }),
    )]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let (_runtime, mut run) = prepared_with(&dir, provider, model);

    run.output::<Review, _, _>(
        "review this diff",
        review_spec(),
        CollectingSink::new(),
        AllowAll,
    )
    .await
    .expect("a shaping turn answers");

    let offers = handle.offers();
    assert_eq!(offers.len(), 1, "one round decides a shape");
    assert!(
        offers[0].terminal.is_some() && offers[0].ordinary().is_empty(),
        "the terminal tool is the only tool: {:?}",
        offers[0].tools
    );
    assert!(
        matches!(offers[0].choice, Some(ToolChoice::Tool { .. })),
        "and the model is told to call it, got {:?}",
        offers[0].choice
    );
}

#[tokio::test]
async fn a_working_turn_that_settles_for_prose_produces_no_value() {
    // The price of the mode: nothing forces the ending, so the model can work
    // and then simply talk. A workflow must hear that as the failure it is
    // rather than receive a value nobody committed.
    let dir = workspace();
    let provider = ScriptedModel::new(vec![Say::Read, Say::Prose]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let (_runtime, mut run) = prepared_with(&dir, provider, model);

    let error = run
        .output::<Review, _, _>(
            "read AGENTS.md, then review this diff",
            review_spec().with_tools(),
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect_err("prose is not an answer to a typed ask");

    assert!(
        matches!(error.error, RunError::Runtime(_)),
        "expected a runtime failure, got {:?}",
        error.error
    );
    // And it is this mode's failure, not the old one: the turn had the whole
    // toolset in front of it for both rounds and still ended on talk.
    let offers = handle.offers();
    assert_eq!(offers.len(), 2);
    assert!(
        offers
            .iter()
            .all(|offer| offer.ordinary().iter().any(|name| *name == "files")),
        "a working turn ran: {offers:?}"
    );
}

#[tokio::test]
async fn a_working_turn_out_of_budget_hands_back_the_bound_it_stopped_on() {
    // A working turn can be refused another round while it is still gathering,
    // which is the one way it fails that reads exactly like a broken provider.
    // Telling the two apart is the report's job — `stopped_by` names the
    // allowance — so the report has to survive the failure and not only the
    // stream. It used to be the other way round, and a caller that wanted the
    // bound had to sift its own event log for it.
    let dir = workspace();
    let provider = ScriptedModel::new(vec![
        Say::Read,
        Say::Answer(json!({ "verdict": "ship", "findings": [] })),
    ])
    .spending(60, 40);
    let handle = provider.clone();
    let model = provider.model.clone();
    let (_runtime, mut run) = prepared_with(&dir, provider, model);

    // An ordinary `CollectingSink`: the sink now comes back inside the report
    // the failure carries, so a test no longer needs a closure over shared
    // state to read the stream of a turn that produced nothing.
    let failure = run
        .output_with_options::<Review, _, _>(
            "read AGENTS.md, then review this diff",
            review_spec().with_tools(),
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default().with_token_budget(100),
        )
        .await
        .expect_err("a turn stopped before the terminal call has no value");

    assert!(
        matches!(failure.error, RunError::Runtime(_)),
        "expected a runtime failure, got {:?}",
        failure.error
    );

    let report = failure.report.expect("a turn that ran has a report");
    assert_eq!(
        report.stopped_by,
        Some(Bound::TokenBudget),
        "the allowance, not the provider, is what ended it"
    );
    assert_eq!(
        report.usage.total_tokens(),
        100,
        "and what it spent getting there"
    );

    let offers = handle.offers();
    assert_eq!(
        offers.len(),
        1,
        "the budget ended the turn before the answering round"
    );
    assert!(
        offers[0].ordinary().iter().any(|name| *name == "files"),
        "and it was a working turn that got cut off: {offers:?}"
    );

    // The stream still says the same thing, because a client reading JSONL has
    // no report to read it off — the two accounts must not disagree.
    assert!(
        matches!(
            report.sink.into_events().last(),
            Some(Event::RunFinished {
                stopped_by: Some(Bound::TokenBudget),
                ..
            })
        ),
        "the stream and the report tell one story"
    );
}
