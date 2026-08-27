//! A customized provider-core instance through basis's supported runtime seam.
//!
//! This test deliberately uses only `basis` exports. Besides proving that a
//! customized Responses definition reaches a prepared run, it keeps the
//! caller's clone and proves that clone shares the registered provider's
//! session state. That is the property a host needs to prewarm the connection
//! the real run will use instead of an unrelated session.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    io::{self, ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use basis::{
    CollectingSink, ContextConfig, MemoryConfig, ModelInfo, ModelSelector, RunOutcome, Runtime,
    ToolRoster, Workspace, WorkspaceBuilder, hooks::HooksConfig, provider_core,
    skills::SkillsConfig, templates::TemplatesConfig, tools::declared::ToolsConfig,
};
#[cfg(feature = "responses-websocket")]
use futures::{SinkExt, StreamExt};
#[cfg(feature = "responses-websocket")]
use tokio_tungstenite::{accept_async, tungstenite::Message as WebSocketMessage};

fn pinned(workspace: &Path, runtime: Arc<Runtime>) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_runtime(runtime)
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
        })
        .with_memory(MemoryConfig::disabled())
}

#[tokio::test]
async fn a_retained_clone_shares_the_registered_responses_session() {
    let endpoint = ScriptedEndpoint::start();
    let provider_id = provider_core::ProviderId::new("custom-responses");
    let mut definition = provider_core::responses::openai_definition();
    definition.descriptor.id = provider_id;
    definition.base_url = Some(endpoint.base_url.clone());
    let provider = provider_core::responses::ResponsesProvider::new(
        definition,
        provider_core::StaticCredentialSource::new("test-key"),
    );

    let runtime = Arc::new(
        Runtime::builder()
            .with_registered_provider(provider.clone())
            .with_ephemeral_history()
            .build()
            .expect("a registered provider builds without basis resolving another one"),
    );
    assert_eq!(runtime.provider(), "custom-responses");

    let request = provider_core::Request {
        model: Cow::Borrowed("test-model"),
        system: None,
        messages: Cow::Owned(vec![provider_core::Message::user(
            provider_core::ContentBlock::text("prewarm the retained session"),
        )]),
        tools: Cow::Owned(Vec::new()),
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        metadata: Cow::Owned(BTreeMap::new()),
        provider_request_options: Default::default(),
    };
    let mut stream = provider
        .session()
        .stream_response(request)
        .await
        .expect("the retained session reaches the scripted endpoint");
    while let Some(event) = stream.recv().await {
        event.expect("the retained session decodes its response");
    }
    assert_eq!(
        provider.session().latest_response_id().as_deref(),
        Some("resp_1")
    );

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules").expect("write context");
    let workspace = pinned(dir.path(), runtime).open().await.expect("opens");
    let report = workspace
        .prepare("use the registered provider")
        .expect("mints")
        .execute(CollectingSink::default())
        .await
        .expect("the prepared run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(report.provider, "custom-responses");
    assert_eq!(report.final_message.as_deref(), Some("reply-2"));
    assert_eq!(
        provider.session().latest_response_id().as_deref(),
        Some("resp_2"),
        "the retained clone must observe state written by the actual run"
    );

    let post_payloads = endpoint
        .requests()
        .into_iter()
        .filter(|request| request.starts_with("POST "))
        .map(|request| {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            serde_json::from_str::<serde_json::Value>(body).expect("request body is JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(post_payloads.len(), 2);
    assert!(post_payloads[0].get("previous_response_id").is_none());
    assert_eq!(post_payloads[1]["previous_response_id"], "resp_1");
}

type TestResponsesProvider =
    provider_core::responses::ResponsesProvider<provider_core::StaticCredentialSource>;

async fn send_direct(provider: &TestResponsesProvider, message: &str) {
    let request = provider_core::Request {
        model: Cow::Borrowed("test-model"),
        system: None,
        messages: Cow::Owned(vec![provider_core::Message::user(
            provider_core::ContentBlock::text(message.to_string()),
        )]),
        tools: Cow::Owned(Vec::new()),
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        metadata: Cow::Owned(BTreeMap::new()),
        provider_request_options: Default::default(),
    };
    let mut stream = provider
        .session()
        .stream_response(request)
        .await
        .expect("the retained seed reaches the scripted endpoint");
    while let Some(event) = stream.recv().await {
        event.expect("the retained seed decodes its response");
    }
}

async fn assert_rebuilt_responses_scopes_do_not_chain(first: &str, second: &str) {
    let endpoint = ScriptedEndpoint::start();
    let mut definition = provider_core::responses::openai_definition();
    definition.descriptor.id = provider_core::ProviderId::new("custom-responses");
    definition.base_url = Some(endpoint.base_url.clone());
    let seed = provider_core::responses::ResponsesProvider::new(
        definition,
        provider_core::StaticCredentialSource::new("test-key"),
    );

    // Deliberately dirty the never-registered seed. A recipe that used an
    // ordinary clone would now leak `resp_1` into generation A.
    send_direct(&seed, "seed state that must not enter a generation").await;
    assert_eq!(
        seed.session().latest_response_id().as_deref(),
        Some("resp_1")
    );

    let generated = Arc::new(Mutex::new(Vec::<TestResponsesProvider>::new()));
    let seed_for_factory = seed.clone();
    let generated_for_warm = Arc::clone(&generated);
    let recipe = Runtime::builder()
        .with_reusable_registered_provider(
            "custom-responses",
            move || Ok::<_, io::Error>(seed_for_factory.fresh_session_scope()),
            move |provider: TestResponsesProvider| {
                generated_for_warm
                    .lock()
                    .expect("generated scopes")
                    .push(provider);
                async { Ok::<_, io::Error>(()) }
            },
        )
        .with_ephemeral_history()
        .into_reusable_recipe()
        .expect("fresh Responses scopes form a runtime recipe");

    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = Workspace::builder(dir.path())
        .with_runtime_recipe(recipe)
        .without_discovery()
        .fresh_only()
        .with_resolved_model(ModelInfo::new("test-model", "custom-responses"))
        .with_tool_roster(ToolRoster::only(std::iter::empty::<String>()))
        .open()
        .await
        .expect("generation A opens")
        .bind_host_tools(Vec::new())
        .expect("generation A binds as tool-free");
    let report = workspace
        .prepare(first)
        .expect("generation A mints")
        .execute(CollectingSink::default())
        .await
        .expect("generation A completes");
    assert!(matches!(report.outcome, RunOutcome::Ok));

    let workspace = workspace
        .rebuild_for_reuse()
        .await
        .expect("generation B rebuilds")
        .bind_host_tools(Vec::new())
        .expect("generation B binds as tool-free");
    let report = workspace
        .prepare(second)
        .expect("generation B mints")
        .execute(CollectingSink::default())
        .await
        .expect("generation B completes");
    assert!(matches!(report.outcome, RunOutcome::Ok));

    let generated = generated.lock().expect("generated scopes");
    assert_eq!(generated.len(), 2);
    assert_eq!(
        generated[0].session().latest_response_id().as_deref(),
        Some("resp_2")
    );
    assert_eq!(
        generated[1].session().latest_response_id().as_deref(),
        Some("resp_3")
    );
    assert_eq!(
        seed.session().latest_response_id().as_deref(),
        Some("resp_1"),
        "the never-registered seed remains isolated from every generation"
    );

    let payloads = endpoint
        .requests()
        .into_iter()
        .filter(|request| request.starts_with("POST "))
        .map(|request| {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            serde_json::from_str::<serde_json::Value>(body).expect("request body is JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 3);
    assert!(
        payloads
            .iter()
            .all(|payload| payload.get("previous_response_id").is_none()),
        "seed, generation A, and generation B must each begin a fresh chain"
    );
    assert!(payloads[1].to_string().contains(first));
    assert!(payloads[2].to_string().contains(second));
}

#[tokio::test]
async fn reusable_responses_isolates_a_then_b() {
    assert_rebuilt_responses_scopes_do_not_chain("contradiction A", "contradiction B").await;
}

#[tokio::test]
async fn reusable_responses_isolates_b_then_a() {
    assert_rebuilt_responses_scopes_do_not_chain("contradiction B", "contradiction A").await;
}

#[cfg(feature = "responses-websocket")]
#[tokio::test]
async fn reusable_websocket_prewarm_targets_each_installed_scope() {
    use basis::runtime::ResponsesTransport;

    let endpoint = WebSocketEndpoint::start().await;
    let mut definition = provider_core::responses::openai_definition();
    definition.descriptor.id = provider_core::ProviderId::new("custom-responses");
    definition.base_url = Some(endpoint.base_url.clone());
    let seed = provider_core::responses::ResponsesProvider::new(
        definition,
        provider_core::StaticCredentialSource::new("test-key"),
    );
    let sibling = seed.fresh_session_scope();
    let factory_seed = seed.clone();

    let recipe = Runtime::builder()
        .with_responses_transport(ResponsesTransport::WebSocket)
        .with_reusable_registered_provider(
            "custom-responses",
            move || Ok::<_, io::Error>(factory_seed.fresh_session_scope()),
            move |provider: TestResponsesProvider| async move {
                provider
                    .session()
                    .connect_websocket(Default::default(), Default::default(), None, None)
                    .await
            },
        )
        .with_ephemeral_history()
        .into_reusable_recipe()
        .expect("websocket provider forms a runtime recipe");

    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = Workspace::builder(dir.path())
        .with_runtime_recipe(recipe)
        .without_discovery()
        .fresh_only()
        .with_resolved_model(ModelInfo::new("test-model", "custom-responses"))
        .with_tool_roster(ToolRoster::only(std::iter::empty::<String>()))
        .open()
        .await
        .expect("generation A opens and prewarms")
        .bind_host_tools(Vec::new())
        .expect("generation A binds as tool-free");
    assert_eq!(endpoint.accepted(), 1);
    assert!(seed.session().websocket_connection_is_closed().await);
    assert!(sibling.session().websocket_connection_is_closed().await);

    workspace
        .prepare("tenant-a")
        .expect("generation A mints")
        .execute(CollectingSink::default())
        .await
        .expect("generation A reuses its prewarmed websocket");
    assert_eq!(endpoint.accepted(), 1);

    let workspace = workspace
        .rebuild_for_reuse()
        .await
        .expect("generation B rebuilds and prewarms")
        .bind_host_tools(Vec::new())
        .expect("generation B binds as tool-free");
    assert_eq!(endpoint.accepted(), 2);
    assert!(seed.session().websocket_connection_is_closed().await);
    assert!(sibling.session().websocket_connection_is_closed().await);

    workspace
        .prepare("tenant-b")
        .expect("generation B mints")
        .execute(CollectingSink::default())
        .await
        .expect("generation B reuses its prewarmed websocket");
    assert_eq!(endpoint.accepted(), 2);

    let frames = endpoint.frames();
    assert_eq!(frames.len(), 2);
    assert!(frames[0].to_string().contains("tenant-a"));
    assert!(frames[1].to_string().contains("tenant-b"));
}

#[test]
fn a_customized_anthropic_definition_uses_the_registered_provider_seam() {
    let mut definition = provider_core::anthropic::definition();
    definition.descriptor.id = provider_core::ProviderId::new("custom-anthropic");
    definition.base_url = Some("http://127.0.0.1:1/".to_string());
    let provider =
        provider_core::anthropic::AnthropicProvider::with_definition_and_credential_source(
            definition,
            provider_core::StaticCredentialSource::new("test-key"),
        );

    let runtime = Runtime::builder()
        .with_registered_provider(provider)
        .with_ephemeral_history()
        .build()
        .expect("a customized Anthropic provider builds without network access");

    assert_eq!(runtime.provider(), "custom-anthropic");
}

struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        listener
            .set_nonblocking(true)
            .expect("make test endpoint nonblocking");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let recorded = Arc::clone(&requests);
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut turns = 0_usize;
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("accepted request stream should be blocking");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("accepted request stream should have a read timeout");
                        let request = read_http_request(&mut stream);
                        recorded.lock().expect("requests").push(request.clone());
                        if request.starts_with("GET ") {
                            answer_models(&mut stream);
                        } else {
                            turns += 1;
                            answer_turn(&mut stream, turns);
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept test request: {error}"),
                }
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

impl Drop for ScriptedEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("test endpoint stops");
        }
    }
}

#[cfg(feature = "responses-websocket")]
struct WebSocketEndpoint {
    base_url: String,
    accepted: Arc<std::sync::atomic::AtomicUsize>,
    frames: Arc<Mutex<Vec<serde_json::Value>>>,
    acceptor: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "responses-websocket")]
impl WebSocketEndpoint {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket endpoint");
        let address = listener.local_addr().expect("read websocket address");
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let accepted_for_task = Arc::clone(&accepted);
        let frames_for_task = Arc::clone(&frames);
        let acceptor = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let index = accepted_for_task.fetch_add(1, Ordering::SeqCst) + 1;
                let frames = Arc::clone(&frames_for_task);
                tokio::spawn(serve_websocket(stream, index, frames));
            }
        });

        Self {
            base_url: format!("http://{address}/v1"),
            accepted,
            frames,
            acceptor,
        }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn frames(&self) -> Vec<serde_json::Value> {
        self.frames.lock().expect("websocket frames").clone()
    }
}

