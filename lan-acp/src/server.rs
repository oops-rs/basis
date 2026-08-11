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
//! `session/load` and `session/resume` spawn for the second half of the same
//! rule: they take a session's turn lock to read its transcript, and a turn
//! holds that lock while it waits for the client to answer. Taking it from the
//! loop would be the same deadlock wearing a different hat.
//!
//! `initialize`, `session/new`, `session/set_mode` and `session/close` answer
//! inline. They touch the disk, the provider's model list, and a sync mutex,
//! but never the client and never a lock a turn can be holding.
//!
//! # What is not registered, and why
//!
//! An unregistered method answers `-32601`, which is an honest "lan cannot do
//! that". Two are left that way deliberately:
//!
//! - **`authenticate`** — lan reads its credential from the environment
//!   (`ANTHROPIC_API_KEY` and the rest, see [`provider`](lan_core::provider)).
//!   There is no login to perform, no token to exchange, and so no auth method
//!   to advertise. A session opened without a credential fails with ACP's
//!   `auth_required` instead, naming the variable to set — which is the part a
//!   client can actually act on.
//! - **`session/delete`** — mentra's `AgentStore` has no delete. lan could
//!   forget a conversation in memory, but `session/delete` removes it from
//!   `session/list`, and the next call would list it again from the store. A
//!   deletion that does not delete is the one answer worse than `-32601`.
//!
//! `session/list` *is* registered, but only when the
//! [`SessionSource`] can actually enumerate — see
//! [`lists_sessions`](SessionSource::lists_sessions).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use agent_client_protocol::{
    Agent, Client, ConnectionTo, Handled,
    schema::v1::{
        AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, CancelNotification,
        CloseSessionRequest, CloseSessionResponse, ContentBlock, CurrentModeUpdate, Error,
        InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
        LoadSessionRequest, LoadSessionResponse, McpCapabilities, NewSessionRequest,
        NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
        ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
        SessionId, SessionInfo, SessionListCapabilities, SessionModeState, SessionNotification,
        SessionResumeCapabilities, SessionUpdate, SetSessionModeRequest, SetSessionModeResponse,
        StopReason,
    },
};

use crate::{
    approver::AcpApprover,
    history,
    mode::{ApprovalMode, ModeError, ModedApprover},
    session::{AcpSession, SessionRegistry},
    update::session_update,
};
use lan_core::{
    Event, McpServer, PersistedSession, PreparedRun, RunConfig, RunError, provider::ProviderError,
    run::EventSink,
};

/// Where an ACP session's [`PreparedRun`] comes from.
///
/// The same seam as [`prepare_with_session`](lan_core::run::prepare_with_session),
/// at the protocol layer: a Rust host that already owns a mentra runtime —
/// custom tools, its own store, a provider lan does not know — can serve ACP
/// over it instead of letting lan build one. lan's own tests are the other
/// consumer, driving the whole server against a scripted runtime with no
/// network.
///
/// A source that builds its own runtime owns its tool authorizer too, and a
/// session mode only reaches calls that authorizer surfaces: install
/// [`ApprovalGate`](lan_core::approval::ApprovalGate) — which is what lan's own
/// source gets from [`prepare_without_prompt`](lan_core::run::prepare_without_prompt)
/// — or the client's mode picker will have nothing to decide.
#[async_trait::async_trait]
pub trait SessionSource: Send + Sync + 'static {
    /// Opens a conversation in `cwd`, for `session/new`, with the MCP servers
    /// the client configured for this session.
    async fn create(&self, cwd: PathBuf, mcp: Vec<McpServer>) -> Result<PreparedRun, RunError>;

    /// Picks up the conversation persisted under `agent_id`, for
    /// `session/load`. The default refuses, which is the honest answer for a
    /// source whose sessions do not outlive the process.
    async fn resume(
        &self,
        agent_id: &str,
        cwd: PathBuf,
        mcp: Vec<McpServer>,
    ) -> Result<PreparedRun, RunError> {
        let _ = (agent_id, cwd, mcp);
        Err(RunError::NoSuchSession)
    }

    /// Whether this source can enumerate the conversations it has persisted.
    ///
    /// `session/list` is advertised and answered only when this is true. A
    /// source that keeps no registry would otherwise report "no sessions" for
    /// a workspace that has some, and a capability that answers wrongly is
    /// worse than one that was never claimed — an unregistered method at least
    /// says so, with `-32601`.
    fn lists_sessions(&self) -> bool {
        false
    }

    /// Every conversation persisted for `cwd`, oldest first.
    ///
    /// Only called when [`lists_sessions`](Self::lists_sessions) is true, so
    /// the default is unreachable rather than a claim about anything.
    async fn list_sessions(&self, cwd: PathBuf) -> Result<Vec<PersistedSession>, RunError> {
        let _ = cwd;
        Ok(Vec::new())
    }
}

