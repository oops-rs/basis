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
//! answer arrives on the loop that is waiting for it. `session/load` is the
//! same hazard from the other side: it reads a transcript behind the lock a
//! running turn holds. Every test here is wrapped in a timeout, because both
//! failures present as a hang rather than an assertion.

use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Responder,
    schema::{
        ProtocolVersion,
        v1::{
            CloseSessionRequest, ContentBlock, InitializeRequest, ListSessionsRequest,
            LoadSessionRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
            RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
            SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate,
            SetSessionModeRequest, StopReason, TextContent,
        },
    },
};
use lan_acp::{ServeConfig, SessionSource};
use lan_core::{
    PreparedRun, RunConfig, RunError, approval::ApprovalGate, run::prepare_with_session,
};
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};

/// Every exchange here is local and scripted; exceeding this means something
/// is stuck, which is the failure these tests exist to catch.
const NOT_STUCK: Duration = Duration::from_secs(10);

/// The runtime identifier every mock here files its agents under. Each mock
/// still gets its own SQLite file, so sharing the name costs nothing and means
/// a test can ask for its own sessions back by a value it knows.
const MOCK_RUNTIME: &str = "lan-acp-tests";

/// Serves sessions over a scripted runtime, so the protocol is exercised
/// without a provider.
struct MockSource {
    mock: Arc<MockRuntime>,
    workspace: PathBuf,
}

impl MockSource {
    fn new(mock: &Arc<MockRuntime>, workspace: &tempfile::TempDir) -> Self {
        Self {
            mock: Arc::clone(mock),
            workspace: workspace.path().to_path_buf(),
        }
    }

    /// The workspace is the temp dir rather than the client's cwd: the
    /// scripted runtime is what is under test, not path discovery.
    fn config(&self) -> RunConfig {
        RunConfig::new(&self.workspace, "").with_context(lan_core::ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
    }
}

#[async_trait::async_trait]
impl SessionSource for MockSource {
    async fn create(
        &self,
        _cwd: PathBuf,
        _mcp: Vec<lan_core::McpServer>,
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

        prepare_with_session(session, &self.config(), "openai", "mock-model")
    }

    async fn resume(
        &self,
        agent_id: &str,
        _cwd: PathBuf,
        _mcp: Vec<lan_core::McpServer>,
    ) -> Result<PreparedRun, RunError> {
        // The mock persists to a real store, so this is the same resume a
        // second process would perform — which is what `session/load` is for.
        let session = self.mock.runtime().resume_session(agent_id)?;

        prepare_with_session(session, &self.config(), "openai", "mock-model")
    }

    fn lists_sessions(&self) -> bool {
        true
    }

