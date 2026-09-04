//! The approval loop, end to end.
//!
//! The property under test is that a consequential call is *answered*. mentra's
//! session authorizer blocks the turn on a oneshot until someone resolves the
//! request, so a harness that emits `permission_requested` without resolving
//! it does not merely lose a feature — it hangs. These tests fail by timing
//! out, which is exactly the failure they exist to catch.
//!
//! There is no policy to configure any more (ADR-0010): the gate surfaces every
//! consequential call and the approver answers all of it. So these drive the
//! approvers basis actually ships, rather than a stand-in for an enum that no
//! longer exists.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use basis::{
    AllowAll, ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, CollectingSink, DenyAll,
    Event, PreparedRun, ToolSideEffectLevel,
    approval::{
        ApprovalGate, RuntimeError, ToolAuthorizationDecision, ToolAuthorizationRequest,
        ToolAuthorizer, is_consequential,
    },
    run::prepare_with_session,
    tools::declared::{DeclaredTool, DeclaredToolSpec, SideEffect},
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session,
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::VolatileRuntimeStore,
    session::{PermissionRuleScope, RememberedRule, RuleKey},
};
use serde_json::json;

/// Every run here must finish well inside this; exceeding it means a request
/// went unanswered and the turn is stuck.
const NOT_STUCK: Duration = Duration::from_secs(10);

/// Replays a fixed script of assistant turns.
struct ScriptedProvider {
    model: ModelInfo,
    turns: Mutex<VecDeque<Vec<ContentBlock>>>,
}

impl ScriptedProvider {
    fn new(model: ModelInfo, turns: Vec<Vec<ContentBlock>>) -> Self {
        Self {
            model,
            turns: Mutex::new(turns.into()),
        }
    }
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

/// A runtime whose first turn reads something and writes a file — one call the
/// gate lets through and one it must put to the approver.
fn runtime_writing_a_file(workspace: &Path) -> (Runtime, ModelInfo) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            vec![
                ContentBlock::ToolUse {
                    id: "call-0".to_string(),
                    name: "check_background".to_string(),
                    input: json!({}),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "files".to_string(),
                    input: json!({
                        "operations": [
                            { "op": "create", "path": "made.txt", "content": "hi" }
                        ]
                    }),
                },
            ],
            vec![ContentBlock::text("done")],
        ],
    );

    let gate = ApprovalGate::new();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        // Nothing here reads a conversation back, so the history has nowhere
        // to be: mentra's in-memory store keeps this suite off the disk
        // entirely rather than leaving a temp database per test behind.
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(gate)
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

fn context() -> basis::ContextConfig {
    basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    }
}

/// Records what it was asked, then lets the approver under test answer.
struct Recording<A> {
    inner: A,
    seen: Arc<Mutex<Vec<ApprovalRequest>>>,
}

#[async_trait]
impl<A: Approver> Approver for Recording<A> {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        self.seen
            .lock()
            .expect("not poisoned")
            .push(request.clone());
        self.inner.approve(request).await
    }
}

/// Runs the scripted turn under `approver`, reporting the stream and every
/// request the approver was put.
async fn run_with<A: Approver>(
    workspace: &Path,
    approver: A,
) -> (Vec<Event>, Vec<ApprovalRequest>) {
    let (runtime, model) = runtime_writing_a_file(workspace);
    let session = session(&runtime, workspace, model);
    let seen = Arc::new(Mutex::new(Vec::new()));

    let mut prepared = prepare_with_session(
        session,
        workspace,
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(
            CollectingSink::new(),
            Recording {
                inner: approver,
                seen: Arc::clone(&seen),
            },
        ),
    )
    .await
    .expect("the run must not hang waiting on an unanswered approval")
    .expect("the run completes");

    let asked = seen.lock().expect("not poisoned").clone();
    (report.sink.into_events(), asked)
}

/// Whether the named tool reported an error, or `None` if it never completed.
fn tool_failed(events: &[Event], tool: &str) -> Option<bool> {
    events.iter().find_map(|event| match event {
        Event::ToolCompleted {
            tool_name,
            is_error,
            ..
        } if tool_name == tool => Some(*is_error),
        _ => None,
    })
}

/// The result text the named tool produced — the same string the model reads
/// back as that call's outcome.
fn tool_result(events: &[Event], tool: &str) -> Option<String> {
    events.iter().find_map(|event| match event {
        Event::ToolCompleted {
            tool_name, summary, ..
        } if tool_name == tool => Some(summary.clone()),
        _ => None,
    })
}

