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
        shell: ShellAccess::Granted,
        root: canonical(root),
        shared: true,
    }
}

fn permissive_entry(root: &Path, shell: ShellAccess) -> WorkspaceGuardEntry {
    WorkspaceGuardEntry {
        runner: Arc::new(HookRunner::new(root, Vec::new())),
        shell,
        root: canonical(root),
        shared: true,
    }
}

async fn decide(dispatch: &HookDispatch, context: &PreExecutionContext) -> HookDecision {
    dispatch
        .pre_tool_execution(context)
        .await
        .expect("the dispatcher never errors")
}

#[tokio::test]
async fn a_registered_workspace_is_the_one_consulted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = dispatch.register(denying_entry(dir.path(), "mine"));

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

    let registration = dispatch.register(denying_entry(dir.path(), "mine"));
    drop(registration);

    assert!(matches!(
        decide(&dispatch, &context(dir.path(), "files", json!({}))).await,
        HookDecision::Allow
    ));
}

#[tokio::test]
async fn an_earlier_registrations_drop_does_not_evict_a_later_one() {
    // Two live workspaces on one canonical root: the later registration wins
    // while both live, and the earlier one's drop must remove only itself.
    let dir = tempfile::tempdir().expect("tempdir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));

    let first = dispatch.register(denying_entry(dir.path(), "first"));
    let _second = dispatch.register(denying_entry(dir.path(), "second"));
    drop(first);

    let decision = decide(&dispatch, &context(dir.path(), "files", json!({}))).await;
    assert!(
        matches!(&decision, HookDecision::Deny(reason) if reason.contains("second")),
        "{decision:?}"
    );
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
    let _registration = dispatch.register(denying_entry(&link, "mine"));

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
    let _registration = dispatch.register(permissive_entry(dir.path(), ShellAccess::Denied));

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
    let _second = dispatch.register(permissive_entry(granted.path(), ShellAccess::Granted));
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
async fn writes_into_the_protected_git_paths_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".git/hooks")).expect("hooks dir");
    let dispatch = Arc::new(HookDispatch::new(Vec::new()));
    let _registration = dispatch.register(permissive_entry(dir.path(), ShellAccess::Granted));

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
    let _registration = dispatch.register(WorkspaceGuardEntry {
        runner: Arc::new(
            HookRunner::new(dir.path(), Vec::new())
                .with_reporter(|_| {})
                .with_interceptor(Broken),
        ),
        shell: ShellAccess::Granted,
        root: canonical(dir.path()),
        shared: true,
    });

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
    let _registration = dispatch.register(WorkspaceGuardEntry {
        runner: Arc::new(HookRunner::new(dir.path(), Vec::new())),
        shell: ShellAccess::Denied,
        root: canonical(dir.path()),
        shared: false,
    });

    for (tool, input) in [
        (SPAWN, json!({"input": "!ls"})),
        (
            "files",
            json!({"operations": [{"op": "create", "path": ".git/hooks/pre-commit", "content": "x"}]}),
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
