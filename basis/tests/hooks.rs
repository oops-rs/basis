#![cfg(unix)]
//! Subprocess hooks, end to end: a real `.basis/hooks.json`, real scripts on
//! disk, real processes.
//!
//! The unit tests in `src/hooks/` cover the pieces. What is worth proving here
//! is the whole path a workspace actually takes — a file someone wrote, hooks
//! discovered from it, a program spawned, and a decision coming back — because
//! every interesting failure of this feature lives in the seams between those.
//!
//! No network, and nothing slower than a shell script. The one test that waits
//! on a clock asserts it did *not* wait long.
//!
//! The last two tests go further and drive a real runtime, because everything
//! above them proves basis's half of the contract and none of it proves that the
//! runtime ever consults a hook — or that a rewritten input reaches the tool.

use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session,
    agent::{AgentConfig, WorkspaceConfig},
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::VolatileRuntimeStore,
    test::{MockRuntime, MockToolCall},
};
use serde_json::json;

use basis::{
    CollectingSink, Event, RunConfig,
    hooks::{
        self, DEFAULT_GLOBAL_HOOKS_FILE, DEFAULT_WORKSPACE_HOOKS_FILE, HookCall, HookConfigError,
        HookOutcome, HookRunner, HooksConfig,
    },
    run::prepare_with_session,
};

/// A workspace with a hooks file and somewhere to put scripts.
struct Workspace {
    dir: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Writes an executable `/bin/sh` script under `hooks/` and returns the
    /// workspace-relative path a hooks file would name.
    fn script(&self, name: &str, body: &str) -> String {
        let dir = self.path().join("hooks");
        fs::create_dir_all(&dir).expect("create hooks dir");

        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

        format!("./hooks/{name}")
    }

    fn hooks_file(&self, body: &str) {
        write_json(&self.path().join(DEFAULT_WORKSPACE_HOOKS_FILE), body);
    }

    fn config(&self) -> HooksConfig {
        HooksConfig {
            workspace_file: PathBuf::from(DEFAULT_WORKSPACE_HOOKS_FILE),
            global_dir: None,
        }
    }

    /// Discovers hooks and puts one tool call to them.
    fn decide(&self, tool_name: &str, input_json: &str) -> HookOutcome {
        let hooks = hooks::load(self.path(), &self.config()).expect("the hooks file parses");

        // Failure reports go nowhere here so the suite stays quiet; the unit
        // tests are where their content is checked.
        HookRunner::new(self.path(), hooks)
            .with_reporter(|_| {})
            .decide(&HookCall::new("agent-1", tool_name, "call-1", input_json))
    }
}

fn write_json(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().expect("a parent")).expect("create dirs");
    fs::write(path, body).expect("write hooks file");
}

fn deny_reason(outcome: HookOutcome) -> String {
    match outcome {
        HookOutcome::Deny(reason) => reason,
        other => panic!("expected the call to be blocked, got {other:?}"),
    }
}

#[test]
fn a_hook_that_allows_lets_the_call_through() {
    let workspace = Workspace::new();
    let script = workspace.script("allow.sh", r#"echo '{"decision":"allow"}'"#);
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "guard", "command": ["{script}"]}}]}}"#
    ));

    assert_eq!(
        workspace.decide("shell", r#"{"command":"ls"}"#),
        HookOutcome::Allow
    );
}