fn asked_about(asked: &[ApprovalRequest]) -> Vec<&str> {
    asked.iter().map(|request| &*request.tool_name).collect()
}

#[tokio::test]
async fn an_approved_call_happens_rather_than_hanging() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, asked) = run_with(workspace.path(), AllowAll).await;

    assert_eq!(
        asked_about(&asked),
        vec!["files"],
        "the write should have been put to the approver"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "the request must also reach the stream"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::PermissionResolved { .. })),
        "and its resolution must too"
    );
    assert_eq!(
        tool_failed(&events, "files"),
        Some(false),
        "an approved call runs"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "an approved write must actually happen"
    );
}

#[tokio::test]
async fn a_refused_call_does_not_happen() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, asked) = run_with(workspace.path(), DenyAll).await;

    assert_eq!(asked_about(&asked), vec!["files"]);
    assert_eq!(
        tool_failed(&events, "files"),
        Some(true),
        "a refused call fails, and the model reads why"
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a refused write must not reach the disk"
    );
}

#[tokio::test]
async fn a_refusal_tells_the_model_what_the_run_does_not_allow() {
    // The whole point of the reason: it is the tool result the model reads,
    // so a read-only run says so once instead of watching the model retry the
    // same write. Pinned verbatim because paraphrase here is a silent
    // regression — the string is the interface.
    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, _asked) = run_with(workspace.path(), DenyAll).await;

    assert_eq!(
        tool_result(&events, "files").as_deref(),
        Some(
            "Tool execution denied: files changes state outside this process, \
             which this run does not allow"
        )
    );
}

#[tokio::test]
async fn a_refusal_with_nothing_to_say_still_refuses() {
    // An approver that gives no reason is still fail-closed; the model just
    // gets mentra's standing wording rather than basis's.
    struct Silent;

    #[async_trait]
    impl Approver for Silent {
        async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
            ApprovalAnswer::new(ApprovalDecision::Deny)
        }
    }

    let workspace = tempfile::tempdir().expect("tempdir");

    let (events, _asked) = run_with(workspace.path(), Silent).await;

    assert_eq!(
        tool_result(&events, "files").as_deref(),
        Some("Tool execution denied: denied by session approver")
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a refused write must not reach the disk, reason or no reason"
    );
}

/// Restores the store root's permissions on drop, so a panicking assertion —
/// or a timeout — cannot leave a read-only tempdir behind that `TempDir::drop`
/// silently fails to remove.
#[cfg(unix)]
struct RestorePermissions<'a>(&'a Path);

#[cfg(unix)]
impl Drop for RestorePermissions<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// Warms a store-backed session, then makes its store root unwritable —
/// shared setup for the two "survives a store outage" tests below. Returns
/// `None` (skip the test) when this process writes through `0o555` anyway
/// (root), since every assertion downstream would test nothing.
///
/// mentra 0.26 made resolving a remembered answer fallible: the rule was
/// persisted to the live store *before* the oneshot was answered, so a store
/// failure could leave an "…for this session" answer downgraded to a plain
/// denial with a notice explaining why. mentra 0.27 removes the failure mode
/// instead of basis having to recover from it: `AllowForSession` and
/// `DenyForSession` now remember into `PermissionRuleScope::Process`
/// (mentra#53), a rung owned by the live session alone and never written to
/// the runtime store — so a "…for this session" answer now survives exactly
/// the outage that used to downgrade it.
#[cfg(unix)]
async fn store_outage_fixture<'a>(
    workspace: &Path,
    store_dir: &'a Path,
    turn: Vec<ContentBlock>,
) -> Option<(PreparedRun, RestorePermissions<'a>)> {
    use std::os::unix::fs::PermissionsExt;

    use mentra::runtime::FileRuntimeStore;

    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            // A plain first turn, so every store file the turn machinery
            // touches — the agent's rows, `runs.jsonl` — exists before the
            // root goes read-only below.
            vec![ContentBlock::text("warmed")],
            turn,
            vec![ContentBlock::text("done")],
        ],
    );
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(FileRuntimeStore::new(store_dir))
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(ApprovalGate::new())
        .build()
        .expect("runtime builds");
    let session = session(&runtime, workspace, model);

    let mut prepared = prepare_with_session(
        session,
        workspace,
        "warm the store",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");
    prepared
        .execute_with_approver(CollectingSink::new(), AllowAll)
        .await
        .expect("the warming turn runs");

    // A durable remembered rule would rewrite `rules.json` atomically — a
    // fresh temp file in the store root — so a read-only root is exactly a
    // store that can still read its rules (there are none) and cannot record
    // a new durable one. Process-scoped remembering never reaches this
    // directory at all, which is the whole point of both tests below.
    std::fs::set_permissions(store_dir, std::fs::Permissions::from_mode(0o555))
        .expect("make the store root read-only");
    let restore = RestorePermissions(store_dir);

    // Mode bits do not stop root. Probe the premise instead of trusting the
    // effective uid: if this process can still create a file in the root,
    // every assertion downstream would test nothing.
    if std::fs::write(store_dir.join(".probe"), b"").is_ok() {
        eprintln!("skipping: this process writes through 0o555 (running as root?)");
        return None;
    }

    Some((prepared, restore))
}

