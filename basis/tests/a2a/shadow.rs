//! ADR-0037 §6: a bound name stays bound. A teammate made under a name a
//! remote agent also has keeps answering `@<name>` under `auto`, and
//! `reach: remote` is how the remote agent is reached past it.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use basis::{
    AllowAll, CollectingSink, Event, ModelInfo, Provider, Runtime, Workspace, async_trait,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
};
use serde_json::json;

use crate::support::{CardServer, a2a_file, pinned, write};

const PROVIDER: &str = "shadow-provider";
const MODEL: &str = "shadow-model";

/// Answers each request with the next scripted reply, then with text.
struct Scripted {
    replies: Mutex<VecDeque<ContentBlock>>,
}

fn call(id: &str, input: &str, reach: Option<&str>) -> ContentBlock {
    let mut value = json!({ "input": input });
    if let Some(reach) = reach {
        value["reach"] = json!(reach);
    }
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: "spawn".to_string(),
        input: value,
    }
}

#[async_trait]
impl Provider for Scripted {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(PROVIDER)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ContentBlock::text("done"));
        Ok(provider_event_stream_from_response(Response {
            id: "shadow".to_string(),
            model: MODEL.to_string(),
            role: Role::Assistant,
            content: vec![reply],
            stop_reason: None,
            usage: None,
        }))
    }
}

fn model() -> ModelInfo {
    ModelInfo::new(MODEL, PROVIDER).with_context_window(64_000)
}

/// Every `spawn` result of one run, in order.
async fn results(workspace: &Workspace) -> Vec<(bool, String)> {
    let report = workspace
        .prepare("go")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");
    report
        .sink
        .events()
        .iter()
        .filter_map(|event| match event {
            Event::ToolCompleted {
                tool_name,
                is_error,
                summary,
                ..
            } if tool_name == "spawn" => Some((*is_error, summary.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_teammate_keeps_its_name_from_a_remote_agent_until_reach_says_remote() {
    let server = CardServer::start();
    let scripted = Scripted {
        replies: Mutex::new(VecDeque::from([
            // A teammate called `reviewer`, made on purpose beside the
            // remote one.
            call("call-1", "@reviewer look at this", Some("local")),
            ContentBlock::text("the teammate's first word"),
            // `auto` keeps reaching it: the name is bound.
            call("call-2", "@reviewer and this", None),
            ContentBlock::text("the teammate's second word"),
            // `remote` reaches past it.
            call("call-3", "@reviewer and this", Some("remote")),
            ContentBlock::text("done"),
        ])),
    };
    let runtime = Arc::new(
        Runtime::builder()
            .with_provider_instance(scripted)
            .with_ephemeral_history()
            .build()
            .expect("builds"),
    );
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".basis/a2a.json"),
        &a2a_file(&[("reviewer", &server.base)]),
    );
    let workspace = pinned(dir.path(), runtime)
        .with_resolved_model(model())
        .open()
        .await
        .expect("opens");
    assert_eq!(workspace.a2a_agents(), ["reviewer"]);
    let fetched_at_open = server.fetches();

    let results = results(&workspace).await;

    assert_eq!(results.len(), 3, "{results:?}");
    let (failed, first) = &results[0];
    assert!(!failed, "{first}");
    assert!(first.starts_with("Made teammate `reviewer`"), "{first}");
    let (failed, second) = &results[1];
    assert!(!failed, "{second}");
    assert!(
        second.starts_with("Teammate `reviewer`"),
        "bound to the teammate, not moved to the remote agent: {second}"
    );
    assert!(second.ends_with("the teammate's second word"), "{second}");
    let (_, third) = &results[2];
    assert!(
        !third.contains("eammate"),
        "`reach: remote` reaches past the teammate: {third}"
    );
    assert!(
        server.fetches() > fetched_at_open,
        "the remote agent was reached: {third}"
    );
}
