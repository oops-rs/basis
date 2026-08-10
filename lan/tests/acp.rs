//! The ACP server, driven by a real ACP client.
//!
//! Both ends are the genuine article — lan's server and the
//! `agent-client-protocol` client — joined by `Channel::duplex()` instead of a
//! pipe. Every request is really serialized, dispatched, and answered; only the
//! transport and the model are substituted. No subprocess, no network, no cost.
//!
//! The permission test is the important one. ACP handler closures run inside
//! the dispatch loop and block it until they return, so a `session/prompt` that
//! awaited the client's permission answer inline would deadlock forever: the
//! answer arrives on the loop that is waiting for it. Every test here is
//! wrapped in a timeout, because that failure presents as a hang rather than an
//! assertion.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Responder,
    schema::{
        ProtocolVersion,
        v1::{
            ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
            RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
            SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason, TextContent,
        },
    },
};
use lan::{
    ApprovalPolicy, PreparedRun, RunConfig, RunError,
    acp::{ServeConfig, SessionSource},
    approval::PolicyAuthorizer,
    run::prepare_with_session,
};
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};

/// Every exchange here is local and scripted; exceeding this means something
/// is stuck, which is the failure these tests exist to catch.
const NOT_STUCK: Duration = Duration::from_secs(10);

/// Serves sessions over a scripted runtime, so the protocol is exercised
/// without a provider.
struct MockSource {
    mock: Arc<MockRuntime>,
    workspace: PathBuf,
}

#[async_trait::async_trait]
impl SessionSource for MockSource {
    async fn create(&self, _cwd: PathBuf) -> Result<PreparedRun, RunError> {
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

        // The workspace is the temp dir rather than the client's cwd: the
        // scripted runtime is what is under test, not path discovery.
        let config = RunConfig::new(&self.workspace, "").with_context(lan::ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        });

        prepare_with_session(session, &config, "openai", "mock-model")
    }
}

/// What a test client observed.
#[derive(Default)]
struct Observed {
    updates: Vec<SessionUpdate>,
    permission_requests: Vec<RequestPermissionRequest>,
}

impl Observed {
    /// Everything the agent said, concatenated.
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

/// Runs one client conversation against lan's server over an in-process pair.
///
/// `answer` decides how the client responds to a permission request; `None`
/// means the client is never expected to be asked.
async fn drive(
    source: MockSource,
    prompts: Vec<&str>,
    answer: Option<&'static str>,
) -> (Vec<StopReason>, Arc<Mutex<Observed>>) {
    let (client_side, agent_side) = Channel::duplex();
    let observed = Arc::new(Mutex::new(Observed::default()));

    let server = tokio::spawn(lan::acp::serve(
        ServeConfig::with_source(source),
        agent_side,
    ));

    let prompts: Vec<String> = prompts.into_iter().map(str::to_string).collect();
    let seen = Arc::clone(&observed);
    let recorded = Arc::clone(&observed);

    let stop_reasons = Client
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
        .on_receive_request(
            move |request: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  _connection| {
                let recorded = Arc::clone(&recorded);
                async move {
                    recorded
                        .lock()
                        .expect("not poisoned")
                        .permission_requests
                        .push(request.clone());

                    let outcome = match answer {
                        Some(option) => RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(option.to_string()),
                        ),
                        None => RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(RequestPermissionResponse::new(outcome))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_side, |connection: ConnectionTo<Agent>| async move {
            let initialized = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert!(
                initialized.agent_capabilities.load_session,
                "lan resumes sessions, and must say so"
            );

            let session = connection
                .send_request(NewSessionRequest::new(
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
                ))
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

            Ok(stop_reasons)
        });

    let stop_reasons = tokio::time::timeout(NOT_STUCK, stop_reasons)
        .await
        .expect("the conversation must not hang")
        .expect("the client drives cleanly");

    server.abort();

    (stop_reasons, observed)
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn text_mock(chunks: &[&str]) -> MockRuntime {
    MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .stream_text(chunks.to_vec())
        .build()
        .expect("mock runtime builds")
}

#[tokio::test]
async fn a_prompt_streams_back_and_ends_the_turn() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["Hel", "lo ", "world"]));

    let (stop_reasons, observed) = drive(
        MockSource {
            mock,
            workspace: workspace.path().to_path_buf(),
        },
        vec!["say hello"],
        None,
    )
    .await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);