#[test]
fn a_hook_that_denies_blocks_the_call_and_explains() {
    let workspace = Workspace::new();
    let script = workspace.script(
        "deny.sh",
        r#"echo '{"decision":"deny","reason":"force-push is not allowed here"}'"#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "no-force-push", "command": ["{script}"]}}]}}"#
    ));

    let reason = deny_reason(workspace.decide("shell", r#"{"command":"git push --force"}"#));

    assert!(reason.contains("no-force-push"), "got {reason}");
    assert!(
        reason.contains("force-push is not allowed here"),
        "got {reason}"
    );
}

#[test]
fn a_hook_reads_the_call_it_is_being_asked_about() {
    let workspace = Workspace::new();
    // The hook decides from the request it was handed, which is the whole
    // point of the wire contract: deny only the command it was told about.
    let script = workspace.script(
        "inspect.sh",
        r#"
        request=$(cat)
        case "$request" in
            *'"tool_name":"shell"'*'--force'*)
                echo '{"decision":"deny","reason":"saw the force flag"}' ;;
            *) echo '{"decision":"allow"}' ;;
        esac
        "#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "inspect", "command": ["{script}"]}}]}}"#
    ));

    let denied = workspace.decide("shell", r#"{"command":"git push --force"}"#);
    let allowed = workspace.decide("shell", r#"{"command":"git status"}"#);

    assert!(deny_reason(denied).contains("saw the force flag"));
    assert_eq!(allowed, HookOutcome::Allow);
}

#[test]
fn a_hook_is_told_which_schema_it_is_talking_to() {
    let workspace = Workspace::new();
    let script = workspace.script(
        "version.sh",
        r#"
        request=$(cat)
        case "$request" in
            *'"hook_schema":1'*) echo '{"decision":"allow"}' ;;
            *) echo '{"decision":"deny","reason":"unknown basis"}' ;;
        esac
        "#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "version", "command": ["{script}"]}}]}}"#
    ));

    assert_eq!(
        workspace.decide("shell", "{}"),
        HookOutcome::Allow,
        "a hook must be able to check the contract before trusting the rest"
    );
}

#[test]
fn a_hook_that_hangs_is_killed_and_the_call_is_denied() {
    let workspace = Workspace::new();
    let script = workspace.script("hang.sh", "sleep 60");
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [
            {{"name": "stuck", "command": ["{script}"], "timeout_ms": 250}}
        ]}}"#
    ));

    let started = Instant::now();
    let reason = deny_reason(workspace.decide("shell", "{}"));
    let elapsed = started.elapsed();

    assert!(reason.contains("250ms"), "got {reason}");
    assert!(
        elapsed < Duration::from_secs(10),
        "the deadline decides how long a turn waits, not the hook: took {elapsed:?}"
    );
}

#[test]
fn a_hook_that_exits_non_zero_denies_by_default() {
    let workspace = Workspace::new();
    // It printed a perfectly good allow — and then failed. The exit code is
    // what decides whether anything it said counts.
    let script = workspace.script("crash.sh", r#"echo '{"decision":"allow"}'; exit 2"#);
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "crasher", "command": ["{script}"]}}]}}"#
    ));

    let reason = deny_reason(workspace.decide("shell", "{}"));

    assert!(reason.contains("code 2"), "got {reason}");
}

#[test]
fn a_hook_that_prints_something_else_denies_by_default() {
    let workspace = Workspace::new();
    let script = workspace.script("babble.sh", "echo looks fine to me");
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "babbler", "command": ["{script}"]}}]}}"#
    ));

    let reason = deny_reason(workspace.decide("shell", "{}"));

    assert!(reason.contains("not a decision"), "got {reason}");
    assert!(
        reason.contains("looks fine to me"),
        "the reason must quote what it actually printed: {reason}"
    );
}

#[test]
fn a_hook_that_says_nothing_is_not_taken_as_consent() {
    let workspace = Workspace::new();
    let script = workspace.script("mute.sh", "exit 0");
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "mute", "command": ["{script}"]}}]}}"#
    ));

    let reason = deny_reason(workspace.decide("shell", "{}"));

    assert!(reason.contains("printed nothing"), "got {reason}");
}

#[test]
fn a_program_that_does_not_exist_denies_rather_than_disappearing() {
    let workspace = Workspace::new();
    workspace.hooks_file(
        r#"{"schema": 1, "hooks": [{"name": "missing", "command": ["./hooks/never-written.sh"]}]}"#,
    );

    let reason = deny_reason(workspace.decide("shell", "{}"));

    assert!(reason.contains("could not be started"), "got {reason}");
}

