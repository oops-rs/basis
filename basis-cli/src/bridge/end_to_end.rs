//! The bridge, driven by a real ACP client over a real websocket.
//!
//! Everything here is genuine except the model: basis's server, the
//! `agent-client-protocol` client, a TCP socket on loopback, and a real
//! WebSocket handshake between them. That is deliberate — the bridge exists to
//! serve a client basis does not ship, so the thing worth testing is that a
//! client which knows nothing about basis can drive it end to end.
//!
//! Loopback is not "the network" in the sense the project rules forbid: no
//! packet leaves the machine, no name is resolved, and the port is whichever
//! one the OS hands out. Every test is timed out, because the failure modes
//! being guarded against — a handshake that never completes, a dispatch loop
//! that blocks — present as a hang.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use agent_client_protocol::{
    Agent, Client, ConnectionTo,
    schema::{
        ProtocolVersion,
        v1::{
            ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
            SessionUpdate, StopReason, TextContent,
        },
    },
};
use basis::{PreparedRun, RunConfig, RunError, run::prepare_with_session};
use basis_acp::{ServeConfig, SessionSource};
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Error as WsError,
        client::IntoClientRequest,
        http::{HeaderValue, StatusCode, header},
    },
};

use super::{Bridge, BridgeConfig, websocket_transport};

/// Everything here is local and scripted; exceeding this means something is
/// stuck, which is what these tests exist to catch.
const NOT_STUCK: Duration = Duration::from_secs(10);

const A_PAGE: &str = "http://localhost:5173";

/// Serves sessions over a scripted runtime, so the transport is exercised
/// without a provider.
struct MockSource {
    mock: Arc<MockRuntime>,
    workspace: PathBuf,
}

#[async_trait::async_trait]
impl SessionSource for MockSource {
    async fn create(
        &self,
        _cwd: PathBuf,
        _mcp: Vec<basis::McpServer>,
    ) -> Result<PreparedRun, RunError> {
        let session = self
            .mock
            .runtime()
            .create_session_with_config(
                "test",
                self.mock.model(),
                mentra::agent::AgentConfig {
                    workspace: mentra::agent::WorkspaceConfig {
                        base_dir: self.workspace.clone(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .expect("session");

        let config = RunConfig::new(&self.workspace, "").with_context(basis::ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        });

        prepare_with_session(session, &config, "openai", "mock-model")
    }
}

fn text_mock(chunks: &[&str]) -> MockRuntime {
    MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .stream_text(chunks.to_vec())
        .build()
        .expect("mock runtime builds")
}

/// Starts a bridge on a port the OS picks, and answers with its address.
///
/// Port 0 rather than [`DEFAULT_PORT`](super::DEFAULT_PORT) so tests never collide
/// with each other or with a bridge someone is running.
async fn start(config: BridgeConfig, mock: Arc<MockRuntime>, workspace: PathBuf) -> SocketAddr {
    let bridge = Bridge::bind(config)
        .await
        .expect("a loopback port needs no opt-in");
    let address = bridge.local_addr().expect("the listener is bound");

    tokio::spawn(bridge.serve(ServeConfig::with_source(MockSource { mock, workspace })));

    address
}

fn loopback() -> BridgeConfig {
    BridgeConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)))
}

/// Opens a websocket, optionally claiming to be a page at `origin`.
async fn dial(
    address: SocketAddr,
    origin: Option<&str>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsError,
> {
    let mut request = format!("ws://{address}/")
        .into_client_request()
        .expect("a well-formed ws url");

    if let Some(origin) = origin {
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(origin).expect("a header value"),
        );
    }

    connect_async(request)
        .await
        .map(|(socket, _response)| socket)
}

/// What a client saw over one connection.
#[derive(Default)]
struct Observed {
    updates: Vec<SessionUpdate>,
}

