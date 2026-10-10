//! ADR-0031 §3: teaching follows the set. A session already running is told
//! about an agent a reload added from its very next model request — read off
//! what the provider is actually sent — while a sibling open of the same
//! directory, which configured none, is told nothing.

use std::sync::{Arc, Mutex};

use basis::{
    AllowAll, CollectingSink, ModelInfo, PreparedRun, Provider, Runtime, TurnOptions, async_trait,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
};

use crate::support::{CardServer, a2a_file, pinned, write};

const PROVIDER: &str = "teaching-provider";
const MODEL: &str = "teaching-model";

/// Answers every request with text, keeping what `spawn` was described as.
#[derive(Clone, Default)]
struct Recorder {
    spawn: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn taught(&self) -> Vec<String> {
        self.spawn.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for Recorder {
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

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let spawn = request
            .tools
            .iter()
            .find(|tool| tool.name == "spawn")
            .and_then(|tool| tool.description.clone())
            .unwrap_or_default();
        self.spawn.lock().unwrap().push(spawn);
        Ok(provider_event_stream_from_response(Response {
            id: "teaching".to_string(),
            model: MODEL.to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text("done")],
            stop_reason: None,
            usage: None,
        }))
    }
}

/// One more turn of `run`, answered by the recorder.
async fn turn(run: &mut PreparedRun) {
    run.send_with_options(
        "next",
        CollectingSink::default(),
        AllowAll,
        TurnOptions::default(),
    )
    .await
    .expect("the turn completes");
}

fn model() -> ModelInfo {
    ModelInfo::new(MODEL, PROVIDER).with_context_window(64_000)
}

#[tokio::test]
async fn a_running_session_is_taught_an_agent_added_after_it_began() {
    let server = CardServer::start();
    let recorder = Recorder::default();
    let runtime = Arc::new(
        Runtime::builder()
            .with_provider_instance(recorder.clone())
            .with_ephemeral_history()
            .build()
            .expect("builds"),
    );
    let dir = tempfile::tempdir().unwrap();
    let open = || {
        pinned(dir.path(), Arc::clone(&runtime))
            .with_resolved_model(model())
            .open()
    };
    let workspace = open().await.expect("opens with nothing configured");
    let sibling = open().await.expect("the same directory opens again");
    let mut run = workspace.prepare("first").expect("mints");
    let mut theirs = sibling.prepare("first").expect("mints");

    turn(&mut run).await;

    write(
        &dir.path().join(".basis/a2a.json"),
        &a2a_file(&[("nous", &server.base)]),
    );
    workspace.reload_a2a().await.expect("reloads");
    turn(&mut run).await;
    turn(&mut theirs).await;

    let taught = recorder.taught();
    assert_eq!(taught.len(), 3);
    assert!(
        !taught[0].contains("@<agent>"),
        "nothing configured, nothing taught: {}",
        taught[0]
    );
    assert!(
        taught[1].contains("@<agent>") && taught[1].contains("`nous`"),
        "the same session, one reload later: {}",
        taught[1]
    );
    assert!(
        taught[1].contains("Investigates questions about the codebase."),
        "in its card's words: {}",
        taught[1]
    );
    assert_eq!(
        taught[2], taught[0],
        "a sibling open of the directory configured nothing, and is taught nothing"
    );
}
