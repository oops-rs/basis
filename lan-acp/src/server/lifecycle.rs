//! Opening, listing, switching and closing a conversation: every `session/*`
//! method except the turn.
//!
//! What these share is the registry and nothing else — each one looks a
//! session up, puts one in, or takes one out, and none of them runs the agent.
//! The turn is next door in [`turn`](super::turn) because holding a session's
//! run lock for as long as a model takes is a different job from opening it,
//! and `initialize` stays with [`serve`](super::serve) because what lan
//! advertises follows from what it registers.
//!
//! Which of these may block the dispatch loop is the parent module's rule, and
//! stated there: the parent is what decides where a handler runs.
//!
//! [`setup_failed`] lives here rather than beside the other translations to the
//! wire because opening a session is the only thing that can fail for want of
//! a credential.

use std::path::{Path, PathBuf};

use agent_client_protocol::{
    Client, ConnectionTo,
    schema::v1::{
        AvailableCommand, CloseSessionRequest, CloseSessionResponse, CurrentModeUpdate, Error,
        ListSessionsRequest, ListSessionsResponse, NewSessionRequest, NewSessionResponse,
        SessionId, SessionInfo, SessionModeState, SessionUpdate, SetSessionModeRequest,
        SetSessionModeResponse,
    },
};

use super::{ServeConfig, announce_commands, notify};
use crate::{
    history,
    mode::ModeError,
    session::{AcpSession, SessionRegistry},
};
use lan_core::{PersistedSession, RunError, provider::ProviderError};

/// The conversations persisted for one workspace.
///
/// Answers inline: this reads a SQLite table, which is the disk rather than the
/// client, and touches no lock a turn can hold.
pub(super) async fn list_sessions(
    config: &ServeConfig,
    request: ListSessionsRequest,
) -> Result<ListSessionsResponse, Error> {
    // The handler is registered whatever the source is — the builder's type
    // changes with every handler added, so there is no chain to skip one in —
    // so this is where a source that cannot enumerate says so. `-32601` is the
    // answer `initialize` promised by not advertising the capability, and an
    // empty list from a workspace that has conversations is the wrong answer
    // this whole capability exists to avoid.
    if !config.source.lists_sessions() {
        return Err(Error::method_not_found());
    }

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
pub(super) fn session_info(session: PersistedSession, cwd: &Path) -> SessionInfo {
    SessionInfo::new(session.agent_id, cwd.to_path_buf()).title(session.name)
}

/// A session that has just been opened, and what to tell the client about it
/// once it knows the session's id.
pub(super) struct Opened {
    pub(super) response: NewSessionResponse,
    pub(super) commands: Vec<AvailableCommand>,
}

pub(super) async fn new_session(
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
pub(super) enum Replay {
    Yes,
    No,
}

/// Picks a persisted conversation back up, and reports the mode it is in.
///
/// Always called from a spawned task: it takes the turn lock, which a running
/// turn holds while waiting for the client.
#[allow(clippy::too_many_arguments)]
pub(super) async fn open_persisted(
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
        // past the work — only past minting a second session on a conversation
        // this process is already holding.
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
pub(super) fn set_mode(
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
/// session is what frees the conversation — its transcript, and the mentra
/// agent holding it — behind it. A turn still unwinding holds its own handle,
/// so that outlives this call by exactly as long as the turn takes to notice
/// it was cancelled. What closing does *not* free is the runtime or the
/// workspace: since ADR-0018 both are the process's, shared by every session on
/// that directory, and are not one session's to release.
pub(super) fn close_session(
    sessions: &SessionRegistry,
    request: &CloseSessionRequest,
) -> Result<CloseSessionResponse, Error> {
    let session = sessions
        .remove(&request.session_id)
        .ok_or_else(|| Error::invalid_params().data("unknown session"))?;

    session.cancel();

    Ok(CloseSessionResponse::new())
}

/// Turns a setup failure into the error a client can act on.
///
/// A missing credential is the one failure with a remedy the protocol has a
/// name for. lan advertises no auth method to fix it with — there is no login,
/// only an environment variable — so the message carries the variable's name,
/// which is the actionable part.
pub(super) fn setup_failed(error: lan_core::RunError) -> Error {
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