#[test]
fn an_observer_hook_can_be_configured_to_fail_open() {
    let workspace = Workspace::new();
    let broken = workspace.script("notify.sh", "exit 1");
    let guard = workspace.script("guard.sh", r#"echo '{"decision":"allow"}'"#);
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [
            {{"name": "notify", "command": ["{broken}"], "on_failure": "allow"}},
            {{"name": "guard", "command": ["{guard}"]}}
        ]}}"#
    ));

    assert_eq!(
        workspace.decide("shell", "{}"),
        HookOutcome::Allow,
        "a broken observer must not cost the turn, when the file says so"
    );
}

#[test]
fn a_hook_only_hears_about_the_tools_it_listed() {
    let workspace = Workspace::new();
    let marker = workspace.path().join("was-asked");
    let script = workspace.script(
        "files-only.sh",
        &format!(
            r#"touch '{}'; echo '{{"decision":"allow"}}'"#,
            marker.display()
        ),
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [
            {{"name": "files-only", "command": ["{script}"], "tools": ["files"]}}
        ]}}"#
    ));

    assert_eq!(workspace.decide("shell", "{}"), HookOutcome::Allow);
    assert!(
        !marker.exists(),
        "a hook scoped to one tool must not even be spawned for another"
    );

    assert_eq!(workspace.decide("files", "{}"), HookOutcome::Allow);
    assert!(marker.exists(), "the tool it listed must reach it");
}

#[test]
fn a_global_denial_stops_a_workspace_hook_from_ever_running() {
    let workspace = Workspace::new();
    let global = workspace.path().join("global");
    let marker = workspace.path().join("workspace-hook-ran");

    let workspace_script = workspace.script(
        "repo.sh",
        &format!(
            r#"touch '{}'; echo '{{"decision":"allow"}}'"#,
            marker.display()
        ),
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "repo", "command": ["{workspace_script}"]}}]}}"#
    ));

    let global_script = workspace.script(
        "personal.sh",
        r#"echo '{"decision":"deny","reason":"my machine, my rules"}'"#,
    );
    write_json(
        &global.join(DEFAULT_GLOBAL_HOOKS_FILE),
        &format!(
            r#"{{"schema": 1, "hooks": [{{"name": "personal", "command": ["{global_script}"]}}]}}"#
        ),
    );

    let hooks = hooks::load(
        workspace.path(),
        &HooksConfig {
            workspace_file: PathBuf::from(DEFAULT_WORKSPACE_HOOKS_FILE),
            global_dir: Some(global),
        },
    )
    .expect("both files parse");

    let outcome = HookRunner::new(workspace.path(), hooks)
        .with_reporter(|_| {})
        .decide(&HookCall::new("agent-1", "shell", "call-1", "{}"));

    assert!(deny_reason(outcome).contains("my machine, my rules"));
    assert!(
        !marker.exists(),
        "the operator's refusal must land before a repository's hook is spawned"
    );
}

