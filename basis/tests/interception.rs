//! The interception contract's in-process binding, end to end.
//!
//! The unit tests in `src/hooks/` cover the chain's rules and the runner's two
//! adapters. What is worth proving here is that a real runtime consults an
//! interceptor at all, and that a rewritten input is what the tool actually
//! runs on — the only place the whole path (an `Interceptor` -> `HookOutcome`
//! -> `HookDecision` -> the orchestrator substituting the input) is checked
//! end to end rather than in halves.
//!
//! Its sibling is `tests/hooks.rs`, which does the same for the subprocess
//! binding. The tests that need both — who speaks first, and whether a rewrite
//! can slip past the other binding — are here, and are the only ones gated to
//! unix, because a hook fixture is a `/bin/sh` script and an interceptor is not.
//!
//! No network, no subprocess unless a test says so.

use std::path::{Path, PathBuf};

use mentra::{
    ContentBlock, RuntimePolicy, Session,
    agent::{AgentConfig, WorkspaceConfig},
    test::{MockRuntime, MockToolCall},
};
use serde_json::json;

#[cfg(unix)]
use basis::HookSpec;
use basis::{HookOutcome, HookRequest, HookRunner, Interceptor, InterceptorError};

/// An interceptor built from a closure, so a test states only its answer.
struct Scripted {
    name: &'static str,
    answer: fn(&HookRequest) -> Result<HookOutcome, InterceptorError>,
}

impl Scripted {
    fn new(
        name: &'static str,
        answer: fn(&HookRequest) -> Result<HookOutcome, InterceptorError>,
    ) -> Self {
        Self { name, answer }
    }
}

#[async_trait::async_trait]
impl Interceptor for Scripted {
    fn name(&self) -> &str {
        self.name
    }

    async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        (self.answer)(call)
    }
}

/// A workspace to write into, and a runtime whose one scripted turn calls
/// `files` to create `path` inside it.
struct Scenario {
    dir: tempfile::TempDir,
}

impl Scenario {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The runtime, with `runner` installed as the one pre-execution hook —
    /// which is exactly how [`basis::WorkspaceBuilder::open`] installs it.
    fn runtime(&self, runner: HookRunner, creates: &str) -> MockRuntime {
        MockRuntime::builder()
            .with_policy(RuntimePolicy::workspace_bounded(self.path()))
            .with_pre_hook(runner.with_reporter(|_| {}))
            .tool_calls(vec![MockToolCall::new(
                "files",
                json!({"operations": [{"op": "create", "path": creates, "content": "hi"}]}),
            )])
            .text("done")
            .build()
            .expect("the mock runtime builds")
    }

