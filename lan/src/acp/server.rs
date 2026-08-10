//! The ACP agent: request handlers over a connection.
//!
//! # The one rule
//!
//! Handler closures run *inside* the dispatch loop and block it until they
//! return. A handler that awaits a client round trip — which
//! `session/prompt` does, every time the agent asks permission — must
//! [`spawn`](ConnectionTo::spawn) and return immediately, carrying its
//! `Responder` into the spawned task. Awaiting inline deadlocks: the client's
//! answer arrives on a loop that is still blocked waiting for it.
//!
//! `session/new` and `session/load` build a runtime, which touches the disk and
//! the provider's model list but never the client, so they answer inline.

use std::{path::PathBuf, sync::Arc};

use agent_client_protocol::{
    Agent, Client, ConnectionTo, Handled, Stdio,
    schema::v1::{
        AgentCapabilities, CancelNotification, ContentBlock, Error, InitializeRequest,
        InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
        NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, SessionId,
        SessionNotification, StopReason,
    },
};

use super::{
    approver::AcpApprover,
    session::{AcpSession, SessionRegistry},
    update::session_update,
};
use crate::{Event, PreparedRun, RunConfig, RunError, run::EventSink};

/// Where an ACP session's [`PreparedRun`] comes from.
///
/// The same seam as [`prepare_with_session`](crate::run::prepare_with_session),
/// at the protocol layer: a Rust host that already owns a mentra runtime —
/// custom tools, its own store, a provider lan does not know — can serve ACP
/// over it instead of letting lan build one. lan's own tests are the other
/// consumer, driving the whole server against a scripted runtime with no
/// network.
#[async_trait::async_trait]
pub trait SessionSource: Send + Sync + 'static {
    /// Opens a conversation in `cwd`, for `session/new`.
    async fn create(&self, cwd: PathBuf) -> Result<PreparedRun, RunError>;

    /// Picks up the conversation persisted under `agent_id`, for
    /// `session/load`. The default refuses, which is the honest answer for a
    /// source whose sessions do not outlive the process.
    async fn resume(&self, agent_id: &str, cwd: PathBuf) -> Result<PreparedRun, RunError> {
        let _ = (agent_id, cwd);
        Err(RunError::NoSuchSession)
    }
}

/// The default source: build a runtime per session from a [`RunConfig`].
struct ConfiguredSource {
    template: Option<RunConfig>,
}

impl ConfiguredSource {
    /// Builds the config for one session, in the client's working directory.
    fn config_for(&self, cwd: PathBuf) -> RunConfig {
        match &self.template {
            Some(template) => {
                let mut config = template.clone();
                config.workspace = cwd;
                config
            }
            None => RunConfig::new(cwd, ""),
        }
    }
}

#[async_trait::async_trait]
impl SessionSource for ConfiguredSource {
    async fn create(&self, cwd: PathBuf) -> Result<PreparedRun, RunError> {
        crate::run::prepare_without_prompt(self.config_for(cwd)).await
    }

    async fn resume(&self, agent_id: &str, cwd: PathBuf) -> Result<PreparedRun, RunError> {
        crate::run::resume(agent_id, self.config_for(cwd)).await
    }
}

/// How a served connection is configured.
///
/// The client supplies the workspace per session (`cwd` on `session/new`), so
/// what belongs here is only what the client cannot say: which model and
/// endpoint to use, and whether commands are granted.
#[derive(Clone)]
pub struct ServeConfig {
    source: Arc<dyn SessionSource>,
}

impl std::fmt::Debug for ServeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeConfig").finish_non_exhaustive()
    }
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ServeConfig {
    /// Serves sessions built from `template`, whose workspace each session
    /// replaces with the `cwd` its client sent.
    pub fn new(template: impl Into<Option<RunConfig>>) -> Self {
        Self {
            source: Arc::new(ConfiguredSource {
                template: template.into(),
            }),
        }
    }

    /// Serves sessions the caller supplies.
    pub fn with_source(source: impl SessionSource) -> Self {
        Self {
            source: Arc::new(source),
        }
    }
}

