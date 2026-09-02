//! The dispatcher's promises: the right workspace answers, and a miss fails
//! open for workspace hooks only.

use serde_json::json;

use crate::hooks::{HookOutcome, HookRequest, Interceptor, InterceptorError};

use super::*;

/// An interceptor with one fixed answer, for pinning who was consulted.
struct Answers(&'static str, HookOutcome);

#[async_trait::async_trait]
impl Interceptor for Answers {
    fn name(&self) -> &str {
        self.0
    }

    async fn intercept(&self, _call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        Ok(self.1.clone())
    }
}

fn context(dir: &Path, tool: &str, input: serde_json::Value) -> PreExecutionContext {
    PreExecutionContext {
        agent_id: "agent-1".to_string(),
        tool_name: tool.to_string(),
        tool_call_id: "call-1".to_string(),
        input_json: input.to_string(),
        working_directory: dir.to_path_buf(),
    }
}

fn denying_entry(root: &Path, reason: &'static str) -> WorkspaceGuardEntry {
    WorkspaceGuardEntry {
        runner: Arc::new(
            HookRunner::new(root, Vec::new())
                .with_interceptor(Answers("workspace", HookOutcome::Deny(reason.to_string()))),
        ),
        hooks: Vec::new(),
    }
}

fn permissive_entry(root: &Path) -> WorkspaceGuardEntry {
    WorkspaceGuardEntry {
        runner: Arc::new(HookRunner::new(root, Vec::new())),
        hooks: Vec::new(),
    }
}

async fn decide(dispatch: &HookDispatch, context: &PreExecutionContext) -> HookDecision {
    dispatch
        .pre_tool_execution(context)
        .await
        .expect("the dispatcher never errors")
}

fn registered(
    dispatch: &Arc<HookDispatch>,
    root: &Path,
    entry: WorkspaceGuardEntry,
) -> HookRegistration {
    dispatch
        .register(root, entry)
        .expect("the workspace registers")
}

/// An interceptor that says nothing about a call and rewrites every result.
struct Rewrites(&'static str);

#[async_trait::async_trait]
impl Interceptor for Rewrites {
    fn name(&self) -> &str {
        self.0
    }

    async fn intercept(&self, _call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        Ok(HookOutcome::Allow)
    }

    async fn review(&self, _result: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        Ok(HookOutcome::Replace {
            output: json!(self.0),
            is_error: false,
            reason: None,
        })
    }
}

fn rewriting_entry(root: &Path, name: &'static str) -> WorkspaceGuardEntry {
    WorkspaceGuardEntry {
        runner: Arc::new(HookRunner::new(root, Vec::new()).with_interceptor(Rewrites(name))),
        hooks: Vec::new(),
    }
}

fn finished(dir: &Path, output: &str) -> PostExecutionContext {
    PostExecutionContext {
        agent_id: "agent-1".to_string(),
        tool_name: "spawn".to_string(),
        tool_call_id: "call-1".to_string(),
        input_json: json!({"command": "cat .env"}).to_string(),
        working_directory: dir.to_path_buf(),
        content: mentra::tool::ToolResultContent::text(output),
        is_error: false,
    }
}

async fn review(dispatch: &HookDispatch, context: &PostExecutionContext) -> ResultDecision {
    dispatch
        .post_tool_execution(context)
        .await
        .expect("the dispatcher never errors")
}

fn replaced(decision: ResultDecision) -> String {
    match decision {
        ResultDecision::Replace { content, .. } => content.to_display_string(),
        other => panic!("expected a replacement, got {other:?}"),
    }
}

#[tokio::test]
async fn a_result_is_routed_by_the_same_key_the_call_was() {
    let mine = tempfile::tempdir().expect("tempdir");
    let theirs = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _mine = registered(&dispatch, mine.path(), rewriting_entry(mine.path(), "mine"));
    let _theirs = registered(
        &dispatch,
        theirs.path(),
        rewriting_entry(theirs.path(), "theirs"),
    );

    assert_eq!(
        replaced(review(&dispatch, &finished(mine.path(), "secret")).await),
        "mine"
    );
    assert_eq!(
        replaced(review(&dispatch, &finished(theirs.path(), "secret")).await),
        "theirs"
    );
}

#[tokio::test]
async fn an_unknown_directorys_result_still_reaches_the_host() {
    let elsewhere = tempfile::tempdir().expect("tempdir");

    let bare = Arc::new(HookDispatch::new(Vec::new()));
    assert_eq!(
        review(&bare, &finished(elsewhere.path(), "whatever")).await,
        ResultDecision::Keep,
        "no workspace and no host guard is nobody to ask"
    );

    let guarded = Arc::new(HookDispatch::new(vec![Arc::new(Rewrites("host"))]));
    assert_eq!(
        replaced(review(&guarded, &finished(elsewhere.path(), "whatever")).await),
        "host",
        "a miss fails open for workspace hooks only, on this seam as on the other"
    );
}

#[tokio::test]
async fn a_workspace_with_nobody_to_ask_keeps_its_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(&dispatch, dir.path(), permissive_entry(dir.path()));

    assert_eq!(
        review(&dispatch, &finished(dir.path(), "untouched")).await,
        ResultDecision::Keep
    );
}

#[tokio::test]
async fn a_registered_workspace_is_the_one_consulted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(&dispatch, dir.path(), denying_entry(dir.path(), "mine"));

    let decision = decide(
        &dispatch,
        &context(dir.path(), "files", json!({"operations": []})),
    )
    .await;

    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("mine")),
        "{decision:?}"
    );
}

