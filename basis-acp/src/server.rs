//! The ACP agent: what is registered on a connection, and where each handler
//! runs.
//!
//! The handlers themselves are split by what they touch:
//! [`lifecycle`] holds the `session/*` methods that open, list, switch and
//! close a conversation, [`turn`] holds the one that runs the agent,
//! [`config`] holds what the connection was configured with, and
//! [`workspaces`] holds what basis's own sessions are built on — one runtime for
//! the process, one workspace per directory (ADR-0018). What stays here is the
//! wiring, the capabilities answer that describes all of it, and the two sends
//! a handler may make from either side of the rule below.
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
//! `session/load`, `session/resume` and `session/set_config_option` spawn for
//! the second half of the same rule: they take a session's turn lock — to read
//! its transcript, or to reach the model and the effort that live on the run —
//! and a turn holds that lock while it waits for the client to answer. Taking
//! it from the loop would be the same deadlock wearing a different hat.
//!
//! `initialize`, `session/new`, `session/set_mode` and `session/close` answer
//! inline. They touch the disk, the provider's model list, and a sync mutex,
//! but never the client and never a lock a turn can be holding. `set_mode` is
//! the near miss worth naming: it looks like `set_config_option` and is not,
//! because the permission mode is deliberately kept *outside* the turn lock —
//! ACP says a mode may arrive mid-generation, and a model cannot change
//! mid-generation anyway.
//!
//! # What is not registered, and why
//!
//! An unregistered method answers `-32601`, which is an honest "basis cannot do
//! that". One is left that way deliberately:
//!
//! - **`authenticate`** — basis reads its credential from the environment
//!   (`ANTHROPIC_API_KEY` and the rest, see [`provider`](basis::provider)).
//!   There is no login to perform, no token to exchange, and so no auth method
//!   to advertise. A session opened without a credential fails with ACP's
//!   `auth_required` instead, naming the variable to set — which is the part a
//!   client can actually act on.
//!
//! `session/list` and `session/delete` are conditional rather than absent: both
//! are registered whatever the source is — a builder whose type changes with
//! each handler has no chain to skip one in — and each answers `-32601` itself
//! when the [`SessionSource`] cannot do it, which is the answer `initialize`
//! promised by not advertising the capability. See
//! [`lists_sessions`](SessionSource::lists_sessions) and
//! [`deletes_sessions`](SessionSource::deletes_sessions). `session/delete` was
//! unregistered outright until mentra grew a store delete: forgetting a
//! conversation in memory while the store handed it back on the next
//! `session/list` would have been a deletion that does not delete, which is the
//! one answer worse than `-32601`.

mod config;
mod lifecycle;
mod turn;
mod workspaces;

pub use config::{ServeConfig, SessionSource, SessionTemplate};

use agent_client_protocol::{
    Agent, Client, ConnectionTo, Handled,
    schema::v1::{
        AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, CancelNotification,
        CloseSessionRequest, DeleteSessionRequest, Error, InitializeRequest, InitializeResponse,
        ListSessionsRequest, LoadSessionRequest, LoadSessionResponse, McpCapabilities,
        NewSessionRequest, PromptCapabilities, PromptRequest, ResumeSessionRequest,
        ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
        SessionDeleteCapabilities, SessionId, SessionListCapabilities, SessionNotification,
        SessionResumeCapabilities, SessionUpdate, SetSessionConfigOptionRequest,
        SetSessionModeRequest,
    },
};

use crate::session::SessionRegistry;
use lifecycle::Replay;

/// Serves ACP over any transport, which is what makes the server testable
/// in-process — see `tests/acp/`, which drives it over `Channel::duplex()`.
///
/// [`serve_stdio`](crate::serve_stdio) is this over stdin and stdout, which is
/// what the binary's explicit `basis serve --acp` command runs.
pub async fn serve<T>(config: ServeConfig, transport: T) -> Result<(), Error>
where
    T: agent_client_protocol::ConnectTo<Agent> + 'static,
{
    let sessions = SessionRegistry::new();

    Agent
        .builder()
        .name("basis")
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
                        let picked = lifecycle::open_persisted(
                            &config,
                            &sessions,
                            &spawned,
                            request.session_id,
                            request.cwd,
                            request.mcp_servers,
                            Replay::Yes,
                        )
                        .await;

                        responder.respond_with_result(picked.map(|picked| {
                            LoadSessionResponse::new()
                                .modes(picked.modes)
                                .config_options(picked.options)
                        }))
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
                        let picked = lifecycle::open_persisted(
                            &config,
                            &sessions,
                            &spawned,
                            request.session_id,
                            request.cwd,
                            request.mcp_servers,
                            Replay::No,
                        )
                        .await;

                        responder.respond_with_result(picked.map(|picked| {
                            ResumeSessionResponse::new()
                                .modes(picked.modes)
                                .config_options(picked.options)
                        }))
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
                async move |request: SetSessionConfigOptionRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
                    let sessions = sessions.clone();
                    let spawned = connection.clone();

                    // Spawn: the model and the effort live on the run, behind
                    // the turn lock. See `lifecycle::set_config_option`.
                    connection.spawn(async move {
                        let settings =
                            lifecycle::set_config_option(&sessions, &spawned, request).await;

                        responder.respond_with_result(settings)
                    })?;

                    Ok(Handled::Yes)
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
                let config = config.clone();
                async move |request: DeleteSessionRequest,
                            responder,
                            connection: ConnectionTo<Client>| {
                    let sessions = sessions.clone();
                    let config = config.clone();

                    // Spawn: deleting waits out a turn in flight by taking the
                    // lock that turn is holding, and that turn is waiting for
                    // the client on this loop.
                    connection.spawn(async move {
                        let deleted = lifecycle::delete_session(&config, &sessions, request).await;

                        responder.respond_with_result(deleted)
                    })?;

                    Ok(Handled::Yes)
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

/// What basis can do, as told to a client.
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
                // stdio is mandatory and needs no advertising. The remote
                // transports do: a client that is not told basis speaks one
                // filters those servers out of `session/new`, so however well
                // the translation behind it works, an unadvertised transport
                // is an unreachable one. mentra has clients for both SSE and
                // Streamable HTTP.
                .mcp_capabilities(McpCapabilities::new().sse(true).http(true))
                // Images, because every provider mentra serves carries inline
                // image bytes and basis stopped being the layer that narrowed
                // a prompt to text. Audio is still not claimed: mentra has no
                // block for it, so a client offering one would be offering
                // basis something to drop.
                .prompt_capabilities(PromptCapabilities::new().embedded_context(true).image(true))
                .session_capabilities(
                    SessionCapabilities::new()
                        // The same resume as `session/load`, minus the
                        // replay, for a client that draws its own history.
                        .resume(SessionResumeCapabilities::new())
                        // Closing is real work here: it stops the turn and
                        // drops the conversation this process was holding open.
                        .close(SessionCloseCapabilities::new())
                        // Only when the source can actually enumerate. A host
                        // serving sessions that die with the process has no
                        // list to offer, and should not claim one.
                        .list(
                            config
                                .source
                                .lists_sessions()
                                .then(SessionListCapabilities::new),
                        )
                        // And only when it can actually remove one. A source
                        // that cannot is not asked; a client that asks anyway
                        // gets the -32601 this omission promised.
                        .delete(
                            config
                                .source
                                .deletes_sessions()
                                .then(SessionDeleteCapabilities::new),
                        ),
                ),
        )
        // Left empty on purpose: basis has no login flow to offer. See the
        // module docs.
        .auth_methods(Vec::new())
        .agent_info(agent_client_protocol::schema::v1::Implementation::new(
            "basis",
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
