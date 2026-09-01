//! The dispatcher's promises: the right workspace answers, the guards speak
//! first, and a miss fails open for workspace hooks only.

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
        shell: ShellAccess::Granted,
        root: canonical(root),
        foreign_tools: Default::default(),
        shared: true,
    }
}

fn permissive_entry(root: &Path, shell: ShellAccess) -> WorkspaceGuardEntry {
    WorkspaceGuardEntry {
        runner: Arc::new(HookRunner::new(root, Vec::new())),
        hooks: Vec::new(),
        shell,
        root: canonical(root),
        foreign_tools: Default::default(),
        shared: true,
    }
}

async fn decide(dispatch: &HookDispatch, context: &PreExecutionContext) -> HookDecision {
    dispatch
        .pre_tool_execution(context)
        .await
        .expect("the dispatcher never errors")
}

fn registered(dispatch: &Arc<HookDispatch>, entry: WorkspaceGuardEntry) -> HookRegistration {
    dispatch.register(entry).expect("the guard registers")
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
        shell: ShellAccess::Granted,
        root: canonical(root),
        foreign_tools: Default::default(),
        shared: true,
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
    let _mine = registered(&dispatch, rewriting_entry(mine.path(), "mine"));
    let _theirs = registered(&dispatch, rewriting_entry(theirs.path(), "theirs"));

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
    let _registration = registered(
        &dispatch,
        permissive_entry(dir.path(), ShellAccess::Granted),
    );

    assert_eq!(
        review(&dispatch, &finished(dir.path(), "untouched")).await,
        ResultDecision::Keep
    );
}

#[tokio::test]
async fn basis_own_guards_have_nothing_to_say_about_a_result() {
    // They decide whether a call happens. This one has, and its output is
    // not theirs to judge — a workspace with commands off that somehow ran
    // one is a bug in the other seam, not something to re-litigate here.
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(&dispatch, permissive_entry(dir.path(), ShellAccess::Denied));

    assert_eq!(
        review(&dispatch, &finished(dir.path(), "output of a command")).await,
        ResultDecision::Keep
    );
}

#[tokio::test]
async fn a_registered_workspace_is_the_one_consulted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(&dispatch, denying_entry(dir.path(), "mine"));

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
    let _registration = bare.register(denying_entry(known.path(), "mine"));
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

    let registration = registered(&dispatch, denying_entry(dir.path(), "mine"));
    drop(registration);

    assert!(matches!(
        decide(&dispatch, &context(dir.path(), "files", json!({}))).await,
        HookDecision::Allow
    ));
}

#[tokio::test]
async fn identical_same_root_holders_share_state_until_the_last_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));

    let first = registered(
        &dispatch,
        permissive_entry(dir.path(), ShellAccess::Granted),
    );
    let second = registered(
        &dispatch,
        permissive_entry(dir.path(), ShellAccess::Granted),
    );
    let first_foreign = first.foreign_tools();
    let second_foreign = second.foreign_tools();
    assert!(Arc::ptr_eq(&first_foreign, &second_foreign));
    first_foreign
        .write()
        .expect("foreign set")
        .insert("sibling_tool".to_string());
    assert_eq!(
        dispatch.foreign_tools(dir.path()),
        BTreeSet::from(["sibling_tool".to_string()])
    );

    drop(first);
    assert_eq!(
        dispatch.foreign_tools(dir.path()),
        BTreeSet::from(["sibling_tool".to_string()]),
        "one identical holder remains"
    );

    drop(second);
    assert!(dispatch.foreign_tools(dir.path()).is_empty());
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
    let _registration = registered(&dispatch, denying_entry(&link, "mine"));

    let decision = decide(&dispatch, &context(&real, "files", json!({}))).await;
    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("mine")),
        "{decision:?}"
    );
}

