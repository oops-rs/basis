//! The end that drives: a real ACP client, and what it saw.
//!
//! The counterpart to `source`, holding both halves of what a test does over
//! the connection — `connected` stands a client up against basis's server, and
//! `open`, `say`, and `drive` are the requests every test sends; `Observed` is
//! everything that came back the other way, which is where the assertions
//! land. The timeout is here rather than in each test because it wraps the
//! whole conversation, not any one request.

use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Error, Responder,
    schema::{
        ProtocolVersion,
        v1::{
            ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, PromptResponse,
            RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
            SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionId,
            SessionNotification, SessionUpdate, StopReason, TextContent,
        },
    },
};
use basis_acp::ServeConfig;

use crate::source::MockSource;

/// Every exchange here is local and scripted; exceeding this means something
/// is stuck, which is the failure these tests exist to catch.
const NOT_STUCK: Duration = Duration::from_secs(10);

/// What a test client observed.
#[derive(Default)]
pub(crate) struct Observed {
    pub(crate) updates: Vec<SessionUpdate>,
    pub(crate) permission_requests: Vec<RequestPermissionRequest>,
    /// Permission requests a handler chose to leave unanswered, held here so
    /// the responder stays alive: a person who has not decided yet is a
    /// responder nobody has used, not one that was dropped.
    pub(crate) unanswered: Vec<Responder<RequestPermissionResponse>>,
}

/// What the client does when the agent asks permission. It is handed the
/// request, the responder and the connection, and may answer at once, hold
/// the answer, or do something else to the session first.
pub(crate) type OnPermission = Arc<
    dyn Fn(
            RequestPermissionRequest,
            Responder<RequestPermissionResponse>,
            ConnectionTo<Agent>,
            &Arc<Mutex<Observed>>,
        ) -> Result<(), Error>
        + Send
        + Sync,
>;

/// A client that answers every permission request with `answer`, or, given
/// `None`, with `Cancelled` — a client not prepared to be asked.
pub(crate) fn answering(answer: Option<&'static str>) -> OnPermission {
    Arc::new(move |_request, responder, _connection, _observed| {
        let outcome = match answer {
            Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.to_string(),
            )),
            None => RequestPermissionOutcome::Cancelled,
        };
        responder.respond(RequestPermissionResponse::new(outcome))
    })
}

