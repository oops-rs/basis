//! Public consume/rebuild lifecycle for one strictly host-defined workspace.

use std::{
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use basis::tools::{
    ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
};
use basis::{
    BranchError, Event, EventSink, ModelInfo, RunError, Runtime, ToolRoster, Workspace,
    provider_core,
};

const PROVIDER: &str = "reusable";
const MODEL: &str = "test-model";

#[derive(Clone, Default)]
struct Activity {
    makes: Arc<AtomicUsize>,
    warms: Arc<AtomicUsize>,
}

fn provider() -> provider_core::responses::ResponsesProvider<provider_core::StaticCredentialSource>
{
    let mut definition = provider_core::responses::openai_definition();
    definition.descriptor.id = provider_core::ProviderId::new(PROVIDER);
    definition.base_url = Some("http://127.0.0.1:1/".to_string());
    provider_core::responses::ResponsesProvider::new(
        definition,
        provider_core::StaticCredentialSource::new("test-key"),
    )
}

fn recipe(activity: &Activity) -> basis::RuntimeRecipe {
    let makes = Arc::clone(&activity.makes);
    let warms = Arc::clone(&activity.warms);
    Runtime::builder()
        .with_reusable_registered_provider(
            PROVIDER,
            move || {
                makes.fetch_add(1, Ordering::SeqCst);
                Ok::<_, io::Error>(provider().fresh_session_scope())
            },
            move |_provider| {
                warms.fetch_add(1, Ordering::SeqCst);
                async { Ok::<_, io::Error>(()) }
            },
        )
        .with_ephemeral_history()
        .into_reusable_recipe()
        .expect("repeatable provider and ephemeral history form a recipe")
}

fn recipe_failing_second_warm(activity: &Activity) -> basis::RuntimeRecipe {
    let makes = Arc::clone(&activity.makes);
    let warms = Arc::clone(&activity.warms);
    Runtime::builder()
        .with_reusable_registered_provider(
            PROVIDER,
            move || {
                makes.fetch_add(1, Ordering::SeqCst);
                Ok::<_, io::Error>(provider().fresh_session_scope())
            },
            move |_provider| {
                let generation = warms.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if generation == 2 {
                        Err(io::Error::other("replacement warm refused"))
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .with_ephemeral_history()
        .into_reusable_recipe()
        .expect("repeatable provider and ephemeral history form a recipe")
}

fn reusable_builder(path: &std::path::Path, activity: &Activity) -> basis::WorkspaceBuilder {
    Workspace::builder(path)
        .with_runtime_recipe(recipe(activity))
        .without_discovery()
        .fresh_only()
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER))
        .with_tool_roster(ToolRoster::only(std::iter::empty::<String>()))
}

async fn open_bound(path: &std::path::Path, activity: &Activity) -> Workspace {
    reusable_builder(path, activity)
        .open()
        .await
        .expect("reusable workspace opens")
        .bind_host_tools(Vec::new())
        .expect("tool-free checkout binds")
}

#[tokio::test]
async fn reusable_open_requires_every_fail_closed_posture() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(root.path().join(".basis")).expect("config directory");
    std::fs::write(root.path().join("AGENTS.md"), [0xff]).expect("hostile context");
    std::fs::write(root.path().join(".basis/config.json"), "not json").expect("hostile config");

    let error = Workspace::builder(root.path())
        .with_runtime_recipe(recipe(&Activity::default()))
        .fresh_only()
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER))
        .open()
        .await
        .expect_err("posture is refused before hostile discovery is read");
    assert!(matches!(
        error,
        RunError::ReusableWorkspaceRequiresDiscoveryOff
    ));

    let error = Workspace::builder(root.path())
        .with_runtime_recipe(recipe(&Activity::default()))
        .without_discovery()
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER))
        .open()
        .await
        .expect_err("fresh-only must be explicit");
    assert!(matches!(
        error,
        RunError::ReusableWorkspaceRequiresFreshOnly
    ));

    let error = Workspace::builder(root.path())
        .with_runtime_recipe(recipe(&Activity::default()))
        .without_discovery()
        .fresh_only()
        .open()
        .await
        .expect_err("resolved metadata is required");
    assert!(matches!(
        error,
        RunError::ReusableWorkspaceRequiresResolvedModel
    ));

    let error = Workspace::builder(root.path())
        .with_runtime_recipe(recipe(&Activity::default()))
        .without_discovery()
        .fresh_only()
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER))
        .open()
        .await
        .expect_err("a default deny-list can widen with registrations");
    assert!(matches!(
        error,
        RunError::ReusableWorkspaceRequiresExactRoster
    ));

    let activity = Activity::default();
    let error = Workspace::builder(root.path())
        .with_runtime_recipe(recipe(&activity))
        .without_discovery()
        .fresh_only()
        .with_resolved_model(ModelInfo::new(MODEL, "different-provider"))
        .with_tool_roster(ToolRoster::only(std::iter::empty::<String>()))
        .open()
        .await
        .expect_err("provider identity is refused before building or warming");
    assert!(matches!(
        error,
        RunError::ResolvedModelProviderMismatch { .. }
    ));
    assert_eq!(activity.makes.load(Ordering::SeqCst), 0);
    assert_eq!(activity.warms.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn bind_mint_rebuild_and_remint_are_one_fresh_generation_each() {
    let root = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();
    let workspace = reusable_builder(root.path(), &activity)
        .open()
        .await
        .expect("first generation opens");

    let error = workspace
        .prepare("must bind first")
        .expect_err("unbound means no mint");
    assert!(matches!(error, RunError::ReusableWorkspaceToolsUnbound));

    let workspace = workspace
        .bind_host_tools(Vec::new())
        .expect("an explicitly empty set binds a tool-free checkout");
    let first = workspace.prepare("first").expect("first mint");
    drop(first);

    let workspace = workspace
        .rebuild_for_reuse()
        .await
        .expect("consuming rebuild succeeds after the run drops");
    let error = workspace
        .prepare("must bind the replacement")
        .expect_err("a replacement always returns unbound");
    assert!(matches!(error, RunError::ReusableWorkspaceToolsUnbound));

    let workspace = workspace
        .bind_host_tools(Vec::new())
        .expect("bind the replacement explicitly");
    let second = workspace.prepare("second").expect("mint posture reset");
    drop(second);

    assert_eq!(activity.makes.load(Ordering::SeqCst), 2);
    assert_eq!(activity.warms.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn live_runs_and_observers_each_block_and_consume_rebuild() {
    let root = tempfile::tempdir().expect("workspace");
    let workspace = open_bound(root.path(), &Activity::default()).await;
    let run = workspace.prepare("live").expect("mint");
    let error = workspace
        .rebuild_for_reuse()
        .await
        .expect_err("a live run owns the generation");
    assert!(matches!(
        error,
        RunError::ReusableWorkspaceOutstanding { leases: 1 }
    ));
    drop(run);

    let workspace = open_bound(root.path(), &Activity::default()).await;
    let run = workspace.prepare("observed").expect("mint");
    let guard = run.register_agent_event_tap(|_| {});
    drop(run);
    let error = workspace
        .rebuild_for_reuse()
        .await
        .expect_err("the observer retains its own lease");
    assert!(matches!(
        error,
        RunError::ReusableWorkspaceOutstanding { leases: 1 }
    ));
    drop(guard);
}

struct BlockingSink {
    emissions: usize,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl EventSink for BlockingSink {
    fn emit(&mut self, _event: Event) -> io::Result<()> {
        self.emissions += 1;
        if self.emissions == 1 {
            return Ok(());
        }

        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        let (released, wake) = &*self.release;
        let released = released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            wake.wait_while(released, |released| !*released)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detached_blocked_forwarder_keeps_its_lease_until_actual_exit() {
    let root = tempfile::tempdir().expect("workspace");
    let workspace = open_bound(root.path(), &Activity::default()).await;
    let mut run = workspace.prepare("start a turn").expect("mint");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let sink = BlockingSink {
        emissions: 0,
        entered: Some(entered_tx),
        release: Arc::clone(&release),
    };
    let driver = tokio::spawn(async move { run.execute(sink).await });

    tokio::time::timeout(Duration::from_secs(5), entered_rx)
        .await
        .expect("forwarder emitted a session event")
        .expect("forwarder reached the blocking sink");
    driver.abort();
    let _ = driver.await;

    let error = workspace
        .rebuild_for_reuse()
        .await
        .expect_err("dropping the JoinHandle detached, not finished, the forwarder");
    assert!(matches!(
        error,
        RunError::ReusableWorkspaceOutstanding { leases: 1 }
    ));

    let (released, wake) = &*release;
    *released
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    wake.notify_all();
}

#[tokio::test]
async fn every_public_raw_escape_permanently_refuses_reuse() {
    let root = tempfile::tempdir().expect("workspace");

    let workspace = open_bound(root.path(), &Activity::default()).await;
    let _ = workspace.mentra_runtime().tools();
    assert!(matches!(
        workspace.rebuild_for_reuse().await,
        Err(RunError::ReusableWorkspaceRawAccess)
    ));

    let workspace = open_bound(root.path(), &Activity::default()).await;
    let run = workspace.prepare("session ref").expect("mint");
    let _ = run.session().id();
    drop(run);
    assert!(matches!(
        workspace.rebuild_for_reuse().await,
        Err(RunError::ReusableWorkspaceRawAccess)
    ));

    let workspace = open_bound(root.path(), &Activity::default()).await;
    let mut run = workspace.prepare("session mut").expect("mint");
    let _ = run.session_mut().id();
    drop(run);
    assert!(matches!(
        workspace.rebuild_for_reuse().await,
        Err(RunError::ReusableWorkspaceRawAccess)
    ));

    let workspace = open_bound(root.path(), &Activity::default()).await;
    let run = workspace.prepare("session owned").expect("mint");
    let session = run.into_session();
    drop(session);
    assert!(matches!(
        workspace.rebuild_for_reuse().await,
        Err(RunError::ReusableWorkspaceRawAccess)
    ));
}

#[tokio::test]
async fn basis_transcript_helpers_do_not_poison_reuse() {
    let root = tempfile::tempdir().expect("workspace");
    let workspace = open_bound(root.path(), &Activity::default()).await;
    let mut run = workspace.prepare("inspect internally").expect("mint");
    assert!(run.transcript().is_empty());
    assert!(run.history().is_empty());
    assert!(run.children("missing").is_empty());
    assert!(matches!(
        run.branch_from("missing"),
        Err(BranchError::UnknownEntry(id)) if id == "missing"
    ));
    drop(run);

    let _replacement = workspace
        .rebuild_for_reuse()
        .await
        .expect("Basis-owned read helpers do not expose raw state");
}

struct ProbeTool {
    name: &'static str,
    context: Arc<()>,
}

impl ToolDefinition for ProbeTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(self.name)
            .description("lifecycle probe")
            .input_schema(serde_json::json!({"type": "object", "properties": {}}))
            .build()
    }
}

#[basis::async_trait]
impl ToolExecutor for ProbeTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: serde_json::Value) -> ToolResult {
        let _keep_alive = &self.context;
        Ok(self.name.to_string())
    }
}

fn probe_tool(name: &'static str) -> (Box<dyn basis::tools::ExecutableTool>, std::sync::Weak<()>) {
    let context = Arc::new(());
    let weak = Arc::downgrade(&context);
    (Box::new(ProbeTool { name, context }), weak)
}

#[tokio::test]
async fn rebuilding_drops_old_tool_context_store_and_registry() {
    let root = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();
    let workspace = reusable_builder(root.path(), &activity)
        .open()
        .await
        .expect("opens");
    let (old_tool, old_context) = probe_tool("old_only");
    let workspace = workspace
        .bind_host_tools(vec![old_tool])
        .expect("old tool binds");
    let run = workspace.prepare("first").expect("old store gets an agent");
    drop(run);
    assert!(old_context.upgrade().is_some());

    let workspace = workspace
        .rebuild_for_reuse()
        .await
        .expect("replacement builds");
    assert!(old_context.upgrade().is_none(), "old registry dropped");

    let (new_tool, new_context) = probe_tool("new_only");
    let workspace = workspace
        .bind_host_tools(vec![new_tool])
        .expect("new tool binds");
    assert!(new_context.upgrade().is_some());

    let runtime = workspace.mentra_runtime();
    let names = runtime
        .tools()
        .into_iter()
        .map(|descriptor| descriptor.provider.name)
        .collect::<Vec<_>>();
    assert!(!names.iter().any(|name| name == "old_only"));
    assert!(names.iter().any(|name| name == "new_only"));
    let identifier = basis::store::runtime_identifier(workspace.path());
    assert!(
        runtime
            .list_persisted_agents(&identifier)
            .expect("list replacement store")
            .is_empty(),
        "replacement volatile store starts empty"
    );
}

#[tokio::test]
async fn binding_failure_consumes_and_drops_the_generation() {
    let root = tempfile::tempdir().expect("workspace");
    let workspace = reusable_builder(root.path(), &Activity::default())
        .open()
        .await
        .expect("opens");
    let (tool, context) = probe_tool("spawn");
    let error = workspace
        .bind_host_tools(vec![tool])
        .expect_err("complete preflight sees the existing name");
    assert!(matches!(error, RunError::HostTool(_)));
    assert!(context.upgrade().is_none(), "failed entry was dropped");
}

#[tokio::test]
async fn invalid_and_duplicate_tool_sets_are_refused_before_registration() {
    let root = tempfile::tempdir().expect("workspace");
    let workspace = reusable_builder(root.path(), &Activity::default())
        .open()
        .await
        .expect("opens");
    let (invalid, invalid_context) = probe_tool("not valid");
    let error = workspace
        .bind_host_tools(vec![invalid])
        .expect_err("invalid names are refused before registration");
    assert!(matches!(error, RunError::ReusableHostToolName { .. }));
    assert!(invalid_context.upgrade().is_none());

    let workspace = reusable_builder(root.path(), &Activity::default())
        .open()
        .await
        .expect("opens again");
    let (first, first_context) = probe_tool("duplicate");
    let (second, second_context) = probe_tool("duplicate");
    let error = workspace
        .bind_host_tools(vec![first, second])
        .expect_err("duplicates are refused before the first registration");
    assert!(matches!(error, RunError::HostTool(_)));
    assert!(first_context.upgrade().is_none());
    assert!(second_context.upgrade().is_none());
}

#[tokio::test]
async fn unbound_rebuild_second_bind_and_drop_never_create_a_replacement() {
    let root = tempfile::tempdir().expect("workspace");

    let activity = Activity::default();
    let workspace = reusable_builder(root.path(), &activity)
        .open()
        .await
        .expect("opens");
    let error = workspace
        .rebuild_for_reuse()
        .await
        .expect_err("an unbound generation cannot be parked");
    assert!(matches!(error, RunError::ReusableWorkspaceToolsUnbound));
    assert_eq!(activity.makes.load(Ordering::SeqCst), 1);
    assert_eq!(activity.warms.load(Ordering::SeqCst), 1);

    let activity = Activity::default();
    let workspace = open_bound(root.path(), &activity).await;
    let error = workspace
        .bind_host_tools(Vec::new())
        .expect_err("a generation binds exactly once");
    assert!(matches!(error, RunError::ReusableWorkspaceAlreadyBound));
    assert_eq!(activity.makes.load(Ordering::SeqCst), 1);
    assert_eq!(activity.warms.load(Ordering::SeqCst), 1);

    let activity = Activity::default();
    let workspace = reusable_builder(root.path(), &activity)
        .open()
        .await
        .expect("opens for drop");
    drop(workspace);
    tokio::task::yield_now().await;
    assert_eq!(activity.makes.load(Ordering::SeqCst), 1);
    assert_eq!(activity.warms.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn replacement_warm_failure_returns_no_reusable_entry() {
    let root = tempfile::tempdir().expect("workspace");
    let activity = Activity::default();
    let workspace = Workspace::builder(root.path())
        .with_runtime_recipe(recipe_failing_second_warm(&activity))
        .without_discovery()
        .fresh_only()
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER))
        .with_tool_roster(ToolRoster::only(std::iter::empty::<String>()))
        .open()
        .await
        .expect("first generation warms")
        .bind_host_tools(Vec::new())
        .expect("bind first generation");

    let error = workspace
        .rebuild_for_reuse()
        .await
        .expect_err("failed replacement warm returns no workspace");
    assert!(matches!(error, RunError::RuntimeRecipeProviderWarm(_)));
    assert_eq!(activity.makes.load(Ordering::SeqCst), 2);
    assert_eq!(activity.warms.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn ordinary_workspaces_cannot_enter_the_reuse_lifecycle() {
    let root = tempfile::tempdir().expect("workspace");
    let workspace = Workspace::builder(root.path())
        .with_runtime_builder(
            Runtime::builder()
                .with_registered_provider(provider())
                .with_ephemeral_history(),
        )
        .without_discovery()
        .with_resolved_model(ModelInfo::new(MODEL, PROVIDER))
        .open()
        .await
        .expect("ordinary private workspace opens");
    assert!(matches!(
        workspace.bind_host_tools(Vec::new()),
        Err(RunError::WorkspaceNotReusable)
    ));
}