/// Serves ACP on stdin/stdout until the client disconnects.
///
/// This is what `lan` with no subcommand runs: the default mode, because
/// embedding is the primary case (ADR-0002, ADR-0003).
pub async fn serve_stdio(config: ServeConfig) -> Result<(), Error> {
    serve(config, Stdio::new()).await
}

/// Serves ACP over any transport, which is what makes the server testable
/// in-process — see `tests/acp.rs`, which drives it over `Channel::duplex()`.
pub async fn serve<T>(config: ServeConfig, transport: T) -> Result<(), Error>
where
    T: agent_client_protocol::ConnectTo<Agent> + 'static,
{
    let sessions = SessionRegistry::new();

    Agent
        .builder()
        .name("lan")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                responder.respond(initialize(&request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let config = config.clone();
                async move |request: NewSessionRequest, responder, _connection| {
                    responder.respond_with_result(new_session(&config, &sessions, request).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let config = config.clone();
                async move |request: LoadSessionRequest, responder, _connection| {
                    responder.respond_with_result(load_session(&config, &sessions, request).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                    let sessions = sessions.clone();
                    let spawned = connection.clone();

                    // Spawn, always. A turn asks the client for permission, and
                    // the answer arrives on this loop.
                    connection.spawn(async move {
                        let outcome = prompt(&sessions, &spawned, request).await;
                        responder.respond_with_result(outcome)
                    })?;

                    Ok(Handled::Yes)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = sessions.clone();
                async move |notification: CancelNotification, _connection| {
                    if let Some(session) = sessions.get(&notification.session_id) {
                        session.cancel();
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
}

/// What lan can do, as told to a client.
fn initialize(request: &InitializeRequest) -> InitializeResponse {
    InitializeResponse::new(request.protocol_version)
        .agent_capabilities(
            AgentCapabilities::new()
                // Sessions are mentra agents, and mentra persists them, so a
                // client can reconnect to one this process never created.
                .load_session(true)
                .prompt_capabilities(PromptCapabilities::new().embedded_context(true)),
        )
        .agent_info(agent_client_protocol::schema::v1::Implementation::new(
            "lan",
            env!("CARGO_PKG_VERSION"),
        ))
}

async fn new_session(
    config: &ServeConfig,
    sessions: &SessionRegistry,
    request: NewSessionRequest,
) -> Result<NewSessionResponse, Error> {
    let run = config
        .source
        .create(request.cwd)
        .await
        .map_err(setup_failed)?;

    let id = sessions.insert(AcpSession::new(run));

    Ok(NewSessionResponse::new(id))
}

async fn load_session(
    config: &ServeConfig,
    sessions: &SessionRegistry,
    request: LoadSessionRequest,
) -> Result<LoadSessionResponse, Error> {
    // Already open on this connection: nothing to load.
    if sessions.get(&request.session_id).is_some() {
        return Ok(LoadSessionResponse::new());
    }

    let run = config
        .source
        .resume(&request.session_id.0, request.cwd)
        .await
        .map_err(setup_failed)?;

    sessions.insert(AcpSession::new(run));

    Ok(LoadSessionResponse::new())
}

/// Runs one turn, streaming its events to the client as `session/update`.
///
/// Always called from a spawned task, never from the dispatch loop.
async fn prompt(
    sessions: &SessionRegistry,
    connection: &ConnectionTo<Client>,
    request: PromptRequest,
) -> Result<PromptResponse, Error> {
    let session = sessions
        .get(&request.session_id)
        .ok_or_else(|| Error::invalid_params().data("unknown session"))?;

    let text = prompt_text(&request.prompt);
    if text.trim().is_empty() {
        return Err(Error::invalid_params().data("prompt has no text content"));
    }

    let approver = AcpApprover::new(request.session_id.clone(), connection.clone());
    let sink = NotificationSink::new(request.session_id.clone(), connection.clone());

    // Held across the turn: one conversation runs one turn at a time, which is
    // what ACP's own model assumes. The cancellation token lives outside this
    // lock, so `session/cancel` can reach it while the turn holds it.
    let mut run = session.lock_turn().await;
    let options = session.begin_turn();
    let cancelled = options.cancel.clone();

    let report = run.send_with_options(text, sink, approver, options).await;
    session.end_turn();
    drop(run);

    // A cancelled turn fails inside mentra, so the token — not the error — is
    // what distinguishes "the client stopped it" from "it broke". ACP requires
    // `Cancelled` in that case.
    if cancelled.is_some_and(|token| token.is_cancelled()) {
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }

    match report {
        Ok(report) if report.succeeded() => Ok(PromptResponse::new(StopReason::EndTurn)),
        Ok(report) => Err(Error::internal_error().data(match report.outcome {
            crate::RunOutcome::Error { message } => message,
            crate::RunOutcome::Ok => "the turn failed".to_string(),
        })),
        Err(error) => Err(Error::internal_error().data(error.to_string())),
    }
}

/// The text of a prompt, concatenating its text blocks.
///
/// Resource links and embedded resources are named rather than inlined: lan
/// does not fetch on the client's behalf, and dropping them silently would
/// lose what the user attached.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            ContentBlock::ResourceLink(link) => Some(format!("[{}]({})", link.name, link.uri)),
            ContentBlock::Resource(resource) => match &resource.resource {
                agent_client_protocol::schema::v1::EmbeddedResourceResource::TextResourceContents(
                    contents,
                ) => Some(contents.text.clone()),
                _ => None,
            },
            _ => None,
        }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn setup_failed(error: crate::RunError) -> Error {
    Error::internal_error().data(error.to_string())
}

/// An [`EventSink`] that forwards to the client as `session/update`.
struct NotificationSink {
    session_id: SessionId,
    connection: ConnectionTo<Client>,
}

impl NotificationSink {
    fn new(session_id: SessionId, connection: ConnectionTo<Client>) -> Self {
        Self {
            session_id,
            connection,
        }
    }
}

impl EventSink for NotificationSink {
    fn emit(&mut self, event: Event) -> std::io::Result<()> {
        let Some(update) = session_update(&event) else {
            return Ok(());
        };

        // Fire-and-forget, so this is safe from any task. A send failure means
        // the client is gone; returning the error stops forwarding for the
        // rest of the turn rather than writing into a dead socket repeatedly.
        self.connection
            .send_notification(SessionNotification::new(self.session_id.clone(), update))
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        ProtocolVersion,
        v1::{ResourceLink, TextContent},
    };

    #[test]
    fn initialize_advertises_resumable_sessions() {
        let response = initialize(&InitializeRequest::new(ProtocolVersion::V1));

        assert!(
            response.agent_capabilities.load_session,
            "sessions are persisted mentra agents, so a client can reconnect"
        );
        assert_eq!(
            response.protocol_version,
            ProtocolVersion::V1,
            "the client's version is echoed, not overridden"
        );
    }

    #[test]
    fn prompt_text_joins_text_blocks() {
        let text = prompt_text(&[
            ContentBlock::Text(TextContent::new("first".to_string())),
            ContentBlock::Text(TextContent::new("second".to_string())),
        ]);

        assert_eq!(text, "first\nsecond");
    }

    #[test]
    fn a_resource_link_is_named_rather_than_dropped() {
        let text = prompt_text(&[ContentBlock::ResourceLink(ResourceLink::new(
            "notes.md".to_string(),
            "file:///repo/notes.md".to_string(),
        ))]);

        assert!(
            text.contains("notes.md") && text.contains("file:///repo/notes.md"),
            "what the user attached must survive into the prompt: {text}"
        );
    }

    #[test]
    fn an_empty_prompt_produces_no_text() {
        assert!(prompt_text(&[]).is_empty());
    }

    #[test]
    fn the_config_template_takes_the_clients_working_directory() {
        let source = ConfiguredSource {
            template: Some(
                RunConfig::new("/placeholder", "").with_shell(crate::ShellAccess::Granted),
            ),
        };

        let built = source.config_for(PathBuf::from("/repo"));

        assert_eq!(built.workspace, PathBuf::from("/repo"));
        assert_eq!(
            built.shell,
            crate::ShellAccess::Granted,
            "everything the client cannot say must carry through"
        );
    }
}