/// The default source: build a runtime per session from a [`RunConfig`].
struct ConfiguredSource {
    template: Option<RunConfig>,
}

impl ConfiguredSource {
    /// Builds the config for one session, in the client's working directory.
    ///
    /// Nothing here says anything about approval. A runtime's authorizer is
    /// fixed for its life, so lan-core installs one that surfaces every
    /// consequential call and answers none of them; which of those the client
    /// actually sees is the session's mode, which can still change (see
    /// [`mode`](crate::mode)).
    fn config_for(&self, cwd: PathBuf, mcp: Vec<McpServer>) -> RunConfig {
        let config = match &self.template {
            Some(template) => {
                let mut config = template.clone();
                config.workspace = cwd;
                config
            }
            None => RunConfig::new(cwd, ""),
        };

        // The client's servers outrank the workspace's own: it is answering
        // for this session in particular. Discovery still runs, so a
        // `.mcp.json` the client said nothing about is still honored.
        let mcp = config.mcp.clone().with_supplied(mcp);
        config.with_mcp(mcp)
    }
}

#[async_trait::async_trait]
impl SessionSource for ConfiguredSource {
    async fn create(&self, cwd: PathBuf, mcp: Vec<McpServer>) -> Result<PreparedRun, RunError> {
        lan_core::run::prepare_without_prompt(self.config_for(cwd, mcp)).await
    }

    async fn resume(
        &self,
        agent_id: &str,
        cwd: PathBuf,
        mcp: Vec<McpServer>,
    ) -> Result<PreparedRun, RunError> {
        lan_core::run::resume(agent_id, self.config_for(cwd, mcp)).await
    }

    fn lists_sessions(&self) -> bool {
        true
    }

    /// Reads mentra's store directly. Building a session to enumerate sessions
    /// would resolve a model over the network to answer a question about a
    /// SQLite table.
    ///
    /// This depends on `run::resolve` tagging each conversation with
    /// [`store::runtime_identifier`](lan_core::store::runtime_identifier) for its
    /// workspace. Without that, conversations are written under mentra's
    /// `"default"` tag and no workspace's list will find them.
    async fn list_sessions(&self, cwd: PathBuf) -> Result<Vec<PersistedSession>, RunError> {
        lan_core::store::list(&cwd)
    }
}

/// How a served connection is configured.
///
/// The client supplies the workspace per session (`cwd` on `session/new`), so
/// what belongs here is only what the client cannot say: which model and
/// endpoint to use, whether commands are granted, and which permission mode
/// each session opens in.
#[derive(Clone)]
pub struct ServeConfig {
    source: Arc<dyn SessionSource>,
    /// Where a new session's mode picker starts.
    initial_mode: ApprovalMode,
}