#[cfg(unix)]
#[tokio::test]
async fn a_for_session_denial_survives_a_store_outage() {
    use basis::event::NoticeSeverity;

    struct RefuseWithReason;

    #[async_trait]
    impl Approver for RefuseWithReason {
        async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
            ApprovalAnswer::new(ApprovalDecision::DenyForSession)
                .because("writes are refused at this desk")
        }
    }

    let workspace = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let Some((mut prepared, _restore)) = store_outage_fixture(
        workspace.path(),
        store_dir.path(),
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "files".to_string(),
            input: json!({
                "operations": [
                    { "op": "create", "path": "deny-me.txt", "content": "hi" }
                ]
            }),
        }],
    )
    .await
    else {
        return;
    };

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.send_with_options(
            "make the file",
            CollectingSink::new(),
            RefuseWithReason,
            basis::TurnOptions::default(),
        ),
    )
    .await
    .expect("a store outage must not hang the turn")
    .expect("the run completes");

    let events = report.sink.into_events();
    let results: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCompleted {
                tool_name, summary, ..
            } if tool_name == "files" => Some(summary.clone()),
            _ => None,
        })
        .collect();
    assert!(
        results
            .iter()
            .any(|result| result.ends_with("writes are refused at this desk")),
        "the refusal keeps the person's own reason, not a store error dressed \
         as one: {results:?}"
    );
    assert!(
        !workspace.path().join("deny-me.txt").exists(),
        "a refused write must not reach disk"
    );
    let notices: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            Event::Notice {
                severity: NoticeSeverity::Warning,
                message,
            } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        !notices
            .iter()
            .any(|message| message.contains("could not be")),
        "no store-failure notice belongs on this stream: a process-scoped \
         remember never touched the read-only root: {notices:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_for_session_approval_survives_a_store_outage() {
    use basis::event::NoticeSeverity;

    struct AllowEverything;

    #[async_trait]
    impl Approver for AllowEverything {
        async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
            ApprovalDecision::AllowForSession.into()
        }
    }

    let workspace = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let Some((mut prepared, _restore)) = store_outage_fixture(
        workspace.path(),
        store_dir.path(),
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "files".to_string(),
            input: json!({
                "operations": [
                    { "op": "create", "path": "allow-me.txt", "content": "hi" }
                ]
            }),
        }],
    )
    .await
    else {
        return;
    };

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.send_with_options(
            "make the file",
            CollectingSink::new(),
            AllowEverything,
            basis::TurnOptions::default(),
        ),
    )
    .await
    .expect("a store outage must not hang the turn")
    .expect("the run completes");

    assert!(
        workspace.path().join("allow-me.txt").exists(),
        "the approval must actually run: remembering it for the session never \
         touched the read-only store root"
    );
    let events = report.sink.into_events();
    let notices: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            Event::Notice {
                severity: NoticeSeverity::Warning,
                message,
            } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        !notices
            .iter()
            .any(|message| message.contains("could not be")),
        "no store-failure notice belongs on this stream: nothing was \
         downgraded: {notices:?}"
    );
}

#[tokio::test]
async fn a_read_only_call_is_never_put_to_the_approver() {
    let workspace = tempfile::tempdir().expect("tempdir");

    // Under the strictest approver there is: a read that reached it would be
    // denied, so this catches both halves of the rule at once.
    let (events, asked) = run_with(workspace.path(), DenyAll).await;

    assert!(
        !asked_about(&asked).contains(&"check_background"),
        "prompting for reads trains people to approve without reading: {:?}",
        asked_about(&asked)
    );
    assert_eq!(
        tool_failed(&events, "check_background"),
        Some(false),
        "and a read must still run while everything else is refused"
    );
}