    /// Enumerates the mock's own store.
    ///
    /// Scoped by the mock's runtime identifier rather than by `cwd`: what is
    /// under test here is the protocol — that a client asks, and gets back the
    /// conversations as `SessionInfo` — not lan's workspace-scoping scheme,
    /// which `store.rs` covers on its own.
    async fn list_sessions(
        &self,
        _cwd: PathBuf,
    ) -> Result<Vec<lan_core::PersistedSession>, RunError> {
        Ok(self
            .mock
            .runtime()
            .list_persisted_agents(MOCK_RUNTIME)?
            .into_iter()
            .filter(|agent| !agent.is_teammate)
            .map(|agent| lan_core::PersistedSession {
                agent_id: agent.id,
                name: agent.name,
                messages: agent.history_len,
            })
            .collect())
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

    /// What the *user* is shown as having said. Only a replay produces these:
    /// a live turn never echoes the prompt back.
    fn replayed_user_text(&self) -> String {
        self.updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::UserMessageChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn mode_changes(&self) -> Vec<String> {
        self.updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::CurrentModeUpdate(mode) => Some(mode.current_mode_id.0.to_string()),
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

/// Drives one client conversation against lan's server over an in-process
/// pair, running `body` once the connection is up.
///
/// `answer` decides how the client responds to a permission request; `None`
/// means the client is never expected to be asked.
async fn connected<F, Fut, T>(
    source: MockSource,
    answer: Option<&'static str>,
    body: F,
) -> (T, Arc<Mutex<Observed>>)
where
    F: FnOnce(ConnectionTo<Agent>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, agent_client_protocol::Error>> + Send,
    T: Send + 'static,
{
    let (client_side, agent_side) = Channel::duplex();
    let observed = Arc::new(Mutex::new(Observed::default()));

    let server = tokio::spawn(lan_acp::serve(ServeConfig::with_source(source), agent_side));

    let seen = Arc::clone(&observed);
    let recorded = Arc::clone(&observed);

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

            body(connection).await
        });

    let result = tokio::time::timeout(NOT_STUCK, driven)
        .await
        .expect("the conversation must not hang")
        .expect("the client drives cleanly");

    server.abort();

    (result, observed)
}

/// Opens a session and sends `prompts` on it, one turn at a time.
async fn drive(
    source: MockSource,
    prompts: Vec<&str>,
    answer: Option<&'static str>,
) -> (Vec<StopReason>, Arc<Mutex<Observed>>) {
    let prompts: Vec<String> = prompts.into_iter().map(str::to_string).collect();

    connected(source, answer, |connection| async move {
        let session = open(&connection).await?;

        let mut stop_reasons = Vec::new();
        for prompt in prompts {
            stop_reasons.push(say(&connection, &session, &prompt).await?);
        }

        Ok(stop_reasons)
    })
    .await
}

/// `session/new` in the current directory, returning the id lan minted.
async fn open(connection: &ConnectionTo<Agent>) -> Result<SessionId, agent_client_protocol::Error> {
    let response = connection
        .send_request(NewSessionRequest::new(
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        ))
        .block_task()
        .await?;

    Ok(response.session_id)
}

/// One turn, returning why it stopped.
async fn say(
    connection: &ConnectionTo<Agent>,
    session: &SessionId,
    prompt: &str,
) -> Result<StopReason, agent_client_protocol::Error> {
    let response = connection
        .send_request(PromptRequest::new(
            session.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        ))
        .block_task()
        .await?;

    Ok(response.stop_reason)
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn text_mock(chunks: &[&str]) -> MockRuntime {
    MockRuntime::builder()
        .model("mock-model", "openai")
        .runtime_identifier(MOCK_RUNTIME)
        .with_policy(RuntimePolicy::permissive())
        .stream_text(chunks.to_vec())
        .build()
        .expect("mock runtime builds")
}

/// A runtime that wants to write one file, and an authorizer that surfaces the
/// attempt. Without the authorizer nothing is ever asked, so the permission
/// path under test would silently not happen.
fn writing_mock(workspace: &tempfile::TempDir) -> MockRuntime {
    MockRuntime::builder()
        .model("mock-model", "openai")
        .runtime_identifier(MOCK_RUNTIME)
        // Not permissive: the authorizer must have something to prompt about.
        .with_policy(RuntimePolicy::workspace_bounded(workspace.path()))
        .with_tool_authorizer(ApprovalGate::new())
        .tool_calls(vec![MockToolCall::new(
            "files",
            serde_json::json!({
                "operations": [{ "op": "create", "path": "made.txt", "content": "hi" }]
            }),
        )])
        .text("done")
        .build()
        .expect("mock runtime builds")
}

#[tokio::test]
async fn a_prompt_streams_back_and_ends_the_turn() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["Hel", "lo ", "world"]));

    let (stop_reasons, observed) =
        drive(MockSource::new(&mock, &workspace), vec!["say hello"], None).await;

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
            .runtime_identifier(MOCK_RUNTIME)
            .with_policy(RuntimePolicy::permissive())
            .text("first")
            .text("second")
            .build()
            .expect("mock runtime builds"),
    );

    let (stop_reasons, observed) =
        drive(MockSource::new(&mock, &workspace), vec!["one", "two"], None).await;

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
    let mock = Arc::new(writing_mock(&workspace));

    let (stop_reasons, observed) = drive(
        MockSource::new(&mock, &workspace),
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
    let mock = Arc::new(writing_mock(&workspace));

    let (stop_reasons, observed) = drive(
        MockSource::new(&mock, &workspace),
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

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            Ok(connection
                .send_request(PromptRequest::new(
                    "no-such-session",
                    vec![ContentBlock::Text(TextContent::new("hello"))],
                ))
                .block_task()
                .await)
        },
    )
    .await;

    assert!(
        result.is_err(),
        "prompting a session that was never opened must be an error"
    );
}

#[tokio::test]
async fn a_new_session_offers_the_modes_it_can_switch_between() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (modes, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let response = connection
                .send_request(NewSessionRequest::new(PathBuf::from("/")))
                .block_task()
                .await?;

            Ok(response.modes)
        },
    )
    .await;

    let modes = modes.expect("a session reports the modes it has");
    assert_eq!(
        &*modes.current_mode_id.0, "prompt",
        "over ACP there is a client to ask"
    );

    let offered: Vec<String> = modes
        .available_modes
        .iter()
        .map(|mode| mode.id.0.to_string())
        .collect();
    assert_eq!(offered, vec!["always", "prompt", "never"]);
}

#[tokio::test]
async fn switching_to_always_stops_asking_and_says_so() {
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));

    // `None` means the client is not prepared to answer: if lan asked anyway,
    // the request would be cancelled and the write would not happen.
    let (stop_reason, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            connection
                .send_request(SetSessionModeRequest::new(session.clone(), "always"))
                .block_task()
                .await?;

            say(&connection, &session, "make a file").await
        },
    )
    .await;

    assert_eq!(stop_reason, StopReason::EndTurn);

    let observed = observed.lock().expect("not poisoned");
    assert!(
        observed.permission_requests.is_empty(),
        "the mode answered; the client should not have been asked"
    );
    assert_eq!(
        observed.mode_changes(),
        vec!["always"],
        "a mode change is session state, so every view of the session hears it"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "an allowed write must still happen"
    );
}