    fn session(&self, mock: &MockRuntime) -> Session {
        mock.runtime()
            .create_session_with_config(
                "test",
                mock.model(),
                AgentConfig {
                    workspace: WorkspaceConfig {
                        base_dir: self.path().to_path_buf(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .expect("session")
    }

    /// Drives the scripted turn and hands back every tool result as text.
    async fn run(&self, mock: &MockRuntime) -> String {
        let mut session = self.session(mock);
        let _ = session.append_turn(vec![ContentBlock::text("go")]).await;

        tool_results(&session)
    }
}

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

/// A runner with interceptors and no hooks file anywhere — the shape a host
/// that has never heard of `.basis/hooks.json` ends up with.
fn only(interceptors: Vec<Scripted>) -> impl FnOnce(&Path) -> HookRunner {
    move |workspace| {
        interceptors
            .into_iter()
            .fold(HookRunner::new(workspace, Vec::new()), |runner, one| {
                runner.with_interceptor(one)
            })
    }
}

#[tokio::test]
async fn an_interceptor_that_allows_leaves_the_call_alone() {
    let scenario = Scenario::new();
    let runner = only(vec![Scripted::new("watcher", |_| Ok(HookOutcome::Allow))])(scenario.path());

    let mock = scenario.runtime(runner, "made.txt");
    scenario.run(&mock).await;

    assert!(
        scenario.file("made.txt").exists(),
        "an interceptor with no objection must not change what happens"
    );
}

#[tokio::test]
async fn the_runtime_consults_an_interceptor_and_a_denial_reaches_the_model() {
    let scenario = Scenario::new();
    let runner = only(vec![Scripted::new("no-writes", |_| {
        Ok(HookOutcome::Deny(
            "this workspace is read-only today".to_string(),
        ))
    })])(scenario.path());

    let mock = scenario.runtime(runner, "made.txt");
    let results = scenario.run(&mock).await;

    assert!(
        !scenario.file("made.txt").exists(),
        "a denied call must not have run"
    );
    assert!(
        results.contains("this workspace is read-only today"),
        "the interceptor's own words must reach the model, not a bare refusal: {results}"
    );
    assert!(
        results.contains("no-writes"),
        "and they must say which interceptor said them: {results}"
    );
}

#[tokio::test]
async fn a_rewritten_input_is_what_the_tool_runs_on() {
    let scenario = Scenario::new();
    // The interceptor rewrites the whole operation, redirecting the write.
    // Nothing but the file system can prove which input the tool saw.
    let runner = only(vec![Scripted::new("redirect", |_| {
        Ok(HookOutcome::Modify {
            input: json!({"operations": [{"op": "create", "path": "approved.txt", "content": "hi"}]}),
            reason: Some("writes go to approved.txt".to_string()),
        })
    })])(scenario.path());

    let mock = scenario.runtime(runner, "wherever.txt");
    let results = scenario.run(&mock).await;

    assert!(
        scenario.file("approved.txt").exists(),
        "the tool must have run on the interceptor's input, not the model's: {results}"
    );
    assert!(
        !scenario.file("wherever.txt").exists(),
        "the model's original path must never have been written"
    );
}

#[tokio::test]
async fn an_interceptor_reads_the_call_it_is_being_asked_about() {
    let scenario = Scenario::new();
    // Deciding from the request is the whole point of handing one over — and
    // the input arrives parsed, so this is a lookup rather than a second parse.
    let runner = only(vec![Scripted::new("inspect", |call| {
        let creates_a_secret = call.input["operations"][0]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with(".env"));

        Ok(if creates_a_secret {
            HookOutcome::Deny(format!("{} may not write credentials", call.tool_name))
        } else {
            HookOutcome::Allow
        })
    })])(scenario.path());

    let mock = scenario.runtime(runner, "secrets.env");
    let results = scenario.run(&mock).await;

    assert!(!scenario.file("secrets.env").exists());
    assert!(
        results.contains("files may not write credentials"),
        "the interceptor judged the call it was shown: {results}"
    );
}

#[tokio::test]
async fn an_interceptor_that_breaks_denies_rather_than_taking_the_turn() {
    for (name, interceptor) in [
        (
            "errors",
            Scripted::new("vault", |_| {
                Err(std::io::Error::other("the vault is unreachable").into())
            }),
        ),
        (
            "panics",
            Scripted::new("confused", |_| panic!("unwrapped a None")),
        ),
    ] {
        let scenario = Scenario::new();
        let runner = only(vec![interceptor])(scenario.path());

        let mock = scenario.runtime(runner, "made.txt");
        let results = scenario.run(&mock).await;

        assert!(
            !scenario.file("made.txt").exists(),
            "{name}: fail-closed means a broken guard blocks the call"
        );
        assert!(
            results.contains("could not answer"),
            "{name}: and says so where whoever has to fix it will read it: {results}"
        );
    }
}

// ---------------------------------------------------------------------------
// Both bindings at once
//
// One contract, two bindings (ADR-0012) — so the interesting cases are the ones
// where both are present and basis's ordering has to hold across them. Gated to
// unix because the other binding's fixtures are `/bin/sh` scripts.
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn sh(name: &str, script: &str) -> HookSpec {
    HookSpec::new(
        name,
        vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
    )
}

#[cfg(unix)]
#[tokio::test]
async fn the_hosts_own_guard_speaks_before_a_workspaces() {
    let scenario = Scenario::new();
    let marker = scenario.file("hook-ran");
    let runner = HookRunner::new(
        scenario.path(),
        vec![sh(
            "repo",
            &format!(
                r#"touch '{}'; echo '{{"decision":"allow"}}'"#,
                marker.display()
            ),
        )],
    )
    .with_interceptor(Scripted::new("host", |_| {
        Ok(HookOutcome::Deny("my program, my rules".to_string()))
    }));

    let mock = scenario.runtime(runner, "made.txt");
    let results = scenario.run(&mock).await;

    assert!(results.contains("my program, my rules"), "{results}");
    assert!(
        !marker.exists(),
        "the host's refusal must land before a program the repository chose is spawned"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_interceptors_rewrite_is_still_re_checked_by_a_hook() {
    // The no-smuggling property `PreExecutionHooks::run` documents. basis's
    // runner owns the ordering, so it is basis's runner that has to keep the
    // property — and keep it *across* the two bindings, not only within one.
    let scenario = Scenario::new();
    let runner = HookRunner::new(
        scenario.path(),
        vec![sh(
            "guard",
            r#"
            request=$(cat)
            case "$request" in
                *'"path":"approved.txt"'*)
                    echo '{"decision":"deny","reason":"no rewrite makes this fine"}' ;;
                *) echo '{"decision":"deny","reason":"saw the original, not the rewrite"}' ;;
            esac
            "#,
        )],
    )
    .with_interceptor(Scripted::new("rewriter", |_| {
        Ok(HookOutcome::Modify {
            input: json!({"operations": [{"op": "create", "path": "approved.txt", "content": "hi"}]}),
            reason: None,
        })
    }));

    let mock = scenario.runtime(runner, "wherever.txt");
    let results = scenario.run(&mock).await;

    assert!(
        results.contains("no rewrite makes this fine"),
        "the hook must have been shown the rewrite, and must still be able to refuse it: {results}"
    );
    assert!(!scenario.file("approved.txt").exists());
    assert!(!scenario.file("wherever.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_hook_can_rewrite_what_an_interceptor_rewrote() {
    // Modifications compose across the bindings the same way they compose
    // within one, and the trail names both hands.
    let scenario = Scenario::new();
    let runner = HookRunner::new(
        scenario.path(),
        vec![sh(
            "narrow",
            r#"
            request=$(cat)
            case "$request" in
                *'"content":"once"'*)
                    echo '{"decision":"modify","input":{"operations":[{"op":"create","path":"final.txt","content":"twice"}]}}' ;;
                *) echo '{"decision":"deny","reason":"saw the original"}' ;;
            esac
            "#,
        )],
    )
    .with_interceptor(Scripted::new("first", |_| {
        Ok(HookOutcome::Modify {
            input: json!({"operations": [{"op": "create", "path": "middle.txt", "content": "once"}]}),
            reason: None,
        })
    }));

    let mock = scenario.runtime(runner, "original.txt");
    let results = scenario.run(&mock).await;

    assert!(
        scenario.file("final.txt").exists(),
        "the tool runs on what the chain left behind: {results}"
    );
    assert_eq!(
        std::fs::read_to_string(scenario.file("final.txt")).expect("read"),
        "twice"
    );
    for abandoned in ["original.txt", "middle.txt"] {
        assert!(
            !scenario.file(abandoned).exists(),
            "{abandoned} was superseded and must never have been written"
        );
    }
}