#[test]
fn a_hooks_file_that_does_not_parse_stops_the_run() {
    let workspace = Workspace::new();
    workspace.hooks_file(r#"{"schema": 1, "hooks": [ oops"#);

    let error = hooks::load(workspace.path(), &workspace.config()).expect_err("rejected");

    assert!(matches!(error, HookConfigError::Parse { .. }));
    assert!(
        error.to_string().contains(DEFAULT_WORKSPACE_HOOKS_FILE),
        "the error must name the file: {error}"
    );
}

#[test]
fn a_workspace_with_no_hooks_file_costs_nothing() {
    let workspace = Workspace::new();

    let hooks =
        hooks::load(workspace.path(), &workspace.config()).expect("absence is not an error");

    assert!(hooks.is_empty());
    assert_eq!(
        HookRunner::new(workspace.path(), hooks).decide(&HookCall::new("a", "shell", "c", "{}")),
        HookOutcome::Allow
    );
}

#[test]
fn a_hook_can_rewrite_the_call_instead_of_refusing_it() {
    let workspace = Workspace::new();
    // Denying a call with a secret in it costs a round trip and often does not
    // converge; handing back a redacted one does.
    let script = workspace.script(
        "redact.sh",
        r#"echo '{"decision":"modify","input":{"command":"deploy --token REDACTED"},"reason":"stripped a token"}'"#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "redact", "command": ["{script}"]}}]}}"#
    ));

    let outcome = workspace.decide("shell", r#"{"command":"deploy --token hunter2"}"#);

    let HookOutcome::Modify { input, reason } = outcome else {
        panic!("expected a modification, got {outcome:?}");
    };
    assert_eq!(input["command"], "deploy --token REDACTED");
    assert_eq!(
        reason,
        Some("hook 'redact': stripped a token".to_string()),
        "who changed the call belongs in the trail"
    );
}

#[test]
fn modifications_compose_and_a_later_hook_still_decides() {
    let workspace = Workspace::new();
    // Each hook answers based on what it was shown, so the chain's result is
    // only reachable if every hook saw its predecessor's output.
    let first = workspace.script(
        "pin.sh",
        r#"echo '{"decision":"modify","input":{"command":"git push origin main"}}'"#,
    );
    let second = workspace.script(
        "check.sh",
        r#"
        request=$(cat)
        case "$request" in
            *'"command":"git push origin main"'*)
                echo '{"decision":"modify","input":{"command":"git push --dry-run origin main"}}' ;;
            *) echo '{"decision":"deny","reason":"saw the original, not the rewrite"}' ;;
        esac
        "#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [
            {{"name": "pin", "command": ["{first}"]}},
            {{"name": "check", "command": ["{second}"]}}
        ]}}"#
    ));

    let outcome = workspace.decide("shell", r#"{"command":"git push --force"}"#);

    let HookOutcome::Modify { input, reason } = outcome else {
        panic!("expected a modification, got {outcome:?}");
    };
    assert_eq!(input["command"], "git push --dry-run origin main");
    assert_eq!(reason, Some("hook 'pin'; hook 'check'".to_string()));
}

#[test]
fn a_rewrite_cannot_smuggle_a_call_past_a_later_guard() {
    let workspace = Workspace::new();
    let rewriter = workspace.script(
        "rewrite.sh",
        r#"echo '{"decision":"modify","input":{"command":"still bad"}}'"#,
    );
    let guard = workspace.script(
        "guard.sh",
        r#"echo '{"decision":"deny","reason":"no rewrite makes this fine"}'"#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [
            {{"name": "rewrite", "command": ["{rewriter}"]}},
            {{"name": "guard", "command": ["{guard}"]}}
        ]}}"#
    ));

    let reason = deny_reason(workspace.decide("shell", "{}"));

    assert!(
        reason.contains("no rewrite makes this fine"),
        "got {reason}"
    );
}

#[test]
fn a_rewrite_lan_cannot_use_blocks_the_call() {
    let workspace = Workspace::new();
    // Running the original would silently ignore a hook that believed it had
    // intervened, which is the one outcome nobody asked for.
    let script = workspace.script("broken.sh", r#"echo '{"decision":"modify","input":42}'"#);
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "broken", "command": ["{script}"]}}]}}"#
    ));

    let reason = deny_reason(workspace.decide("shell", "{}"));

    assert!(reason.contains("not a JSON object"), "got {reason}");
}

// ---------------------------------------------------------------------------
// Through a real runtime
//
// Everything above proves basis answers correctly when asked. These two prove the
// runtime asks — and that a rewritten input is what the tool actually runs on,
// which is the only place the whole chain (subprocess -> HookOutcome::Modify ->
// HookDecision::Modify -> the orchestrator substituting the input) is checked
// end to end rather than in halves.
// ---------------------------------------------------------------------------

/// A runtime whose one scripted turn calls `files` to create `path`.
fn runtime_creating(workspace: &Workspace, path: &str) -> MockRuntime {
    let hooks = hooks::load(workspace.path(), &workspace.config()).expect("the hooks file parses");

    MockRuntime::builder()
        .with_policy(RuntimePolicy::workspace_bounded(workspace.path()))
        .with_pre_hook(HookRunner::new(workspace.path(), hooks).with_reporter(|_| {}))
        .tool_calls(vec![MockToolCall::new(
            "files",
            json!({"operations": [{"op": "create", "path": path, "content": "hi"}]}),
        )])
        .text("done")
        .build()
        .expect("the mock runtime builds")
}