#[tokio::test]
async fn switching_to_read_only_refuses_without_asking() {
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));

    let (stop_reason, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            connection
                .send_request(SetSessionModeRequest::new(session.clone(), "never"))
                .block_task()
                .await?;

            say(&connection, &session, "make a file").await
        },
    )
    .await;

    assert_eq!(
        stop_reason,
        StopReason::EndTurn,
        "a refusal is a normal turn"
    );

    let observed = observed.lock().expect("not poisoned");
    assert!(
        observed.permission_requests.is_empty(),
        "read-only has nothing to ask about"
    );
    assert!(
        !workspace.path().join("made.txt").exists(),
        "a mode that refuses must actually refuse"
    );
}

#[tokio::test]
async fn a_mode_lan_never_offered_is_refused() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            Ok(connection
                .send_request(SetSessionModeRequest::new(session, "architect"))
                .block_task()
                .await)
        },
    )
    .await;

    assert!(
        result.is_err(),
        "a mode lan cannot act on must be an error, not a silent no-op"
    );
}

#[tokio::test]
async fn loading_a_session_replays_the_conversation_and_resuming_does_not() {
    let workspace = workspace();
    let mock = Arc::new(
        MockRuntime::builder()
            .model("mock-model", "openai")
            .runtime_identifier(MOCK_RUNTIME)
            .with_policy(RuntimePolicy::permissive())
            .text("41")
            .build()
            .expect("mock runtime builds"),
    );

    // One connection has the conversation and goes away, as a client that was
    // closed would.
    let (session_id, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;
            say(&connection, &session, "remember 41").await?;
            Ok(session)
        },
    )
    .await;

    // A second connection over the same store picks it up — the cross-process
    // case, minus the process.
    let loading = session_id.clone();
    let (modes, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let response = connection
                .send_request(LoadSessionRequest::new(loading, PathBuf::from("/")))
                .block_task()
                .await?;
            Ok(response.modes)
        },
    )
    .await;

    assert!(
        modes.is_some(),
        "a loaded session reports its mode like a new one"
    );

    // Copied out rather than asserted under the guard: a `std::sync::Mutex`
    // guard alive across an await is a deadlock waiting for a reason, and
    // clippy is right to refuse it even where this test happens not to await.
    let (replayed, agent_text, updates) = {
        let observed = observed.lock().expect("not poisoned");
        (
            observed.replayed_user_text(),
            observed.agent_text(),
            format!("{:?}", observed.updates),
        )
    };
    assert!(
        replayed.contains("remember 41"),
        "loading must replay what the user said: {updates}"
    );
    assert!(agent_text.contains("41"), "and what the agent answered");

    // Resuming is the same pickup without the replay, for a client that keeps
    // its own history.
    let (resumed, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let response = connection
                .send_request(ResumeSessionRequest::new(session_id, PathBuf::from("/")))
                .block_task()
                .await?;
            Ok(response.modes)
        },
    )
    .await;

    assert!(resumed.is_some());
    assert!(
        observed.lock().expect("not poisoned").updates.is_empty(),
        "resuming replays nothing, which is the whole difference from loading"
    );
}

#[tokio::test]
async fn a_closed_session_is_forgotten() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            connection
                .send_request(CloseSessionRequest::new(session.clone()))
                .block_task()
                .await?;

            Ok(say(&connection, &session, "still there?").await)
        },
    )
    .await;

    assert!(
        result.is_err(),
        "closing frees the session, so prompting it afterwards must fail"
    );
}

#[tokio::test]
async fn listing_reports_the_conversations_a_workspace_has() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["hello"]));
    let cwd = workspace.path().to_path_buf();

    let ((opened, listed), _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        move |connection| async move {
            let session = open(&connection).await?;
            say(&connection, &session, "one").await?;

            let response = connection
                .send_request(ListSessionsRequest::new().cwd(cwd))
                .block_task()
                .await?;

            Ok((session, response))
        },
    )
    .await;

    let found = listed
        .sessions
        .iter()
        .find(|info| info.session_id == opened)
        .expect("the conversation just had must be in its workspace's list");

    assert_eq!(
        found.cwd,
        workspace.path(),
        "a conversation is listed for the workspace it belongs to"
    );
    assert!(
        listed.next_cursor.is_none(),
        "one workspace's conversations arrive in one read, so there is no second page"
    );
}

#[tokio::test]
async fn listing_without_a_workspace_says_so_rather_than_guessing() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            Ok(connection
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await)
        },
    )
    .await;

    assert!(
        result.is_err(),
        "lan scopes conversations per workspace; answering with one workspace's \
         sessions as though they were all of them would be a lie"
    );
}

#[tokio::test]
async fn the_advertised_session_methods_are_the_ones_lan_answers() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (capabilities, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let initialized = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            Ok(initialized.agent_capabilities.session_capabilities)
        },
    )
    .await;

    // This mock can enumerate, so `list` is claimed here. The source that
    // cannot, and therefore must not claim it, is a unit test — building one
    // is cheaper than serving one.
    assert!(capabilities.list.is_some());
    assert!(capabilities.close.is_some());
    assert!(capabilities.resume.is_some());
    assert!(
        capabilities.delete.is_none(),
        "mentra's store cannot delete, so lan must not claim it can"
    );
}