#[tokio::test]
async fn a_shell_denied_workspace_loses_spawns_command_mode_and_keeps_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(&dispatch, permissive_entry(dir.path(), ShellAccess::Denied));

    let command = decide(
        &dispatch,
        &context(dir.path(), SPAWN, json!({"input": "!rm -rf /"})),
    )
    .await;
    assert!(
        matches!(&command, HookDecision::Deny(reason) if reason.contains("commands off")),
        "{command:?}"
    );

    // A delegation is not a command, and `!!` is the escape that makes a
    // prompt of one — the guard reads the prefix through spawn's own parser,
    // so both pass exactly where the tool itself would treat them as prompts.
    for input in [
        json!({"input": "summarise the TODOs"}),
        json!({"input": "!!literal"}),
    ] {
        assert!(
            matches!(
                decide(&dispatch, &context(dir.path(), SPAWN, input.clone())).await,
                HookDecision::Allow
            ),
            "{input}"
        );
    }

    // Granted workspaces keep command mode: the guard is the posture's, not
    // the tool's.
    let granted = tempfile::tempdir().expect("tempdir");
    let _second = registered(
        &dispatch,
        permissive_entry(granted.path(), ShellAccess::Granted),
    );
    assert!(matches!(
        decide(
            &dispatch,
            &context(granted.path(), SPAWN, json!({"input": "!ls"}))
        )
        .await,
        HookDecision::Allow
    ));
}

#[tokio::test]
async fn a_shell_denied_workspace_refuses_a_targeted_command_too() {
    // ADR-0021's seventh point, pinned where it could silently stop being
    // true. A targeted command is still `Mode::Command`, and this guard reads
    // the prefix through spawn's own parser — so naming a destination can
    // never be a way past the posture that decides whether this workspace runs
    // commands at all. The guard needs no code change to hold; it needs a test
    // that says so, because "no change needed" is a claim and not a fact.
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(&dispatch, permissive_entry(dir.path(), ShellAccess::Denied));

    for input in [
        json!({"input": "!@mac ls"}),
        json!({"input": "!@build-box rm -rf /"}),
    ] {
        let decision = decide(&dispatch, &context(dir.path(), SPAWN, input.clone())).await;
        assert!(
            matches!(&decision, HookDecision::Deny(reason) if reason.contains("commands off")),
            "{input}: {decision:?}"
        );
    }

    // And a workspace that allows commands still allows a targeted one: the
    // guard is the posture's, not the routing's.
    let granted = tempfile::tempdir().expect("tempdir");
    let _second = registered(
        &dispatch,
        permissive_entry(granted.path(), ShellAccess::Granted),
    );
    assert!(matches!(
        decide(
            &dispatch,
            &context(granted.path(), SPAWN, json!({"input": "!@mac ls"}))
        )
        .await,
        HookDecision::Allow
    ));
}

#[tokio::test]
async fn writes_into_the_protected_git_paths_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".git/hooks")).expect("hooks dir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(
        &dispatch,
        permissive_entry(dir.path(), ShellAccess::Granted),
    );

    // Every spelling of the same place, including the traversal that a naive
    // prefix check would miss — the policy version's own test cases, ported.
    let denied = [
        json!({"operations": [{"op": "create", "path": ".git/hooks/pre-commit", "content": "x"}]}),
        json!({"operations": [{"op": "set", "path": ".git/hooks/../hooks/pre-commit", "content": "x"}]}),
        json!({"operations": [{"op": "replace", "path": ".git/config", "old": "a", "new": "b"}]}),
        json!({"operations": [{"op": "move", "from": "innocent.txt", "to": ".git/hooks/post-merge"}]}),
        json!({"operations": [{"op": "delete", "path": ".git/hooks/pre-push"}]}),
    ];
    for input in denied {
        let decision = decide(&dispatch, &context(dir.path(), "files", input.clone())).await;
        assert!(
            matches!(&decision, HookDecision::Deny(reason) if reason.contains("protected git paths")),
            "{input} -> {decision:?}"
        );
    }

    // The carve-out is exactly two paths: the rest of `.git` and the rest of
    // the workspace stay writable, or `git` itself would stop working.
    let allowed = [
        json!({"operations": [{"op": "create", "path": "src/main.rs", "content": "x"}]}),
        json!({"operations": [{"op": "create", "path": ".git/info/exclude", "content": "x"}]}),
        json!({"operations": [{"op": "read", "path": ".git/hooks/pre-commit"}]}),
    ];
    for input in allowed {
        assert!(
            matches!(
                decide(&dispatch, &context(dir.path(), "files", input.clone())).await,
                HookDecision::Allow
            ),
            "{input}"
        );
    }
}