fn session_in(mock: &MockRuntime, workspace: &Workspace) -> Session {
    mock.runtime()
        .create_session_with_config(
            "test",
            mock.model(),
            AgentConfig {
                workspace: WorkspaceConfig {
                    base_dir: workspace.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session")
}

/// Every tool result the turn produced, as text.
fn tool_results(session: &Session) -> String {
    session
        .replay()
        .items()
        .iter()
        .filter_map(|item| item.message.as_ref())
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_runtime_consults_a_hook_and_a_denial_reaches_the_model() {
    let workspace = Workspace::new();
    let script = workspace.script(
        "no-writes.sh",
        r#"echo '{"decision":"deny","reason":"this workspace is read-only today"}'"#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "no-writes", "command": ["{script}"]}}]}}"#
    ));

    let mock = runtime_creating(&workspace, "made.txt");
    let mut session = session_in(&mock, &workspace);

    let _ = session.append_turn(vec![ContentBlock::text("go")]).await;

    assert!(
        !workspace.path().join("made.txt").exists(),
        "a denied call must not have run"
    );
    let results = tool_results(&session);
    assert!(
        results.contains("this workspace is read-only today"),
        "the hook's own words must reach the model, not a bare refusal: {results}"
    );
    assert!(
        results.contains("no-writes"),
        "and they must say which hook said them: {results}"
    );
}

#[tokio::test]
async fn a_rewritten_input_is_what_the_tool_runs_on() {
    let workspace = Workspace::new();
    // The hook rewrites the whole operation, redirecting the write. Nothing but
    // the file system can prove which input the tool saw.
    let script = workspace.script(
        "redirect.sh",
        r#"echo '{"decision":"modify","input":{"operations":[{"op":"create","path":"approved.txt","content":"hi"}]},"reason":"writes go to approved.txt"}'"#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "redirect", "command": ["{script}"], "tools": ["files"]}}]}}"#
    ));

    let mock = runtime_creating(&workspace, "wherever.txt");
    let mut session = session_in(&mock, &workspace);

    let _ = session.append_turn(vec![ContentBlock::text("go")]).await;

    assert!(
        workspace.path().join("approved.txt").exists(),
        "the tool must have run on the hook's input, not the model's: {}",
        tool_results(&session)
    );
    assert!(
        !workspace.path().join("wherever.txt").exists(),
        "the model's original path must never have been written"
    );
}

// ---------------------------------------------------------------------------
// After the call, through a real runtime
//
// The other half of the contract, and the half that cannot be checked in
// pieces: that what the *model* is handed is the hook's replacement, and that
// what the *stream* carries is still what the tool really returned. Those are
// two readers of one call, and a change that collapsed them would pass every
// test above.
//
// mentra's `MockRuntime` can install a pre-execution hook and not a
// post-execution one, so this drives a runtime of its own — the pattern
// `tests/approval.rs` already uses for the same reason. The provider records
// what it was sent, because the messages handed over on the next round *are*
// the model's view of the result.
// ---------------------------------------------------------------------------

/// Replays a fixed script of assistant turns, remembering every tool result it
/// was sent.
struct ScriptedProvider {
    model: ModelInfo,
    turns: Mutex<VecDeque<Vec<ContentBlock>>>,
    shown: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.shown.lock().expect("not poisoned").extend(
            request
                .messages
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { content, .. } => Some(content.to_string()),
                    _ => None,
                }),
        );

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

/// Runs one scripted turn that reads `path`, with the workspace's hooks on the
/// seam mentra consults after a tool runs.
///
/// Answers the events basis emitted and every tool result the model was shown.
async fn read_through_hooks(workspace: &Workspace, path: &str) -> (Vec<Event>, Vec<String>) {
    let hooks = hooks::load(workspace.path(), &workspace.config()).expect("the hooks file parses");
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let shown = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::builder()
        .with_provider_instance(ScriptedProvider {
            model: model.clone(),
            turns: Mutex::new(VecDeque::from(vec![vec![ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "files".to_string(),
                input: json!({"operations": [{"op": "read", "path": path}]}),
            }]])),
            shown: Arc::clone(&shown),
        })
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(workspace.path()))
        .with_post_hook(HookRunner::new(workspace.path(), hooks).with_reporter(|_| {}))
        .build()
        .expect("the runtime builds");

    let session = runtime
        .create_session_with_config(
            "test",
            model,
            AgentConfig {
                workspace: WorkspaceConfig {
                    base_dir: workspace.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session");

    let config = RunConfig::new(workspace.path(), "read it").with_context(basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    });
    let report = prepare_with_session(session, &config, "openai", "scripted-model")
        .expect("prepared")
        .execute(CollectingSink::new())
        .await
        .expect("the run completes");

    let shown = shown.lock().expect("not poisoned").clone();
    (report.sink.into_events(), shown)
}

/// What [`Event::ToolCompleted`] said about the call, which is the stream's
/// record of what the tool returned.
fn completed_summary(events: &[Event]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            Event::ToolCompleted { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("the tool completed")
}

#[tokio::test]
async fn a_post_hook_replaces_what_the_model_reads_and_not_what_the_stream_says() {
    let workspace = Workspace::new();
    fs::write(
        workspace.path().join("config.rs"),
        "let key = \"AKIA0123\";\n",
    )
    .expect("write the file the tool will read");

    // Answerable only from the output: nothing in `{"op":"read"}` says the
    // file has a credential in it.
    let script = workspace.script(
        "no-secrets.sh",
        r#"
        request=$(cat)
        case "$request" in
            *AKIA*) echo '{"decision":"replace","output":"[redacted]","reason":"a key"}' ;;
            *) echo '{"decision":"allow"}' ;;
        esac
        "#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [
            {{"name": "no-secrets", "command": ["{script}"], "event": "post_tool_use"}}
        ]}}"#
    ));

    let (events, shown) = read_through_hooks(&workspace, "config.rs").await;

    assert_eq!(
        shown,
        vec!["[redacted]".to_string()],
        "the model must read the replacement, and nothing of the original"
    );
    let summary = completed_summary(&events);
    assert!(
        summary.contains("AKIA0123"),
        "the stream is the record of what happened, not of what the model was \
         allowed to see: {summary}"
    );
}