impl Observed {
    /// Everything the agent said, concatenated.
    pub(crate) fn agent_text(&self) -> String {
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
    pub(crate) fn replayed_user_text(&self) -> String {
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

    /// Everything the agent said about itself rather than to the user, which
    /// is where a compaction, a retry and basis's own asides all land.
    pub(crate) fn thought_text(&self) -> String {
        self.updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentThoughtChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The names in the last `AvailableCommandsUpdate` the agent sent.
    pub(crate) fn command_names(&self) -> Vec<String> {
        self.updates
            .iter()
            .rev()
            .find_map(|update| match update {
                SessionUpdate::AvailableCommandsUpdate(commands) => Some(
                    commands
                        .available_commands
                        .iter()
                        .map(|command| command.name.clone())
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub(crate) fn mode_changes(&self) -> Vec<String> {
        self.updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::CurrentModeUpdate(mode) => Some(mode.current_mode_id.0.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Every `ConfigOptionUpdate` the agent broadcast, as `(option id, current
    /// value)` pairs — one vector per update, so a test can tell "the second
    /// change" from "both changes".
    pub(crate) fn config_updates(&self) -> Vec<Vec<(String, String)>> {
        self.updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::ConfigOptionUpdate(config) => Some(
                    config
                        .config_options
                        .iter()
                        .map(|option| (option.id.0.to_string(), current_value(option)))
                        .collect(),
                ),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn tool_calls(&self) -> usize {
        self.updates
            .iter()
            .filter(|update| matches!(update, SessionUpdate::ToolCall(_)))
            .count()
    }
}

/// The value a session config option is currently on.
///
/// Shared by the notification reader above and by the tests reading a
/// `session/new` or `session/set_config_option` response, so all three agree
/// on what "current" means.
pub(crate) fn current_value(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select(select) => select.current_value.0.to_string(),
        SessionConfigKind::Boolean(boolean) => boolean.current_value.to_string(),
        other => panic!("unhandled option kind: {other:?}"),
    }
}

/// Drives one client conversation against basis's server over an in-process
/// pair, running `body` once the connection is up.
///
/// `answer` decides how the client responds to a permission request; `None`
/// means the client is never expected to be asked.
pub(crate) async fn connected<F, Fut, T>(
    source: MockSource,
    answer: Option<&'static str>,
    body: F,
) -> (T, Arc<Mutex<Observed>>)
where
    F: FnOnce(ConnectionTo<Agent>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, agent_client_protocol::Error>> + Send,
    T: Send + 'static,
{
    connected_with(ServeConfig::with_source(source), answering(answer), body).await
}

/// [`connected`], for a test that needs to say more: which [`ServeConfig`]
/// the server runs on — two servers sharing one is how a second connection
/// to the same process is stood up — and what the client does when asked
/// permission.
pub(crate) async fn connected_with<F, Fut, T>(
    config: ServeConfig,
    on_permission: OnPermission,
    body: F,
) -> (T, Arc<Mutex<Observed>>)
where
    F: FnOnce(ConnectionTo<Agent>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, agent_client_protocol::Error>> + Send,
    T: Send + 'static,
{
    let (client_side, agent_side) = Channel::duplex();
    let observed = Arc::new(Mutex::new(Observed::default()));

    let server = tokio::spawn(basis_acp::serve(config, agent_side));

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
                  connection: ConnectionTo<Agent>| {
                let recorded = Arc::clone(&recorded);
                let on_permission = Arc::clone(&on_permission);
                async move {
                    recorded
                        .lock()
                        .expect("not poisoned")
                        .permission_requests
                        .push(request.clone());

                    on_permission(request, responder, connection, &recorded)
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
                "basis resumes sessions, and must say so"
            );

            body(connection).await
        });

    let result = tokio::time::timeout(NOT_STUCK, driven)
        .await
        .expect("the conversation must not hang")
        .expect("the client drives cleanly");

    // The client is gone, so the server must end — and it must have, before
    // the next thing a test does, because what it does on the way out (letting
    // go of the conversations this connection held) is what a second
    // connection depends on. Aborting it here would skip exactly that.
    tokio::time::timeout(NOT_STUCK, server)
        .await
        .expect("a server whose client went away must end")
        .expect("the server task does not panic")
        .expect("a client going away is the normal end of a connection, not an error");

    (result, observed)
}

/// Opens a session and sends `prompts` on it, one turn at a time.
pub(crate) async fn drive(
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

/// `session/new` in the current directory, returning the id basis minted.
pub(crate) async fn open(
    connection: &ConnectionTo<Agent>,
) -> Result<SessionId, agent_client_protocol::Error> {
    let response = connection
        .send_request(NewSessionRequest::new(
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        ))
        .block_task()
        .await?;

    Ok(response.session_id)
}

/// One turn, returning why it stopped.
pub(crate) async fn say(
    connection: &ConnectionTo<Agent>,
    session: &SessionId,
    prompt: &str,
) -> Result<StopReason, agent_client_protocol::Error> {
    let response = start_say(connection, session, prompt).block_task().await?;

    Ok(response.stop_reason)
}

/// Starts a prompt without waiting for its response. Keeping this handle is
/// what lets a duplex test send a notification while the request is still in
/// flight on the agent side.
pub(crate) fn start_say(
    connection: &ConnectionTo<Agent>,
    session: &SessionId,
    prompt: &str,
) -> agent_client_protocol::SentRequest<PromptResponse> {
    connection.send_request(PromptRequest::new(
        session.clone(),
        vec![ContentBlock::Text(TextContent::new(prompt))],
    ))
}