#[cfg(feature = "responses-websocket")]
impl Drop for WebSocketEndpoint {
    fn drop(&mut self) {
        self.acceptor.abort();
    }
}

#[cfg(feature = "responses-websocket")]
async fn serve_websocket(
    stream: tokio::net::TcpStream,
    index: usize,
    frames: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let mut websocket = accept_async(stream).await.expect("upgrade websocket");
    while let Some(message) = websocket.next().await {
        let message = message.expect("read websocket frame");
        let WebSocketMessage::Text(text) = message else {
            continue;
        };
        let frame: serde_json::Value =
            serde_json::from_str(&text).expect("response.create frame is JSON");
        if frame["type"] != "response.create" {
            continue;
        }
        frames.lock().expect("websocket frames").push(frame);

        for event in [
            serde_json::json!({
                "type": "response.created",
                "response": {
                    "id": format!("resp_ws_{index}"),
                    "model": "test-model",
                    "status": "in_progress"
                }
            }),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": { "type": "message", "content": [] }
            }),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "message",
                    "content": [{
                        "type": "output_text",
                        "text": format!("reply-{index}")
                    }]
                }
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": format!("resp_ws_{index}"),
                    "model": "test-model",
                    "status": "completed"
                }
            }),
        ] {
            websocket
                .send(WebSocketMessage::Text(event.to_string().into()))
                .await
                .expect("send websocket response event");
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let read = stream.read(&mut buffer).expect("read request");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if header_end.is_none()
            && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = index + 4;
            header_end = Some(end);
            let headers = String::from_utf8_lossy(&bytes[..end]);
            content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or_default();
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            break;
        }
    }

    String::from_utf8(bytes).expect("request should be utf8")
}

fn answer_models(stream: &mut TcpStream) {
    let body = r#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#;
    answer(stream, "application/json", body);
}

fn answer_turn(stream: &mut TcpStream, index: usize) {
    let body = [
        format!(
            r#"{{"type":"response.created","response":{{"id":"resp_{index}","model":"test-model","status":"in_progress"}}}}"#
        ),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","content":[]}}"#.to_string(),
        format!(
            r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"message","content":[{{"type":"output_text","text":"reply-{index}"}}]}}}}"#
        ),
        format!(
            r#"{{"type":"response.completed","response":{{"id":"resp_{index}","model":"test-model","status":"completed"}}}}"#
        ),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>();
    answer(stream, "text/event-stream", &body);
}

fn answer(stream: &mut TcpStream, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("write response");
}
