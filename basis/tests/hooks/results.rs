//! Subprocess hooks after the call, end to end.
//!
//! The other half of the contract, and the half that cannot be checked in
//! pieces: that what the *model* is handed is the hook's replacement, and that
//! what the *stream* carries is still what the tool really returned. Those are
//! two readers of one call, and a change that collapsed them would pass every
//! test in the parent module.
//!
//! mentra's `MockRuntime` can install a pre-execution hook and not a
//! post-execution one, so this drives a runtime of its own — the pattern
//! `tests/approval.rs` already uses for the same reason. The provider records
//! what it was sent, because the messages handed over on the next round *are*
//! the model's view of the result.

use std::{
    collections::VecDeque,
    fs,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy,
    agent::{AgentConfig, WorkspaceConfig},
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::VolatileRuntimeStore,
};
use serde_json::json;

use basis::{CollectingSink, Event, hooks, hooks::HookRunner, run::prepare_with_session};

use super::Workspace;

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

    let context = basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    };
    let report = prepare_with_session(
        session,
        workspace.path(),
        "read it",
        &context,
        "openai",
        "scripted-model",
    )
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