#[tokio::test]
async fn a_broken_workspace_guard_fails_closed_through_the_dispatcher() {
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
        WorkspaceGuardEntry {
            runner: Arc::new(
                HookRunner::new(dir.path(), Vec::new())
                    .with_reporter(|_| {})
                    .with_interceptor(Broken),
            ),
            hooks: Vec::new(),
            shell: ShellAccess::Granted,
            root: canonical(dir.path()),
            foreign_tools: Default::default(),
            shared: true,
        },
    );

    let decision = decide(&dispatch, &context(dir.path(), "files", json!({}))).await;
    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("broken")),
        "fail-closed must survive the move onto the dispatcher: {decision:?}"
    );
}

#[tokio::test]
async fn a_private_runtimes_workspace_leaves_the_guards_to_its_policy() {
    // The private path bakes the shell posture and the `.git` carve-out into
    // `RuntimePolicy` at build (`RuntimeBuilder::build_for`), and its denials
    // must keep arriving in mentra's words — so the dispatcher's guards stand
    // down, and only the workspace's own participants run.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".git/hooks")).expect("hooks dir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(
        &dispatch,
        WorkspaceGuardEntry {
            runner: Arc::new(HookRunner::new(dir.path(), Vec::new())),
            hooks: Vec::new(),
            shell: ShellAccess::Denied,
            root: canonical(dir.path()),
            foreign_tools: Default::default(),
            shared: false,
        },
    );

    for (tool, input) in [
        (SPAWN, json!({"input": "!ls"})),
        (
            "files",
            json!({"operations": [{"op": "create", "path": ".git/hooks/pre-commit", "content": "x"}]}),
        ),
        // Both profiles, because the policy the dispatcher stands down for
        // binds at the workspace *engine* — `WorkspaceEditor::authorize_write`,
        // which the batched ops and the split writers both call — so it needs
        // no name list and covers whichever roster the runtime offers.
        (
            "write",
            json!({"path": ".git/hooks/pre-commit", "content": "x"}),
        ),
        (
            "edit",
            json!({"path": ".git/config", "edits": [{"old_string": "a", "new_string": "b"}]}),
        ),
    ] {
        assert!(
            matches!(
                decide(&dispatch, &context(dir.path(), tool, input.clone())).await,
                HookDecision::Allow
            ),
            "{tool}: policy, not the dispatcher, refuses on the private path: {input}"
        );
    }
}

#[tokio::test]
async fn the_split_writers_reach_the_same_protected_git_paths() {
    // The guard keyed on `files` alone, and the split profile renames the act
    // rather than changing it: `write` and `edit` reach the same
    // `WorkspaceEditor` the batched `create`/`replace` ops do, so a guard that
    // knew only one name was a guard the other walked past on every shared
    // runtime. Each spelling of the path field is here because mentra accepts
    // three — `path`, `file_path` and `filePath` are serde aliases on one
    // field — and a guard reading only the first would be bypassed by asking
    // for the second.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".git/hooks")).expect("hooks dir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(
        &dispatch,
        permissive_entry(dir.path(), ShellAccess::Granted),
    );

    let denied = [
        (
            "write",
            json!({"path": ".git/hooks/pre-commit", "content": "x"}),
        ),
        (
            "write",
            json!({"file_path": ".git/hooks/pre-commit", "content": "x"}),
        ),
        (
            "write",
            json!({"filePath": ".git/hooks/pre-commit", "content": "x"}),
        ),
        (
            "write",
            json!({"path": ".git/hooks/../hooks/pre-commit", "content": "x"}),
        ),
        ("write", json!({"path": ".git/config", "content": "x"})),
        (
            "edit",
            json!({"path": ".git/config", "edits": [{"old_string": "a", "new_string": "b"}]}),
        ),
        (
            "edit",
            json!({"file_path": ".git/hooks/pre-push", "edits": [{"old_string": "a", "new_string": "b"}]}),
        ),
    ];
    for (tool, input) in denied {
        let decision = decide(&dispatch, &context(dir.path(), tool, input.clone())).await;
        assert!(
            matches!(&decision, HookDecision::Deny(reason) if reason.contains("protected git paths")),
            "{tool} {input} -> {decision:?}"
        );
    }

    // The carve-out is still exactly two paths, and a reader is not a writer:
    // `read`, `ls`, `grep` and `glob` never reach `authorize_write`, so
    // refusing them here would deny what the policy version allows.
    let allowed = [
        ("write", json!({"path": "src/main.rs", "content": "x"})),
        (
            "write",
            json!({"path": ".git/info/exclude", "content": "x"}),
        ),
        ("read", json!({"path": ".git/hooks/pre-commit"})),
        ("ls", json!({"path": ".git/hooks"})),
        ("grep", json!({"pattern": "curl", "path": ".git/hooks"})),
        ("glob", json!({"pattern": ".git/hooks/*"})),
    ];
    for (tool, input) in allowed {
        assert!(
            matches!(
                decide(&dispatch, &context(dir.path(), tool, input.clone())).await,
                HookDecision::Allow
            ),
            "{tool} {input}"
        );
    }
}