#[tokio::test]
async fn an_unknown_directory_runs_host_interceptors_and_nothing_else() {
    let known = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");

    // With no host interceptors, a miss allows: there are no workspace hooks
    // to consult because there is no workspace.
    let bare = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = bare.register(known.path(), denying_entry(known.path(), "mine"));
    assert!(matches!(
        decide(&bare, &context(elsewhere.path(), "files", json!({}))).await,
        HookDecision::Allow
    ));

    // With one, the host still speaks — a miss fails open for workspace hooks
    // only, never for the host's own guard.
    let guarded = Arc::new(HookDispatch::new(vec![Arc::new(Answers(
        "host",
        HookOutcome::Deny("host says no".to_string()),
    ))]));
    let decision = decide(&guarded, &context(elsewhere.path(), "files", json!({}))).await;
    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("host says no")),
        "{decision:?}"
    );
}

#[tokio::test]
async fn a_dropped_workspace_stops_being_consulted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));

    let registration = registered(&dispatch, dir.path(), denying_entry(dir.path(), "mine"));
    drop(registration);

    assert!(matches!(
        decide(&dispatch, &context(dir.path(), "files", json!({}))).await,
        HookDecision::Allow
    ));
}

#[tokio::test]
async fn identical_same_root_holders_are_consulted_until_the_last_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));

    let first = registered(&dispatch, dir.path(), denying_entry(dir.path(), "mine"));
    let second = registered(&dispatch, dir.path(), denying_entry(dir.path(), "mine"));

    drop(first);
    let decision = decide(&dispatch, &context(dir.path(), "files", json!({}))).await;
    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("mine")),
        "one identical holder remains: {decision:?}"
    );

    drop(second);
    assert!(matches!(
        decide(&dispatch, &context(dir.path(), "files", json!({}))).await,
        HookDecision::Allow
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_spelling_reaches_the_same_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).expect("dir");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    // Registered under one spelling, dispatched under the other: both
    // canonicalize to the target, so the workspace still answers.
    let _registration = registered(&dispatch, &link, denying_entry(&link, "mine"));

    let decision = decide(&dispatch, &context(&real, "files", json!({}))).await;
    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("mine")),
        "{decision:?}"
    );
}

#[tokio::test]
async fn a_broken_interceptor_fails_closed_through_the_dispatcher() {
    struct Broken;

    #[async_trait::async_trait]
    impl Interceptor for Broken {
        fn name(&self) -> &str {
            "broken"
        }

        async fn intercept(&self, _call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
            Err(std::io::Error::other("the vault is unreachable"))?
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(
        &dispatch,
        dir.path(),
        WorkspaceGuardEntry {
            runner: Arc::new(
                HookRunner::new(dir.path(), Vec::new())
                    .with_reporter(|_| {})
                    .with_interceptor(Broken),
            ),
            hooks: Vec::new(),
        },
    );

    let decision = decide(&dispatch, &context(dir.path(), "files", json!({}))).await;
    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("broken")),
        "fail-closed must survive the move onto the dispatcher: {decision:?}"
    );
}