#[tokio::test]
async fn a_hook_that_refuses_a_result_hands_the_model_an_error() {
    let workspace = Workspace::new();
    fs::write(workspace.path().join("config.rs"), "AKIA0123\n").expect("write the file");
    let script = workspace.script(
        "refuse.sh",
        r#"echo '{"decision":"deny","reason":"that file is off limits"}'"#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [
            {{"name": "no-secrets", "command": ["{script}"], "event": "post_tool_use"}}
        ]}}"#
    ));

    let (events, shown) = read_through_hooks(&workspace, "config.rs").await;

    let shown = shown.join("\n");
    assert!(shown.contains("that file is off limits"), "got {shown}");
    assert!(shown.contains("no-secrets"), "got {shown}");
    assert!(!shown.contains("AKIA0123"), "the output must not survive");
    assert!(
        completed_summary(&events).contains("AKIA0123"),
        "and the stream must still say what really happened"
    );
}

#[tokio::test]
async fn a_hook_declared_for_the_other_event_is_not_consulted() {
    let workspace = Workspace::new();
    fs::write(workspace.path().join("config.rs"), "AKIA0123\n").expect("write the file");
    // Declared for the default event, so this run — which installs only the
    // post seam — must never spawn it.
    let script = workspace.script(
        "before.sh",
        r#"echo '{"decision":"replace","output":"should never be asked"}'"#,
    );
    workspace.hooks_file(&format!(
        r#"{{"schema": 1, "hooks": [{{"name": "before", "command": ["{script}"]}}]}}"#
    ));

    let (_, shown) = read_through_hooks(&workspace, "config.rs").await;

    let shown = shown.join("\n");
    assert!(shown.contains("AKIA0123"), "got {shown}");
    assert!(!shown.contains("should never be asked"), "got {shown}");
}
