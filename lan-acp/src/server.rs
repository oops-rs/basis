//! The ACP agent: what is registered on a connection, and where each handler
//! runs.
//!
//! The handlers themselves are split by what they touch:
//! [`lifecycle`] holds the `session/*` methods that open, list, switch and
//! close a conversation, [`turn`] holds the one that runs the agent, and
//! [`config`] holds what the connection was configured with. What stays here
//! is the wiring, the capabilities answer that describes all of it, and the
//! two sends a handler may make from either side of the rule below.
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

mod config;
mod lifecycle;
mod turn;

pub use config::{ServeConfig, SessionSource};

use agent_client_protocol::{
    Agent, Client, ConnectionTo, Handled,
    schema::v1::{
        AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, CancelNotification,
        CloseSessionRequest, Error, InitializeRequest, InitializeResponse, ListSessionsRequest,
        LoadSessionRequest, LoadSessionResponse, McpCapabilities, NewSessionRequest,
        PromptCapabilities, PromptRequest, ResumeSessionRequest, ResumeSessionResponse,
        SessionCapabilities, SessionCloseCapabilities, SessionId, SessionListCapabilities,
        SessionNotification, SessionResumeCapabilities, SessionUpdate, SetSessionModeRequest,
    },
};

use crate::session::SessionRegistry;
use lifecycle::Replay;

/// Serves ACP over any transport, which is what makes the server testable
/// in-process — see `tests/acp/`, which drives it over `Channel::duplex()`.
///
/// [`serve_stdio`](crate::serve_stdio) is this over stdin and stdout, which is
/// what the binary's explicit `lan serve --acp` command runs.
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
                    responder.respond_with_result(lifecycle::list_sessions(&config, request).await)
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
                    let opened = lifecycle::new_session(&config, &sessions, request).await;

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
                        let modes = lifecycle::open_persisted(
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
                        let modes = lifecycle::open_persisted(
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
                    responder.respond_with_result(lifecycle::set_mode(
                        &sessions,
                        &connection,
                        &request,
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: CloseSessionRequest, responder, _connection| {
                    responder.respond_with_result(lifecycle::close_session(&sessions, &request))
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
                        let outcome = turn::prompt(&sessions, &spawned, request).await;
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

#[cfg(test)]
mod tests;
