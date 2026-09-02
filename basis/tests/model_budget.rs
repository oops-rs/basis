//! Model-request budgets and retry overrides through Basis's public turn API.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use basis::{
    AllowAll, CollectingSink, Event, ModelInfo, Provider, RunFailure, RunFailureCategory,
    RunOutcome, Runtime, TurnOptions, Workspace, async_trait,
    runtime::{
        ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        ProviderRetry, Request,
    },
};

const PROVIDER: &str = "model-budget-provider";
const MODEL: &str = "model-budget-model";

struct AlwaysRetryable {
    requests: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for AlwaysRetryable {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(PROVIDER)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Err(ProviderError::InvalidRequest(
            "resolved model must bypass listing".to_string(),
        ))
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::Retryable {
            message: "retry the scripted request".to_string(),
            delay: None,
        })
    }
}

#[tokio::test]
async fn model_budget_counts_retry_attempts_and_preserves_the_wire_outcome() {
    let requests = Arc::new(AtomicUsize::new(0));
    let fixture = tempfile::tempdir().expect("workspace");
    let workspace = Workspace::builder(fixture.path())
        .without_discovery()
        .with_runtime_builder(
            Runtime::builder()
                .with_provider_instance(AlwaysRetryable {
                    requests: Arc::clone(&requests),
                })
                .with_ephemeral_history(),
        )
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER))
        .open()
        .await
        .expect("model-budget workspace opens");
    let mut run = workspace
        .prepare("spend exactly two requests")
        .expect("mints");
    let options = TurnOptions::default()
        .with_model_budget(2)
        .with_retry_budget(2)
        .with_provider_retry(ProviderRetry {
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            retry_after_cap: Duration::ZERO,
        });

    let report = run
        .execute_with_approver_and_options(CollectingSink::default(), AllowAll, options)
        .await
        .expect("a terminal turn failure is a completed Basis report");
    let expected = RunOutcome::Error {
        message: "model budget exceeded at 2 request(s)".to_string(),
    };

    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert!(matches!(
        report.failure.as_ref(),
        Some(RunFailure::ModelBudgetExceeded(2))
    ));
    assert_eq!(
        report.failure.as_ref().map(RunFailure::category),
        Some(RunFailureCategory::Terminal)
    );
    assert_eq!(report.outcome, expected);
    assert_eq!(report.final_message, None);
    assert_eq!(report.stopped_by, None);
    assert!(matches!(
        report.sink.events().last(),
        Some(Event::RunFinished {
            outcome,
            stopped_by: None,
            ..
        }) if outcome == &expected
    ));
}

#[tokio::test]
async fn retryable_provider_category_and_source_survive_without_string_parsing() {
    let requests = Arc::new(AtomicUsize::new(0));
    let fixture = tempfile::tempdir().expect("workspace");
    let workspace = Workspace::builder(fixture.path())
        .without_discovery()
        .with_runtime_builder(
            Runtime::builder()
                .with_provider_instance(AlwaysRetryable {
                    requests: Arc::clone(&requests),
                })
                .with_ephemeral_history(),
        )
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER))
        .open()
        .await
        .expect("retry-category workspace opens");
    let mut run = workspace.prepare("fail once").expect("mints");

    let report = run
        .execute_with_approver_and_options(
            CollectingSink::default(),
            AllowAll,
            TurnOptions::default().with_retry_budget(0),
        )
        .await
        .expect("provider failure is a completed Basis report");
    let failure = report.failure.as_ref().expect("typed provider failure");

    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(matches!(
        failure,
        RunFailure::FailedToStreamResponse(ProviderError::Retryable {
            message,
            delay: None,
        }) if message == "retry the scripted request"
    ));
    assert_eq!(failure.category(), RunFailureCategory::Retryable);
    assert_eq!(
        report.outcome,
        RunOutcome::Error {
            message: failure.to_string(),
        }
    );
}
