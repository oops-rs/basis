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

use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use lan_core::{
    AllowAll, CollectingSink, Event, OutputSpec, RunConfig, RunError, RunOutcome,
    run::prepare_with_session,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session, TokenUsage,
    ToolChoice,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::SqliteRuntimeStore,
};
use serde::Deserialize;
use serde_json::{Value, json};

mod common;

/// The shape the caller asks for.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Review {
    verdict: String,
    findings: Vec<String>,
}

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

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules").expect("write AGENTS.md");
    dir
}

fn config(workspace: &Path) -> RunConfig {
    RunConfig::new(workspace, "review this diff").with_context(lan_core::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
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
) -> (Runtime, lan_core::PreparedRun) {
    let model = provider.model.clone();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(SqliteRuntimeStore::new(
            common::scratch_store().join("runtime.sqlite"),
        ))
        .with_policy(RuntimePolicy::workspace_bounded(dir.path()))
        .build()
        .expect("runtime builds");

    let run = prepare_with_session(
        session(&runtime, dir.path(), model),
        &config(dir.path()),
        "anthropic",
        "typed-model",
    )
    .expect("prepared");

    // The runtime is handed back because dropping it would take the session's
    // provider with it.
    (runtime, run)
}

/// What the caller writes by hand. lan derives no schema (see `OutputSpec`), so
/// the descriptions here are the caller's prompt to the model, not a by-product
/// of the type above.
fn review_spec() -> OutputSpec {
    OutputSpec::new(
        "submit_review",
        "call this once you have read every changed file",
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
            outcome: RunOutcome::Ok
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

    let error = run
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
        matches!(error, RunError::OutputMismatch(_)),
        "expected a mismatch, got {error:?}"
    );
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
    // was malformed" as the same `MalformedProviderEvent`, and lan will not
    // read error prose to separate them — so both land here. Narrowing this
    // needs an upstream variant, not a string match (ADR-0005).
    assert!(
        matches!(error, RunError::Runtime(_)),
        "expected a runtime failure, got {error:?}"
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
        .with_store(SqliteRuntimeStore::new(
            common::scratch_store().join("runtime.sqlite"),
        ))
        .with_policy(RuntimePolicy::workspace_bounded(dir.path()))
        .build()
        .expect("runtime builds");

    let mut run = prepare_with_session(
        session(&runtime, dir.path(), model),
        &config(dir.path()),
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
}
