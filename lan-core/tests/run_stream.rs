//! Assembly-level tests for the run pipeline.
//!
//! These drive a real mentra [`Runtime`](mentra::Runtime) with a scripted
//! provider (`mentra::test::MockRuntime`), so everything between "prompt in"
//! and "JSONL out" is exercised — session lifecycle, event forwarding, the
//! mapping, and the stream's bookends — with no network call and no cost.
//!
//! This is the harness `docs/p0-groundwork.md` §4a records as already shipped
//! in mentra; lan's tests are its first consumer.

use std::path::{Path, PathBuf};

use lan_core::{
    Bound, CollectingSink, Event, JsonlWriter, RunConfig, RunOutcome, run::prepare_with_session,
};
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};
use serde_json::Value;

/// A workspace with an `AGENTS.md`, so context discovery has something to find.
fn workspace_with_context(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), body).expect("write AGENTS.md");
    dir
}

/// Config pinned to the given workspace, with the parent walk and the global
/// file switched off so a real `AGENTS.md` above the temp dir cannot leak in.
fn config(workspace: &Path, prompt: &str) -> RunConfig {
    RunConfig::new(workspace, prompt).with_context(lan_core::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

fn mock(chunks: &[&str]) -> MockRuntime {
    MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .stream_text(chunks.to_vec())
        .build()
        .expect("mock runtime builds")
}

#[tokio::test]
async fn a_run_streams_deltas_between_the_bookends() {
    let workspace = workspace_with_context("Always be brief.");
    let mock = mock(&["Hel", "lo ", "world"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "say hello");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    let report = prepared
        .execute(CollectingSink::new())
        .await
        .expect("run completes");

    assert!(report.succeeded());
    assert_eq!(report.final_message.as_deref(), Some("Hello world"));

    let events = report.sink.into_events();
    assert!(
        matches!(events.first(), Some(Event::RunStarted { .. })),
        "the stream must open with the header"
    );
    assert!(
        matches!(
            events.last(),
            Some(Event::RunFinished {
                outcome: RunOutcome::Ok,
                ..
            })
        ),
        "the stream must close with the outcome"
    );

    let deltas: String = events
        .iter()
        .filter_map(|event| match event {
            Event::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, "Hello world");

    assert!(
        events.iter().any(
            |event| matches!(event, Event::AssistantMessage { text } if text == "Hello world")
        ),
        "the completed message must appear too"
    );
}

#[tokio::test]
async fn the_header_reports_the_context_that_was_loaded() {
    let workspace = workspace_with_context("Prefer small diffs.");
    let mock = mock(&["ok"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "go");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");
    let report = prepared
        .execute(CollectingSink::new())
        .await
        .expect("run completes");

    let Some(Event::RunStarted {
        schema,
        context_files,
        model,
        provider,
        ..
    }) = report.sink.events().first()
    else {
        panic!("expected the header first");
    };

    assert_eq!(*schema, lan_core::EVENT_SCHEMA_VERSION);
    assert_eq!(model, "mock-model");
    assert_eq!(provider, "openai");
    assert_eq!(context_files.len(), 1);
    assert_eq!(context_files[0].scope, "workspace");

    // Discovery resolves symlinks (on macOS the temp dir is one), so the
    // reported path is the resolved spelling — and the header's `workspace`
    // must agree with it rather than echoing what was typed.
    let resolved = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    assert_eq!(context_files[0].path, resolved.join("AGENTS.md"));

    let Some(Event::RunStarted {
        workspace: reported,
        ..
    }) = report.sink.events().first()
    else {
        unreachable!("already matched above");
    };
    assert_eq!(reported, &resolved);
}

#[tokio::test]
async fn workspace_context_reaches_the_model_as_the_system_prompt() {
    let workspace = workspace_with_context("SENTINEL-RULE: never guess.");
    let mock = mock(&["done"]);
    let session = mock
        .runtime()
        .create_session_with_config(
            "test",
            mock.model(),
            mentra::agent::AgentConfig {
                system: lan_core::WorkspaceContext::discover_with(
                    workspace.path(),
                    &lan_core::ContextConfig {
                        file_name: "AGENTS.md".to_string(),
                        global_dir: None,
                        walk_parents: false,
                    },
                )
                .expect("discovery")
                .render(),
                ..Default::default()
            },
        )
        .expect("session");

    let config = config(workspace.path(), "go");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");
    prepared
        .execute(CollectingSink::new())
        .await
        .expect("run completes");

    let requests = mock.recorded_requests().await;
    let system = requests
        .first()
        .expect("a request was made")
        .system
        .as_deref()
        .expect("a system prompt was sent");

    assert!(
        system.contains("SENTINEL-RULE: never guess."),
        "the workspace's own instructions must reach the model"
    );
}

#[tokio::test]
async fn tool_calls_appear_on_the_stream_with_parsed_input() {
    let workspace = workspace_with_context("rules");
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .tool_calls(vec![MockToolCall::new(
            "files",
            serde_json::json!({"operations": [{"op": "list", "path": "."}]}),
        )])
        .text("listed")
        .build()
        .expect("mock runtime builds");
    let session = mock
        .runtime()
        .create_session_with_config(
            "test",
            mock.model(),
            mentra::agent::AgentConfig {
                workspace: mentra::agent::WorkspaceConfig {
                    base_dir: workspace.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session");

    let config = config(workspace.path(), "list the files");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");
    let report = prepared
        .execute(CollectingSink::new())
        .await
        .expect("run completes");

    let events = report.sink.into_events();
    let queued = events
        .iter()
        .find_map(|event| match event {
            Event::ToolQueued {
                tool_name, input, ..
            } => Some((tool_name, input)),
            _ => None,
        })
        .expect("a tool was queued");

    assert_eq!(queued.0, "files");
    assert_eq!(
        queued.1["operations"][0]["op"], "list",
        "tool input must arrive as JSON, not a JSON-encoded string"
    );

    let completed = events
        .iter()
        .find_map(|event| match event {
            Event::ToolCompleted {
                tool_call_id,
                tool_name,
                is_error,
                ..
            } => Some((tool_call_id.as_str(), tool_name.as_str(), *is_error)),
            _ => None,
        })
        .expect("the call must also be reported as completed");

    assert_eq!(completed.0, "tool-1");
    assert_eq!(
        completed.1, "files",
        "completion must name its tool (oops-rs/mentra#9)"
    );
    assert!(!completed.2, "the scripted call should succeed");
}

#[tokio::test]
async fn the_jsonl_rendering_is_one_parseable_object_per_line() {
    let workspace = workspace_with_context("rules");
    let mock = mock(&["multi\nline", " answer"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "go");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");
    let report = prepared
        .execute(JsonlWriter::new(Vec::new()))
        .await
        .expect("run completes");

    let output = String::from_utf8(report.sink.into_inner()).expect("utf-8");
    let lines: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line parses as json"))
        .collect();

    assert_eq!(lines[0]["type"], "run_started");
    assert_eq!(lines[0]["schema"], lan_core::EVENT_SCHEMA_VERSION);
    assert_eq!(lines[lines.len() - 1]["type"], "run_finished");
    assert_eq!(lines[lines.len() - 1]["status"], "ok");

    let sequence: Vec<u64> = lines
        .iter()
        .map(|line| line["seq"].as_u64().expect("a sequence number"))
        .collect();
    let expected: Vec<u64> = (0..lines.len() as u64).collect();
    assert_eq!(
        sequence, expected,
        "sequence numbers must be dense and ordered"
    );
}

#[tokio::test]
async fn a_failing_turn_still_closes_the_stream() {
    let workspace = workspace_with_context("rules");
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .failure(mentra::ProviderError::UnsupportedCapability(
            "scripted failure".to_string(),
        ))
        .build()
        .expect("mock runtime builds");
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "go");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");
    let report = prepared
        .execute(CollectingSink::new())
        .await
        .expect("the run itself reports rather than erroring");

    assert!(!report.succeeded());
    assert_eq!(report.final_message, None);

    let events = report.sink.into_events();
    assert!(matches!(events.first(), Some(Event::RunStarted { .. })));
    assert!(
        matches!(
            events.last(),
            Some(Event::RunFinished {
                outcome: RunOutcome::Error { .. },
                ..
            })
        ),
        "a failed turn must still terminate the stream"
    );
}

/// A run whose deadline has already passed, so the bound trips on the first
/// check rather than after a wall-clock wait.
#[tokio::test]
async fn a_tripped_bound_is_reported_as_a_bound_not_just_a_failure() {
    let workspace = workspace_with_context("rules");
    let mock = mock(&["never streamed"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "go").with_deadline(std::time::Duration::ZERO);
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    let report = prepared
        .execute(CollectingSink::new())
        .await
        .expect("a bound ends the run, it does not break it");

    // Both halves matter. The stream still says the run did not finish, which
    // is what a client reading events needs; and the report says *why*, which
    // is what the exit code needs — ADR-0015 asks a shell script to tell "out
    // of time" from "the provider refused" without parsing a message.
    assert!(!report.succeeded());
    assert_eq!(report.stopped_by, Some(Bound::Deadline));

    assert!(
        matches!(
            report.sink.into_events().last(),
            Some(Event::RunFinished {
                outcome: RunOutcome::Error { .. },
                ..
            })
        ),
        "a bounded run must still terminate the stream"
    );
}

#[tokio::test]
async fn a_healthy_run_ran_into_no_bound() {
    let workspace = workspace_with_context("rules");
    let mock = mock(&["done"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "go");
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    let report = prepared
        .execute(CollectingSink::new())
        .await
        .expect("run completes");

    assert!(report.succeeded());
    assert_eq!(report.stopped_by, None);
}

#[tokio::test]
async fn an_empty_prompt_is_refused_when_it_would_be_sent() {
    let workspace = workspace_with_context("rules");
    let mock = mock(&["unused"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(workspace.path(), "  \t\n ");

    // Preparing is fine — a session with nothing said yet is what ACP's
    // `session/new` opens. Sending is where the prompt has to be real.
    let mut prepared =
        prepare_with_session(session, &config, "openai", "mock-model").expect("prepared");

    assert!(prepared.execute(CollectingSink::new()).await.is_err());
}

#[tokio::test]
async fn a_missing_workspace_is_refused() {
    let mock = mock(&["unused"]);
    let session = mock
        .runtime()
        .create_session("test", mock.model())
        .expect("session");

    let config = config(&PathBuf::from("/definitely/not/a/real/path"), "go");

    assert!(prepare_with_session(session, &config, "openai", "mock-model").is_err());
}

/// The `.git` carve-out, proven where it matters: through a real runtime, on a
/// tool call the model actually makes.
///
/// A file under `.git/hooks` is a program git runs on the next commit, so an
/// agent that can write one executes code outside anything lan's approval or
/// policy governs. lan denies those paths by default; this asserts the denial
/// reaches the tool rather than living only in a config field.
#[tokio::test]
async fn a_write_into_git_hooks_is_refused() {
    let workspace = workspace_with_context("rules");
    std::fs::create_dir_all(workspace.path().join(".git").join("hooks")).expect("hooks dir");

    let policy = mentra::RuntimePolicy::workspace_bounded(workspace.path())
        .with_denied_write_root(workspace.path().join(".git").join("hooks"));

    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(policy)
        .tool_calls(vec![MockToolCall::new(
            "files",
            serde_json::json!({
                "operations": [{
                    "op": "create",
                    "path": ".git/hooks/pre-commit",
                    "content": "#!/bin/sh\nexfiltrate\n"
                }]
            }),
        )])
        .text("could not")
        .build()
        .expect("mock runtime builds");

    let session = mock
        .runtime()
        .create_session_with_config(
            "test",
            mock.model(),
            mentra::agent::AgentConfig {
                workspace: mentra::agent::WorkspaceConfig {
                    base_dir: workspace.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session");

    let config = config(workspace.path(), "install a hook");
    let report = prepare_with_session(session, &config, "openai", "mock-model")
        .expect("prepared")
        .execute(CollectingSink::new())
        .await
        .expect("the run reports rather than erroring");

    let failed = report
        .sink
        .events()
        .iter()
        .find_map(|event| match event {
            Event::ToolCompleted { is_error, .. } => Some(*is_error),
            _ => None,
        })
        .expect("the write was attempted");

    assert!(failed, "writing a git hook must fail");
    assert!(
        !workspace.path().join(".git/hooks/pre-commit").exists(),
        "and must not reach the disk"
    );
}