impl Observed {
    fn agent_text(&self) -> String {
        self.updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn tool_calls(&self) -> usize {
        self.updates
            .iter()
            .filter(|update| matches!(update, SessionUpdate::ToolCall(_)))
            .count()
    }
}

/// Runs one whole conversation over an open socket: initialize, a session, and
/// every prompt in turn. Answers with the stop reasons and what streamed back.
async fn converse(
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    prompts: Vec<&str>,
) -> (String, Vec<StopReason>, Arc<std::sync::Mutex<Observed>>) {
    let observed = Arc::new(std::sync::Mutex::new(Observed::default()));
    let seen = Arc::clone(&observed);
    let prompts: Vec<String> = prompts.into_iter().map(str::to_string).collect();

    let driven = Client
        .builder()
        .on_receive_notification(
            move |notification: SessionNotification, _connection| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock()
                        .expect("not poisoned")
                        .updates
                        .push(notification.update);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            websocket_transport(socket),
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let session = connection
                    .send_request(NewSessionRequest::new(PathBuf::from(".")))
                    .block_task()
                    .await?;

                let mut stop_reasons = Vec::new();
                for prompt in prompts {
                    let response = connection
                        .send_request(PromptRequest::new(
                            session.session_id.clone(),
                            vec![ContentBlock::Text(TextContent::new(prompt))],
                        ))
                        .block_task()
                        .await?;
                    stop_reasons.push(response.stop_reason);
                }

                Ok((session.session_id.0.to_string(), stop_reasons))
            },
        );

    let (session_id, stop_reasons) = tokio::time::timeout(NOT_STUCK, driven)
        .await
        .expect("a conversation over loopback must not hang")
        .expect("the client drives cleanly");

    (session_id, stop_reasons, observed)
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[tokio::test]
async fn a_client_drives_a_whole_conversation_over_the_socket() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["Hel", "lo ", "bridge"]));
    let address = start(loopback(), mock, workspace.path().to_path_buf()).await;

    let socket = dial(address, None).await.expect("the handshake succeeds");
    let (_, stop_reasons, observed) = converse(socket, vec!["say hello"]).await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);
    assert_eq!(
        observed.lock().expect("not poisoned").agent_text(),
        "Hello bridge",
        "every chunk must arrive, in order, as its own frame"
    );
}

#[tokio::test]
async fn a_second_turn_over_one_socket_sees_the_first() {
    let workspace = workspace();
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            .with_policy(RuntimePolicy::permissive())
            .text("first")
            .text("second")
            .build()
            .expect("mock runtime builds"),
    );
    let address = start(
        loopback(),
        Arc::clone(&mock),
        workspace.path().to_path_buf(),
    )
    .await;

    let socket = dial(address, None).await.expect("the handshake succeeds");
    let (_, stop_reasons, _) = converse(socket, vec!["one", "two"]).await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn, StopReason::EndTurn]);

    // The conversation is a conversation, not two one-shots: the second
    // request carries the first exchange, exactly as it does over stdio.
    let requests = mock.recorded_requests().await;
    let second = requests.get(1).expect("the second turn was sent");
    assert!(
        second
            .messages
            .iter()
            .any(|message| message.text().contains("one")),
        "the second turn must carry the first"
    );
}

#[tokio::test]
async fn a_tool_round_survives_the_transport() {
    let workspace = workspace();
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            .with_policy(RuntimePolicy::permissive())
            .tool_calls(vec![MockToolCall::new(
                "files",
                serde_json::json!({"operations": [{"op": "list", "path": "."}]}),
            )])
            .text("listed them")
            .build()
            .expect("mock runtime builds"),
    );
    let address = start(loopback(), mock, workspace.path().to_path_buf()).await;

    let socket = dial(address, None).await.expect("the handshake succeeds");
    let (_, stop_reasons, observed) = converse(socket, vec!["list the files"]).await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);
    assert!(
        observed.lock().expect("not poisoned").tool_calls() > 0,
        "a tool call is a notification, and notifications must reach the client too"
    );
}

