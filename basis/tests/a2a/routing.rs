//! ADR-0029 §3 after a reload (ADR-0031): who can reach whom is still decided
//! per open. A reload adds an agent to the open that reloaded and to nobody
//! else — not a sibling open of the same directory, and not another directory
//! on the same runtime — so a guessed `@nous` from either is refused before
//! anything is sent.

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
    tools::spawn::{Reach, ReachPolicy},
};
use serde_json::json;

use crate::support::{CardServer, a2a_file, pinned, write};

const PROVIDER: &str = "routing-provider";
const MODEL: &str = "routing-model";

/// Answers each request with the next scripted reply, then with text.
#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<ContentBlock>>,
}

impl Scripted {
    /// Every run here asks for `@nous` on its first round and stops on its
    /// second.
    fn asking_for_nous(runs: usize) -> Self {
        let replies = (0..runs)
            .flat_map(|run| {
                [
                    ContentBlock::ToolUse {
                        id: format!("call-{run}"),
                        name: "spawn".to_string(),
                        input: json!({"input": "@nous what is the answer?"}),
                    },
                    ContentBlock::text("done"),
                ]
            })
            .collect();
        Self {
            replies: Mutex::new(replies),
        }
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
            id: "routing".to_string(),
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

/// What `spawn` answered the run's `@nous`.
async fn asks_for_nous(workspace: &Workspace) -> (bool, String) {
    let report = workspace
        .prepare("ask nous")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");
    report
        .sink
        .events()
        .iter()
        .find_map(|event| match event {
            Event::ToolCompleted {
                tool_name,
                is_error,
                summary,
                ..
            } if tool_name == "spawn" => Some((*is_error, summary.clone())),
            _ => None,
        })
        .expect("spawn ran")
}

#[tokio::test]
async fn a_reload_reaches_only_the_open_that_reloaded() {
    let server = CardServer::start();
    let runtime = Arc::new(
        Runtime::builder()
            .with_provider_instance(Scripted::asking_for_nous(3))
            .with_ephemeral_history()
            // Remote only, so a guess at a name is a refusal rather than a
            // teammate (ADR-0037 §3): what is under test is that the guess
            // reaches nothing, and a refusal says so directly.
            .with_reach_policy(ReachPolicy::fixed(Reach::Remote))
            .build()
            .expect("builds"),
    );
    let (mine, theirs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let open = |root: &std::path::Path| {
        pinned(root, Arc::clone(&runtime))
            .with_resolved_model(model())
            .open()
    };
    let owner = open(mine.path()).await.expect("opens");
    let sibling = open(mine.path()).await.expect("the same directory again");
    let elsewhere = open(theirs.path()).await.expect("another directory");

    write(
        &mine.path().join(".basis/a2a.json"),
        &a2a_file(&[("nous", &server.base)]),
    );
    owner.reload_a2a().await.expect("reloads");
    elsewhere
        .reload_a2a()
        .await
        .expect("reloads, finding nothing");
    assert_eq!(owner.remote_agents(), ["nous"]);
    assert!(
        sibling.remote_agents().is_empty(),
        "the sibling did not reload"
    );

    let (_, routed) = asks_for_nous(&owner).await;
    assert!(
        !routed.contains("no remote agents"),
        "the owner's `@nous` was routed, whatever the card-only server made of it: {routed}"
    );
    let reached = server.fetches();
    for (who, workspace) in [("sibling", &sibling), ("elsewhere", &elsewhere)] {
        let (failed, refusal) = asks_for_nous(workspace).await;
        assert!(failed, "{who}");
        assert!(
            refusal.contains("no remote agents"),
            "{who}'s guess is refused before anything is sent: {refusal}"
        );
    }
    assert_eq!(server.fetches(), reached, "neither guess reached the agent");
}