    let observed = observed.lock().expect("not poisoned");
    assert_eq!(
        observed.agent_text(),
        "Hello world",
        "the client must receive the answer as message chunks"
    );
    assert!(
        observed.permission_requests.is_empty(),
        "nothing consequential happened, so nothing should have been asked"
    );
}

#[tokio::test]
async fn a_second_prompt_continues_the_same_session() {
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

    let (stop_reasons, observed) = drive(
        MockSource {
            mock: Arc::clone(&mock),
            workspace: workspace.path().to_path_buf(),
        },
        vec!["one", "two"],
        None,
    )
    .await;

    assert_eq!(
        stop_reasons,
        vec![StopReason::EndTurn, StopReason::EndTurn],
        "both turns must complete on one session"
    );
    assert_eq!(
        observed.lock().expect("not poisoned").agent_text(),
        "firstsecond"
    );

    // The refactor this protocol rides on: turn two carried turn one.
    let requests = mock.recorded_requests().await;
    let second = requests.get(1).expect("a second request was made");
    assert!(
        second
            .messages
            .iter()
            .any(|message| message.text().contains("one")),
        "the second turn must still carry the first"
    );
}

#[tokio::test]
async fn a_consequential_call_asks_the_client_and_does_not_deadlock() {
    let workspace = workspace();
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            // Not permissive: the authorizer must have something to prompt about.
            .with_policy(RuntimePolicy::workspace_bounded(workspace.path()))
            // Without an authorizer nothing is ever asked, so the permission
            // round trip under test would silently not happen.
            .with_tool_authorizer(PolicyAuthorizer::new(ApprovalPolicy::Prompt))
            .tool_calls(vec![MockToolCall::new(
                "files",
                serde_json::json!({
                    "operations": [{ "op": "create", "path": "made.txt", "content": "hi" }]
                }),
            )])
            .text("done")
            .build()
            .expect("mock runtime builds"),
    );

    let (stop_reasons, observed) = drive(
        MockSource {
            mock,
            workspace: workspace.path().to_path_buf(),
        },
        vec!["make a file"],
        Some("allow-once"),
    )
    .await;

    assert_eq!(stop_reasons, vec![StopReason::EndTurn]);

    let observed = observed.lock().expect("not poisoned");
    assert_eq!(
        observed.permission_requests.len(),
        1,
        "the write should have been put to the client"
    );
    assert!(
        observed.tool_calls() >= 1,
        "the call itself must also reach the client as a tool call"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "an approved write must actually happen"
    );
}

#[tokio::test]
async fn a_refused_call_does_not_happen() {
    let workspace = workspace();
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            .with_policy(RuntimePolicy::workspace_bounded(workspace.path()))
            .with_tool_authorizer(PolicyAuthorizer::new(ApprovalPolicy::Prompt))
            .tool_calls(vec![MockToolCall::new(
                "files",
                serde_json::json!({
                    "operations": [{ "op": "create", "path": "made.txt", "content": "hi" }]
                }),
            )])
            .text("could not")
            .build()
            .expect("mock runtime builds"),
    );

    let (stop_reasons, observed) = drive(
        MockSource {
            mock,
            workspace: workspace.path().to_path_buf(),
        },
        vec!["make a file"],
        Some("reject-once"),
    )
    .await;

    assert_eq!(
        stop_reasons,
        vec![StopReason::EndTurn],
        "a refusal is a normal turn, not a protocol failure"
    );
    assert_eq!(
        observed
            .lock()
            .expect("not poisoned")
            .permission_requests
            .len(),
        1
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a refused write must not reach the disk"
    );
}

#[tokio::test]
async fn an_unknown_session_is_rejected_rather_than_served() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (client_side, agent_side) = Channel::duplex();
    let server = tokio::spawn(lan::acp::serve(
        ServeConfig::with_source(MockSource {
            mock,
            workspace: workspace.path().to_path_buf(),
        }),
        agent_side,
    ));

    let result =
        Client
            .builder()
            .connect_with(client_side, |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                Ok(connection
                    .send_request(PromptRequest::new(
                        "no-such-session",
                        vec![ContentBlock::Text(TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await)
            });

    let inner = tokio::time::timeout(NOT_STUCK, result)
        .await
        .expect("must not hang")
        .expect("the client drives cleanly");

    assert!(
        inner.is_err(),
        "prompting a session that was never opened must be an error"
    );

    server.abort();
}