impl std::fmt::Debug for ServeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeConfig")
            .field("initial_mode", &self.initial_mode)
            .finish_non_exhaustive()
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
    ///
    /// Sessions open at [`ApprovalMode::Prompt`] rather than at lan's library
    /// default of allowing everything: over ACP there is a client to ask, which
    /// is the whole reason the protocol carries a permission request. An
    /// operator who wants otherwise says so with
    /// [`with_initial_mode`](Self::with_initial_mode) — the template cannot
    /// carry it, because a [`RunConfig`] no longer has an opinion about
    /// approval to carry (ADR-0010).
    pub fn new(template: impl Into<Option<RunConfig>>) -> Self {
        Self {
            source: Arc::new(ConfiguredSource {
                template: template.into(),
            }),
            initial_mode: ApprovalMode::default(),
        }
    }

    /// Serves sessions the caller supplies.
    pub fn with_source(source: impl SessionSource) -> Self {
        Self {
            source: Arc::new(source),
            initial_mode: ApprovalMode::default(),
        }
    }

    /// Opens each session in `mode` instead of asking every time.
    pub fn with_initial_mode(self, mode: ApprovalMode) -> Self {
        Self {
            initial_mode: mode,
            ..self
        }
    }
}

/// Serves ACP over any transport, which is what makes the server testable
/// in-process — see `tests/acp.rs`, which drives it over `Channel::duplex()`.
///
/// [`serve_stdio`](crate::serve_stdio) is this over stdin and stdout, which is
/// what `lan` with no subcommand runs.
pub async fn serve<T>(config: ServeConfig, transport: T) -> Result<(), Error>
where
    T: agent_client_protocol::ConnectTo<Agent> + 'static,
{
    let sessions = SessionRegistry::new();

    Agent
        .builder()
        .name("lan")
        .on_receive_request(
            {
                let config = config.clone();
                async move |request: InitializeRequest, responder, _connection| {
                    responder.respond(initialize(&request, &config))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let config = config.clone();
                async move |request: ListSessionsRequest, responder, _connection| {
                    responder.respond_with_result(list_sessions(&config, request).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let config = config.clone();
                async move |request: NewSessionRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
                    let opened = new_session(&config, &sessions, request).await;

                    // Commands are announced after the response, because until
                    // a client has read the session id it has nothing to file
                    // the notification under.
                    let announcement = opened.as_ref().ok().map(|opened| {
                        (opened.response.session_id.clone(), opened.commands.clone())
                    });

                    responder.respond_with_result(opened.map(|opened| opened.response))?;

                    if let Some((session_id, commands)) = announcement {
                        // The session exists whether or not this lands, so a
                        // dead connection is not a failed `session/new`.
                        let _ = announce_commands(&connection, &session_id, commands);
                    }

                    Ok(Handled::Yes)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let config = config.clone();
                async move |request: LoadSessionRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
                    let sessions = sessions.clone();
                    let config = config.clone();
                    let spawned = connection.clone();

                    // Spawn: loading replays the transcript, which needs the
                    // turn lock a running turn may be holding.
                    connection.spawn(async move {
                        let modes = open_persisted(
                            &config,
                            &sessions,
                            &spawned,
                            request.session_id,
                            request.cwd,
                            request.mcp_servers,
                            Replay::Yes,
                        )
                        .await;

                        responder.respond_with_result(
                            modes.map(|modes| LoadSessionResponse::new().modes(modes)),
                        )
                    })?;

                    Ok(Handled::Yes)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let config = config.clone();
                async move |request: ResumeSessionRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
                    let sessions = sessions.clone();
                    let config = config.clone();
                    let spawned = connection.clone();

                    connection.spawn(async move {
                        let modes = open_persisted(
                            &config,
                            &sessions,
                            &spawned,
                            request.session_id,
                            request.cwd,
                            request.mcp_servers,
                            Replay::No,
                        )
                        .await;

                        responder.respond_with_result(
                            modes.map(|modes| ResumeSessionResponse::new().modes(modes)),
                        )
                    })?;

                    Ok(Handled::Yes)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: SetSessionModeRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
                    responder.respond_with_result(set_mode(&sessions, &connection, &request))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: CloseSessionRequest, responder, _connection| {
                    responder.respond_with_result(close_session(&sessions, &request))
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
///
/// Modes are not advertised here — ACP carries them per session, on the
/// `session/new` and `session/load` responses, because a session is what has
/// one.
fn initialize(request: &InitializeRequest, config: &ServeConfig) -> InitializeResponse {
    InitializeResponse::new(request.protocol_version)
        .agent_capabilities(
            AgentCapabilities::new()
                // Sessions are mentra agents, and mentra persists them, so a
                // client can reconnect to one this process never created.
                .load_session(true)
                // stdio is mandatory and needs no advertising. SSE does:
                // a client that is not told lan speaks it will never offer an
                // SSE server, and mentra has a client for that transport.
                .mcp_capabilities(McpCapabilities::new().sse(true))
                .prompt_capabilities(PromptCapabilities::new().embedded_context(true))
                .session_capabilities(
                    SessionCapabilities::new()
                        // The same resume as `session/load`, minus the
                        // replay, for a client that draws its own history.
                        .resume(SessionResumeCapabilities::new())
                        // Closing is real work here: it stops the turn and
                        // drops the runtime the session was holding.
                        .close(SessionCloseCapabilities::new())
                        // Only when the source can actually enumerate. A host
                        // serving sessions that die with the process has no
                        // list to offer, and should not claim one.
                        .list(
                            config
                                .source
                                .lists_sessions()
                                .then(SessionListCapabilities::new),
                        ),
                ),
        )
        // Left empty on purpose: lan has no login flow to offer. See the
        // module docs.
        .auth_methods(Vec::new())
        .agent_info(agent_client_protocol::schema::v1::Implementation::new(
            "lan",
            env!("CARGO_PKG_VERSION"),
        ))
}

/// The conversations persisted for one workspace.
///
/// Answers inline: this reads a SQLite table, which is the disk rather than the
/// client, and touches no lock a turn can hold.
async fn list_sessions(
    config: &ServeConfig,
    request: ListSessionsRequest,
) -> Result<ListSessionsResponse, Error> {
    // lan scopes conversations to the workspace they were opened in, so
    // "every session everywhere" is a question it cannot answer: mentra's
    // shared store has no index across workspaces. Saying so is better than
    // returning one workspace's sessions as though they were all of them, and
    // a client always knows its own `cwd` — it had to send one to open a
    // session at all.
    let cwd = request.cwd.ok_or_else(|| {
        Error::invalid_params().data("cwd is required: lan lists conversations per workspace")
    })?;

    let sessions = config
        .source
        .list_sessions(cwd.clone())
        .await
        .map_err(setup_failed)?;

    Ok(ListSessionsResponse::new(
        sessions
            .into_iter()
            .map(|session| session_info(session, &cwd))
            .collect(),
    ))
    // No cursor: the answer is one workspace's conversations, mentra returns
    // them in one read, and `next_cursor: None` tells a client there is no
    // second page to ask for.
}

/// One persisted conversation, as `session/list` reports it.
///
/// The `cwd` is the workspace that was listed rather than anything stored:
/// mentra's summary carries no path, but a conversation only appears in a
/// workspace's list *because* it belongs to that workspace, so this is the one
/// it was opened in.
///
/// `updated_at` is left unset. mentra orders by creation and exposes no
/// timestamp, and a client sorting by a value lan made up would be sorting by
/// nothing.
fn session_info(session: PersistedSession, cwd: &Path) -> SessionInfo {
    SessionInfo::new(session.agent_id, cwd.to_path_buf()).title(session.name)
}

/// A session that has just been opened, and what to tell the client about it
/// once it knows the session's id.
struct Opened {
    response: NewSessionResponse,
    commands: Vec<AvailableCommand>,
}

async fn new_session(
    config: &ServeConfig,
    sessions: &SessionRegistry,
    request: NewSessionRequest,
) -> Result<Opened, Error> {
    // Dropping what the client configured here was the bug: a session that
    // came up without its servers looks exactly like one whose servers had
    // nothing to offer.
    let mcp = crate::from_acp(&request.mcp_servers).map_err(|error| setup_failed(error.into()))?;

    let run = config
        .source
        .create(request.cwd, mcp)
        .await
        .map_err(setup_failed)?;

    // Read before the run moves into the session, where it would be behind a
    // lock a turn can hold.
    let commands = crate::available_commands(&run.context().templates);

    let session = AcpSession::new(run, config.initial_mode);
    let modes = session.modes().state();
    let id = sessions.insert(session);

    Ok(Opened {
        response: NewSessionResponse::new(id).modes(modes),
        commands,
    })
}

/// Whether a persisted conversation is streamed back to the client as it is
/// picked up. This is the whole difference between `session/load` and
/// `session/resume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replay {
    Yes,
    No,
}

/// Picks a persisted conversation back up, and reports the mode it is in.
///
/// Always called from a spawned task: it takes the turn lock, which a running
/// turn holds while waiting for the client.
#[allow(clippy::too_many_arguments)]
async fn open_persisted(
    config: &ServeConfig,
    sessions: &SessionRegistry,
    connection: &ConnectionTo<Client>,
    session_id: SessionId,
    cwd: PathBuf,
    mcp_servers: Vec<agent_client_protocol::schema::v1::McpServer>,
    replay: Replay,
) -> Result<SessionModeState, Error> {
    let session = match sessions.get(&session_id) {
        // Already open on this connection. Reconnecting a client that lost its
        // view is exactly when a replay is wanted, so this is not a shortcut
        // past the work — only past building a second runtime for the same
        // conversation.
        Some(session) => session,
        None => {
            let mcp = crate::from_acp(&mcp_servers).map_err(|error| setup_failed(error.into()))?;

            let run = config
                .source
                .resume(&session_id.0, cwd, mcp)
                .await
                .map_err(setup_failed)?;

            let session = AcpSession::new(run, config.initial_mode);
            sessions.insert(session.clone());
            session
        }
    };

    {
        let run = session.lock_turn().await;
        let commands = crate::available_commands(&run.context().templates);
        let updates = match replay {
            Replay::Yes => history::replay(run.history()),
            Replay::No => Vec::new(),
        };
        // Dropped before anything is sent: holding a turn lock across a write
        // to the client is the shape this module exists to avoid.
        drop(run);

        for update in updates {
            // A send failure means the client is gone. Stop rather than keep
            // writing into a dead socket; the pickup itself still succeeded.
            if notify(connection, &session_id, update).is_err() {
                break;
            }
        }

        // A client picking a conversation back up needs its commands as much
        // as a new one does; they came from the workspace, not the session.
        let _ = announce_commands(connection, &session_id, commands);
    }

    Ok(session.modes().state())
}

/// Switches a session's mode and tells the client it happened.
///
/// Answers inline. The mode sits outside the turn lock precisely so this can:
/// ACP says a mode may be set "whether the Agent is idle or actively
/// generating", and a switch that had to wait for the turn it was meant to
/// govern would arrive too late to govern it.
fn set_mode(
    sessions: &SessionRegistry,
    connection: &ConnectionTo<Client>,
    request: &SetSessionModeRequest,
) -> Result<SetSessionModeResponse, Error> {
    let session = sessions
        .get(&request.session_id)
        .ok_or_else(|| Error::invalid_params().data("unknown session"))?;

    session
        .modes()
        .set(&request.mode_id)
        .map_err(|error: ModeError| Error::invalid_params().data(error.to_string()))?;

    // The client that asked already knows, but a second view of the same
    // session does not, and ACP models the mode as agent state rather than as
    // one client's setting. The switch has happened either way, so a failed
    // notification must not be reported as a failed switch.
    let _ = notify(
        connection,
        &request.session_id,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(request.mode_id.clone())),
    );

    Ok(SetSessionModeResponse::new())
}

/// Drops a session, stopping whatever it was doing first.
///
/// Both halves matter: cancelling is what ACP requires, and forgetting the
/// session is what frees the mentra runtime behind it. A turn still unwinding
/// holds its own handle, so the runtime outlives this call by exactly as long
/// as that turn takes to notice it was cancelled.
fn close_session(
    sessions: &SessionRegistry,
    request: &CloseSessionRequest,
) -> Result<CloseSessionResponse, Error> {
    let session = sessions
        .remove(&request.session_id)
        .ok_or_else(|| Error::invalid_params().data("unknown session"))?;

    session.cancel();

    Ok(CloseSessionResponse::new())
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

    // The session's mode decides which of these requests the client actually
    // sees; the runtime surfaces every consequential call so that it can.
    let approver = ModedApprover::new(
        session.modes().clone(),
        AcpApprover::new(request.session_id.clone(), connection.clone()),
    );
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
            lan_core::RunOutcome::Error { message } => message,
            lan_core::RunOutcome::Ok => "the turn failed".to_string(),
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

/// Tells the client which prompt templates this session exposes as commands.
///
/// Nothing is sent when there are none: `AvailableCommandsUpdate` means "the
/// commands are ready or have changed", and an empty list on a session that
/// never had any says nothing a client can use.
fn announce_commands(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    commands: Vec<AvailableCommand>,
) -> Result<(), Error> {
    if commands.is_empty() {
        return Ok(());
    }

    notify(
        connection,
        session_id,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands)),
    )
}

/// Sends one `session/update`, mapping a dead connection to an ACP error.
///
/// Fire-and-forget, so it is safe from the dispatch loop as well as from a
/// spawned task: nothing waits for the client to say anything back.
fn notify(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    update: SessionUpdate,
) -> Result<(), Error> {
    connection.send_notification(SessionNotification::new(session_id.clone(), update))
}

/// Turns a setup failure into the error a client can act on.
///
/// A missing credential is the one failure with a remedy the protocol has a
/// name for. lan advertises no auth method to fix it with — there is no login,
/// only an environment variable — so the message carries the variable's name,
/// which is the actionable part.
fn setup_failed(error: lan_core::RunError) -> Error {
    if is_missing_credential(&error) {
        return Error::auth_required().data(error.to_string());
    }

    Error::internal_error().data(error.to_string())
}

fn is_missing_credential(error: &lan_core::RunError) -> bool {
    matches!(
        error,
        RunError::Provider(
            ProviderError::NoCredential
                | ProviderError::MissingCredential { .. }
                | ProviderError::NoCompatibleCredential
        )
    )
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
        notify(&self.connection, &self.session_id, update)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        ProtocolVersion,
        v1::{ErrorCode, ResourceLink, TextContent},
    };

    /// A source that cannot enumerate, which is what most hosts supplying
    /// their own sessions are.
    struct Ephemeral;

    #[async_trait::async_trait]
    impl SessionSource for Ephemeral {
        async fn create(
            &self,
            _cwd: PathBuf,
            _mcp: Vec<McpServer>,
        ) -> Result<PreparedRun, RunError> {
            Err(RunError::NoSuchSession)
        }
    }

    fn capabilities(config: &ServeConfig) -> SessionCapabilities {
        initialize(&InitializeRequest::new(ProtocolVersion::V1), config)
            .agent_capabilities
            .session_capabilities
    }

    #[test]
    fn initialize_advertises_resumable_sessions() {
        let response = initialize(
            &InitializeRequest::new(ProtocolVersion::V1),
            &ServeConfig::default(),
        );

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
    fn initialize_advertises_only_the_session_methods_lan_answers() {
        let capabilities = capabilities(&ServeConfig::default());

        assert!(capabilities.resume.is_some());
        assert!(capabilities.close.is_some());
        assert!(
            capabilities.list.is_some(),
            "the default source reads mentra's store, so it can enumerate"
        );
        assert!(
            capabilities.delete.is_none(),
            "mentra's store has no delete; claiming one would promise a deletion that undoes itself"
        );
    }

    #[test]
    fn a_source_that_cannot_enumerate_does_not_claim_a_list() {
        // Reporting "no sessions" for a workspace that has some is worse than
        // -32601, which at least says lan cannot answer.
        assert!(
            capabilities(&ServeConfig::with_source(Ephemeral))
                .list
                .is_none()
        );
    }

    #[test]
    fn no_authentication_method_is_offered() {
        // lan's credential comes from the environment. Offering a method here
        // would invite a call to `authenticate`, which answers -32601.
        assert!(
            initialize(
                &InitializeRequest::new(ProtocolVersion::V1),
                &ServeConfig::default()
            )
            .auth_methods
            .is_empty()
        );
    }

    #[test]
    fn a_listed_session_is_reported_in_the_workspace_it_was_listed_for() {
        let info = session_info(
            PersistedSession {
                agent_id: "agent-1".to_string(),
                name: "lan acp".to_string(),
                messages: 4,
            },
            Path::new("/repo"),
        );

        assert_eq!(
            &*info.session_id.0, "agent-1",
            "the agent id is the session id"
        );
        assert_eq!(
            info.cwd,
            PathBuf::from("/repo"),
            "a conversation is in this list because it belongs to this workspace"
        );
        assert_eq!(info.title.as_deref(), Some("lan acp"));
        assert_eq!(
            info.updated_at, None,
            "mentra exposes no timestamp, and a made-up one would sort a picker by nothing"
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
        // Denied rather than granted, so the assertion below has something to
        // catch: granted is the default, and a template that was dropped
        // entirely would still look right.
        let source = ConfiguredSource {
            template: Some(
                RunConfig::new("/placeholder", "").with_shell(lan_core::ShellAccess::Denied),
            ),
        };

        let built = source.config_for(PathBuf::from("/repo"), Vec::new());

        assert_eq!(built.workspace, PathBuf::from("/repo"));
        assert_eq!(
            built.shell,
            lan_core::ShellAccess::Denied,
            "everything the client cannot say must carry through"
        );
    }

    #[test]
    fn the_clients_mcp_servers_reach_the_config() {
        let source = ConfiguredSource { template: None };

        let built = source.config_for(
            PathBuf::from("/repo"),
            vec![McpServer::Stdio(mentra::McpServerConfig {
                name: "fs".to_string(),
                command: "/bin/mcp-fs".to_string(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
            })],
        );

        let supplied: Vec<&str> = built.mcp.supplied.iter().map(McpServer::name).collect();
        assert_eq!(
            supplied,
            vec!["fs"],
            "session/new is where a client says which servers it wants"
        );
    }

    #[test]
    fn a_session_opens_asking_unless_the_operator_says_otherwise() {
        // The library default is to allow everything, which is right for a
        // headless run and wrong here: a client that can be asked should be.
        assert_eq!(ServeConfig::default().initial_mode, ApprovalMode::Prompt);
        assert_eq!(
            ServeConfig::new(RunConfig::new("/repo", "")).initial_mode,
            ApprovalMode::Prompt,
            "a template says what a run is, not how much it may do without asking"
        );
        assert_eq!(
            ServeConfig::default()
                .with_initial_mode(ApprovalMode::Never)
                .initial_mode,
            ApprovalMode::Never,
            "`lan acp --approve never` opens every session read-only"
        );
    }

    #[test]
    fn a_missing_credential_is_an_authentication_failure_not_an_internal_one() {
        let error = setup_failed(RunError::Provider(ProviderError::NoCredential));

        assert_eq!(error.code, ErrorCode::AuthRequired);
        assert!(
            error
                .data
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .is_some_and(|data| data.contains("ANTHROPIC_API_KEY")),
            "the actionable part is which variable to set: {:?}",
            error.data
        );
    }

    #[test]
    fn other_setup_failures_stay_internal_errors() {
        // Reporting these as `auth_required` would send a client looking for a
        // login that would not have helped.
        let error = setup_failed(RunError::NoSuchSession);

        assert_eq!(error.code, ErrorCode::InternalError);
    }
}