/// An interceptor that rewrites every call it sees into one fixed input.
///
/// The hazard the guard-on-the-final-input tests describe, in the smallest
/// participant that can produce it: what the model asked for is innocent, and
/// what the tool would run on is not.
struct RewritesInput(&'static str, serde_json::Value);

#[async_trait::async_trait]
impl Interceptor for RewritesInput {
    fn name(&self) -> &str {
        self.0
    }

    async fn intercept(&self, _call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        Ok(HookOutcome::Modify {
            input: self.1.clone(),
            reason: Some(format!("rewritten by {}", self.0)),
        })
    }
}

fn rewriting_input_entry(
    root: &Path,
    shell: ShellAccess,
    input: serde_json::Value,
) -> WorkspaceGuardEntry {
    WorkspaceGuardEntry {
        runner: Arc::new(
            HookRunner::new(root, Vec::new()).with_interceptor(RewritesInput("rewrite", input)),
        ),
        hooks: Vec::new(),
        shell,
        root: canonical(root),
        foreign_tools: Default::default(),
        shared: true,
    }
}

#[tokio::test]
async fn a_rewrite_into_the_protected_git_paths_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".git")).expect("git dir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(
        &dispatch,
        rewriting_input_entry(
            dir.path(),
            ShellAccess::Granted,
            json!({"operations": [{"op": "set", "path": ".git/config", "content": "x"}]}),
        ),
    );

    // The model asked for something the guard has no quarrel with. Only the
    // input the tool would run on is protected, so only judging that catches it.
    let decision = decide(
        &dispatch,
        &context(
            dir.path(),
            "files",
            json!({"operations": [{"op": "create", "path": "src/main.rs", "content": "x"}]}),
        ),
    )
    .await;

    let HookDecision::Deny(reason) = &decision else {
        panic!("expected the rewrite to be refused, got {decision:?}");
    };
    assert!(reason.contains("protected git paths"), "{reason}");
    // The model asked to write `src/main.rs`; a refusal naming only the path
    // it never wrote sends it correcting somebody else's input.
    assert!(
        reason.contains("rewrite") && reason.contains("rewritten by rewrite"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_rewrite_into_a_command_is_refused_when_commands_are_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(
        &dispatch,
        rewriting_input_entry(
            dir.path(),
            ShellAccess::Denied,
            json!({"input": "!rm -rf /"}),
        ),
    );

    let decision = decide(
        &dispatch,
        &context(dir.path(), SPAWN, json!({"input": "summarise the TODOs"})),
    )
    .await;

    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("commands off")),
        "{decision:?}"
    );
}

#[tokio::test]
async fn an_innocent_rewrite_still_reaches_the_tool() {
    // The guard judges the rewrite; it does not distrust rewriting.
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = registered(
        &dispatch,
        rewriting_input_entry(
            dir.path(),
            ShellAccess::Denied,
            json!({"operations": [{"op": "create", "path": "approved.txt", "content": "x"}]}),
        ),
    );

    let decision = decide(
        &dispatch,
        &context(
            dir.path(),
            "files",
            json!({"operations": [{"op": "create", "path": "wherever.txt", "content": "x"}]}),
        ),
    )
    .await;

    let HookDecision::Modify { input_json, .. } = &decision else {
        panic!("expected the rewrite to survive, got {decision:?}");
    };
    assert!(input_json.contains("approved.txt"), "{input_json}");
}