#[tokio::test]
async fn a_page_from_an_unnamed_origin_is_refused() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["never reached"]));
    let address = start(loopback(), mock, workspace.path().to_path_buf()).await;

    let refused = tokio::time::timeout(NOT_STUCK, dial(address, Some("https://evil.example")))
        .await
        .expect("a refusal must not hang")
        .expect_err("a page nobody named must not be served");

    // The point is that the *handshake* fails: nothing ACP-shaped is ever
    // exchanged with a page that was not allowed in.
    assert!(
        matches!(&refused, WsError::Http(response) if response.status() == StatusCode::FORBIDDEN),
        "the refusal must say why, in a status a page can see: {refused:?}"
    );
}

#[tokio::test]
async fn a_page_from_a_named_origin_is_served() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["hello page"]));
    let address = start(
        loopback().with_origins([A_PAGE.to_string()]),
        mock,
        workspace.path().to_path_buf(),
    )
    .await;

    let socket = dial(address, Some(A_PAGE))
        .await
        .expect("a named origin is admitted");
    let (_, stop_reasons, observed) = converse(socket, vec!["hello"]).await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);
    assert_eq!(
        observed.lock().expect("not poisoned").agent_text(),
        "hello page"
    );
}

#[tokio::test]
async fn a_named_origin_does_not_admit_its_neighbours() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["never reached"]));
    let address = start(
        loopback().with_origins([A_PAGE.to_string()]),
        mock,
        workspace.path().to_path_buf(),
    )
    .await;

    for near_miss in ["http://localhost:5174", "https://localhost:5173"] {
        let refused = tokio::time::timeout(NOT_STUCK, dial(address, Some(near_miss)))
            .await
            .expect("a refusal must not hang")
            .expect_err("only the origin that was named is served");

        assert!(
            matches!(&refused, WsError::Http(response) if response.status() == StatusCode::FORBIDDEN),
            "{near_miss} is a different origin: {refused:?}"
        );
    }
}

#[tokio::test]
async fn a_native_client_is_served_even_when_no_page_is() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["hello native"]));
    // No allowlist at all — the default. A client sending no `Origin` is not a
    // browser, and refusing it would protect nothing it could not already do.
    let address = start(loopback(), mock, workspace.path().to_path_buf()).await;

    let socket = dial(address, None)
        .await
        .expect("a client with no origin is not a page");
    let (_, stop_reasons, _) = converse(socket, vec!["hello"]).await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);
}

#[tokio::test]
async fn two_connections_are_two_conversations() {
    let workspace = workspace();
    // One scripted turn per client. `text_mock` streams its chunks as a single
    // turn, so two conversations need two `.text(..)` replies, not two chunks.
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            .with_policy(RuntimePolicy::permissive())
            .text("hello")
            .text("hello again")
            .build()
            .expect("mock runtime builds"),
    );
    let address = start(loopback(), mock, workspace.path().to_path_buf()).await;

    let first = dial(address, None)
        .await
        .expect("the first client connects");
    let (first_session, _, _) = converse(first, vec!["hello"]).await;

    let second = dial(address, None)
        .await
        .expect("a second client connects too");
    let (second_session, _, _) = converse(second, vec!["hello"]).await;

    assert_ne!(
        first_session, second_session,
        "one connection is one conversation, exactly as one stdio process is"
    );
}

#[tokio::test]
async fn a_refused_page_does_not_take_the_bridge_down() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["still here"]));
    let address = start(loopback(), mock, workspace.path().to_path_buf()).await;

    let _ = tokio::time::timeout(NOT_STUCK, dial(address, Some("https://evil.example")))
        .await
        .expect("a refusal must not hang")
        .expect_err("the page is refused");

    let socket = dial(address, None)
        .await
        .expect("the bridge is still accepting");
    let (_, stop_reasons, observed) = converse(socket, vec!["hello"]).await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);
    assert_eq!(
        observed.lock().expect("not poisoned").agent_text(),
        "still here",
        "one bad client must not end every other conversation"
    );
}
