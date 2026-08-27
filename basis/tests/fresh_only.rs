//! Gate 1a's explicit one-independent-mint lifecycle.

use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

use basis::{
    Config, ContextConfig, ModelInfo, Provider, RunError, RunSpec, Runtime, Workspace, async_trait,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
};

const PROVIDER: &str = "fresh-only-provider";
const MODEL: &str = "fresh-only-model";

#[derive(Clone, Default)]
struct Activity {
    listings: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
}

struct TextProvider(Activity);

#[async_trait]
impl Provider for TextProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(PROVIDER)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.0.listings.fetch_add(1, Ordering::SeqCst);
        Ok(vec![ModelInfo::new(MODEL, PROVIDER)])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.0.streams.fetch_add(1, Ordering::SeqCst);
        Ok(provider_event_stream_from_response(Response {
            id: "fresh-only-response".to_string(),
            model: request.model.to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text("done")],
            stop_reason: None,
            usage: None,
        }))
    }
}

fn runtime(activity: Activity) -> basis::RuntimeBuilder {
    Runtime::builder()
        .with_provider_instance(TextProvider(activity))
        .with_ephemeral_history()
}

async fn workspace(path: &std::path::Path, activity: Activity, fresh_only: bool) -> Workspace {
    let builder = Workspace::builder(path)
        .without_discovery()
        .with_runtime_builder(runtime(activity))
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER));
    let builder = if fresh_only {
        builder.fresh_only()
    } else {
        builder
    };

    builder.open().await.expect("workspace opens")
}

fn persisted_agents(workspace: &Workspace) -> usize {
    let identifier = basis::store::runtime_identifier(workspace.path());
    workspace
        .mentra_runtime()
        .list_persisted_agents(&identifier)
        .expect("list volatile agents")
        .len()
}

#[tokio::test]
async fn default_workspace_still_allows_multiple_independent_mints() {
    let fixture = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();
    let workspace = workspace(fixture.path(), activity.clone(), false).await;

    let first = workspace.prepare("first").expect("first mint");
    let second = workspace.prepare("second").expect("second mint");

    assert_ne!(first.agent_id(), second.agent_id());
    assert_eq!(persisted_agents(&workspace), 2);
    assert_eq!(activity.listings.load(Ordering::SeqCst), 0);
    assert_eq!(activity.streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fresh_only_refuses_shared_runtime_in_either_builder_order() {
    let fixture = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();

    for fresh_last in [false, true] {
        let runtime = Arc::new(runtime(activity.clone()).build().expect("shared runtime"));
        let builder = Workspace::builder(fixture.path())
            .with_context(ContextConfig::none())
            .with_config(Config::default());
        let builder = if fresh_last {
            builder.with_runtime(runtime).fresh_only()
        } else {
            builder.fresh_only().with_runtime(runtime)
        };
        let error = builder
            .open()
            .await
            .expect_err("fresh-only must own its runtime privately");
        assert!(matches!(error, RunError::FreshOnlySharedRuntime));
    }

    let runtime = Arc::new(runtime(activity.clone()).build().expect("shared runtime"));
    let precedence = Workspace::builder(fixture.path())
        .with_runtime(runtime)
        .fresh_only()
        .without_discovery()
        .open()
        .await
        .expect_err("discovery-off is the more specific shared-runtime refusal");
    assert!(matches!(
        precedence,
        RunError::DiscoveryDisabledSharedRuntime
    ));

    assert_eq!(activity.listings.load(Ordering::SeqCst), 0);
    assert_eq!(activity.streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn concurrent_prepare_attempts_create_exactly_one_agent() {
    let fixture = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();
    let workspace = Arc::new(workspace(fixture.path(), activity.clone(), true).await);
    let barrier = Arc::new(Barrier::new(3));

    let outcomes = std::thread::scope(|scope| {
        let first_workspace = Arc::clone(&workspace);
        let first_barrier = Arc::clone(&barrier);
        let first = scope.spawn(move || {
            first_barrier.wait();
            first_workspace.prepare("first")
        });
        let second_workspace = Arc::clone(&workspace);
        let second_barrier = Arc::clone(&barrier);
        let second = scope.spawn(move || {
            second_barrier.wait();
            second_workspace.prepare("second")
        });
        barrier.wait();
        [
            first.join().expect("first thread"),
            second.join().expect("second thread"),
        ]
    });

    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(RunError::FreshOnlyRunAlreadyAttempted)))
            .count(),
        1
    );
    assert_eq!(persisted_agents(&workspace), 1);
    assert_eq!(activity.listings.load(Ordering::SeqCst), 0);
    assert_eq!(activity.streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_first_prepare_consumes_the_one_attempt_before_mint() {
    let fixture = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();
    let workspace = workspace(fixture.path(), activity.clone(), true).await;

    let first = workspace
        .prepare(RunSpec::new("invalid").with_resolved_model(ModelInfo::new("foreign", "other")))
        .expect_err("first profile validation fails");
    assert!(matches!(
        first,
        RunError::ResolvedModelProviderMismatch { .. }
    ));
    let second = workspace
        .prepare("retry")
        .expect_err("fresh-only never rolls a claim back");
    assert!(matches!(second, RunError::FreshOnlyRunAlreadyAttempted));
    assert_eq!(persisted_agents(&workspace), 0);
    assert_eq!(activity.streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_resume_consumes_the_same_slot_as_prepare() {
    let fixture = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();
    let workspace = workspace(fixture.path(), activity.clone(), true).await;

    workspace
        .resume("missing-agent", "resume")
        .expect_err("missing resume fails");
    let second = workspace
        .prepare("new conversation")
        .expect_err("resume and prepare share one independent slot");
    assert!(matches!(second, RunError::FreshOnlyRunAlreadyAttempted));
    assert_eq!(persisted_agents(&workspace), 0);
    assert_eq!(activity.streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn successful_prepare_blocks_independent_resume_before_lookup() {
    let fixture = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();
    let workspace = workspace(fixture.path(), activity.clone(), true).await;
    let run = workspace.prepare("first").expect("first mint");

    let error = workspace
        .resume(run.agent_id(), "independent resume")
        .expect_err("a second independent handle is refused");
    assert!(matches!(error, RunError::FreshOnlyRunAlreadyAttempted));
    assert_eq!(persisted_agents(&workspace), 1);
    assert_eq!(activity.streams.load(Ordering::SeqCst), 0);
}