/// Refuses every consequential call outright, the way a host with a posture
/// that must not be answerable by a remembered rule writes one.
struct RefusingGate;

#[async_trait]
impl ToolAuthorizer for RefusingGate {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        if !is_consequential(request.preview.side_effect_level) {
            return Ok(ToolAuthorizationDecision::allow());
        }

        Ok(ToolAuthorizationDecision::deny(format!(
            "{} changes state outside this process, which this session refuses",
            request.tool_name
        )))
    }
}

#[tokio::test]
async fn a_session_authorizers_refusal_outranks_a_rule_seeded_before_it() {
    // What `PreparedRun::with_tool_authorizer` exists for. A durable rule
    // resolves the runtime gate's `Prompt` ahead of the approver, so a posture
    // written on the approver can be pre-empted by an allow someone seeded
    // through the permission handle. An authorizer's own `Deny` is terminal:
    // mentra returns it unchanged, reads no rule, and raises no request.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (runtime, model) = runtime_writing_a_file(workspace.path());
    let session = session(&runtime, workspace.path(), model);

    let prepared = prepare_with_session(
        session,
        workspace.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    // Seeded first and durably, so the rule genuinely predates the posture —
    // the seam `basis/examples/reviewed_shell.rs` teaches, at a scope nothing
    // clears.
    prepared
        .session()
        .permission_handle()
        .remember_rule(RememberedRule {
            key: RuleKey {
                tool_name: "files".to_string(),
                pattern: None,
            },
            allow: true,
            scope: PermissionRuleScope::Global,
            reason: None,
        })
        .expect("the rule is remembered");

    let mut prepared = prepared.with_tool_authorizer(RefusingGate);
    let seen = Arc::new(Mutex::new(Vec::new()));

    // `AllowAll`, so nothing an *approver* did can be mistaken for the
    // refusal: whatever reaches one is allowed.
    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(
            CollectingSink::new(),
            Recording {
                inner: AllowAll,
                seen: Arc::clone(&seen),
            },
        ),
    )
    .await
    .expect("the run must not hang")
    .expect("the run completes");

    let events = report.sink.into_events();
    let asked = seen.lock().expect("not poisoned").clone();

    assert!(
        !workspace.path().join("made.txt").exists(),
        "a seeded durable allow must not survive an authorizer that refuses"
    );
    assert_eq!(tool_failed(&events, "files"), Some(true));
    assert!(
        asked.is_empty(),
        "a terminal refusal is never put to the approver: {:?}",
        asked_about(&asked)
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "and never surfaced as a request, which is what proves no rule was read"
    );
    assert_eq!(
        tool_failed(&events, "check_background"),
        Some(false),
        "while a read still runs, because the gate answers `Allow` for one"
    );
}

#[tokio::test]
async fn an_allow_all_run_does_not_hang_on_what_it_cannot_ask_about() {
    // What a headless caller passes: `AllowAll` — nobody to ask, so nothing
    // is refused for want of an answer.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (runtime, model) = runtime_writing_a_file(workspace.path());
    let session = session(&runtime, workspace.path(), model);

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(CollectingSink::new(), AllowAll),
    )
    .await
    .expect("the run must not hang waiting on an unanswered approval")
    .expect("the run completes");

    assert_eq!(
        tool_failed(&report.sink.into_events(), "files"),
        Some(false)
    );
    assert!(workspace.path().join("made.txt").exists());
}

#[tokio::test]
async fn a_broken_sink_stops_the_narration_and_not_the_turn() {
    // `basis spawn --json | head`: stdout closes mid-run. Every consequential call
    // now waits on the task that writes those events, so a forwarder that gave
    // up on the first failed write would leave the turn blocked on a permission
    // nobody was left to answer.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (runtime, model) = runtime_writing_a_file(workspace.path());
    let session = session(&runtime, workspace.path(), model);

    let mut written = 0;
    let sink = basis::run::FnSink::new(move |_event| {
        written += 1;
        match written {
            // The header goes through: a run whose first write fails never
            // starts, which is a different story than this one.
            1 => Ok(()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the reader went away",
            )),
        }
    });

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let result = tokio::time::timeout(NOT_STUCK, prepared.execute_with_approver(sink, AllowAll))
        .await
        .expect("a dead reader must not hang the run");

    assert!(
        matches!(result, Err(basis::RunError::Sink(_))),
        "the broken pipe is still reported"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "and the approved write happened anyway"
    );
}

#[tokio::test]
async fn the_approver_is_told_what_the_tool_would_do() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (_, asked) = run_with(workspace.path(), AllowAll).await;

    let request = &asked[0];
    assert!(!request.request_id.is_empty());
    assert_eq!(request.tool_call_id, "call-1");
    assert!(
        request.description.contains("files"),
        "the description should name the tool: {:?}",
        request.description
    );
    assert_eq!(
        request.input["operations"][0]["op"], "create",
        "input must arrive as JSON so an approver can show what changes"
    );
    assert_eq!(
        request.side_effect_level,
        Some(ToolSideEffectLevel::LocalState),
        "and how far the call reaches, which is what a policy is written against"
    );
}

/// A tool that leaves the machine, which is what an MCP server or a
/// `.basis/tools.json` entry declaring `"side_effect": "external"` looks like
/// to the gate.
///
/// The program does not exist, deliberately: nothing here should ever reach it,
/// and if the denial stopped working the tool would fail with a spawn error
/// rather than with the approver's own words — which is what the tests below
/// tell apart.
fn external_tool(workspace: &Path) -> DeclaredTool {
    DeclaredTool::new(
        DeclaredToolSpec {
            name: "publish".to_string(),
            description: "posts the result somewhere off this machine".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
            command: vec![
                workspace
                    .join("no-such-program")
                    .to_string_lossy()
                    .into_owned(),
            ],
            cwd: None,
            env: Vec::new(),
            timeout_ms: None,
            side_effect: SideEffect::External,
        },
        workspace,
    )
}

/// A turn that edits the checkout and then tries to leave the machine: one
/// `LocalState` call and one `External` one, with a read in front of both.
fn runtime_editing_then_publishing(workspace: &Path) -> (Runtime, ModelInfo) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            vec![
                ContentBlock::ToolUse {
                    id: "call-0".to_string(),
                    name: "files".to_string(),
                    input: json!({
                        "operations": [
                            { "op": "create", "path": "made.txt", "content": "hi" }
                        ]
                    }),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "publish".to_string(),
                    input: json!({}),
                },
            ],
            vec![ContentBlock::text("done")],
        ],
    );

    let gate = ApprovalGate::new();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool(external_tool(workspace))
        .with_tool_authorizer(gate)
        .build()
        .expect("runtime builds");

    (runtime, model)
}

#[tokio::test]
async fn an_approver_can_allow_edits_and_deny_the_network_without_naming_a_tool() {
    // The policy `basis::approval`'s own module doc has always named as the
    // reason the seam is a trait, driven through a real run. What makes it
    // worth a test is the *without naming a tool* half: an approver written as
    // a list of tool names silently stops covering the next MCP server a
    // workspace connects or the next program a repository declares, and until
    // `ApprovalRequest` carried the level there was no other way to write it.
    struct EditsButNotTheNetwork;

    #[async_trait]
    impl Approver for EditsButNotTheNetwork {
        async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
            match request.side_effect_level {
                Some(ToolSideEffectLevel::LocalState) => ApprovalDecision::Allow.into(),
                // Including `None`: a level basis could not recover is judged
                // by the most the call could be doing, never the least.
                _ => ApprovalAnswer::new(ApprovalDecision::Deny)
                    .because("this run may change this checkout and nothing beyond it"),
            }
        }
    }

    let workspace = tempfile::tempdir().expect("tempdir");
    let (runtime, model) = runtime_editing_then_publishing(workspace.path());
    let session = session(&runtime, workspace.path(), model);

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(CollectingSink::new(), EditsButNotTheNetwork),
    )
    .await
    .expect("the run must not hang waiting on an unanswered approval")
    .expect("the run completes");

    let events = report.sink.into_events();

    assert_eq!(
        tool_failed(&events, "files"),
        Some(false),
        "an edit to this checkout is what the policy allows"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "and an allowed edit must actually happen"
    );
    assert_eq!(
        tool_failed(&events, "publish"),
        Some(true),
        "and a call that leaves the machine is what it refuses"
    );
    assert_eq!(
        tool_result(&events, "publish").as_deref(),
        Some(
            "Tool execution denied: this run may change this checkout \
             and nothing beyond it"
        ),
        "refused by the approver, not by a program that failed to start"
    );
}
