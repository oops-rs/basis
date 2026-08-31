//! Opening, listing, switching and closing a conversation: every `session/*`
//! method except the turn.
//!
//! What these share is the registry and nothing else — each one looks a
//! session up, puts one in, or takes one out, and none of them runs the agent.
//! The turn is next door in [`turn`](super::turn) because holding a session's
//! run lock for as long as a model takes is a different job from opening it,
//! and `initialize` stays with [`serve`](super::serve) because what basis
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
        AvailableCommand, CloseSessionRequest, CloseSessionResponse, ConfigOptionUpdate,
        CurrentModeUpdate, DeleteSessionRequest, DeleteSessionResponse, Error, ListSessionsRequest,
        ListSessionsResponse, NewSessionRequest, NewSessionResponse, SessionConfigOption,
        SessionId, SessionInfo, SessionModeState, SessionUpdate, SetSessionConfigOptionRequest,
        SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
    },
};

use super::{ServeConfig, announce_commands, notify};
use crate::{
    history,
    mode::ModeError,
    options,
    session::{AcpSession, SessionRegistry},
};
use basis::{PersistedSession, RunError, provider::ProviderError};

/// The conversations persisted for one workspace.
///
/// Answers inline: this reads the store's files, which is the disk rather than
/// the client, and touches no lock a turn can hold.
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

    // basis scopes conversations to the workspace they were opened in, so
    // "every session everywhere" is a question it cannot answer: mentra's
    // shared store has no index across workspaces. Saying so is better than
    // returning one workspace's sessions as though they were all of them, and
    // a client always knows its own `cwd` — it had to send one to open a
    // session at all.
    let cwd = request.cwd.ok_or_else(|| {
        Error::invalid_params().data("cwd is required: basis lists conversations per workspace")
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
/// `updated_at` is ACP's slot for exactly what basis now has — "ISO 8601
/// timestamp of last activity" — rendered from the epoch second the store
/// holds. A conversation with none is sent without one rather than with a
/// guess: ACP treats the field as optional, and a client sorting by a value
/// basis made up would be sorting by nothing. The list is already in that
/// order when it arrives ([`store::list_in`](basis::store::list_in)), so this
/// is what a client needs to *show* the ordering, not to reproduce it.
pub(super) fn session_info(session: PersistedSession, cwd: &Path) -> SessionInfo {
    SessionInfo::new(session.agent_id, cwd.to_path_buf())
        .title(session.name)
        .updated_at(session.updated_at.map(crate::timestamp::rfc3339))
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

    // Read here, while the run is still in hand: once it is inside the session
    // it is behind a lock a turn can hold, and `session/new` answers inline.
    let settings = options::options(&run.context().model, run.effort());

    let session = AcpSession::new(run, config.initial_mode);
    let modes = session.modes().state();
    let id = sessions.insert(session);

    Ok(Opened {
        response: NewSessionResponse::new(id)
            .modes(modes)
            .config_options(settings),
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

/// The state a picked-up conversation reports: what it will and will not do
/// without asking, and what it is set to.
///
/// One struct because `session/load` and `session/resume` answer with the same
/// two, and reading them out of one turn lock is what keeps them describing the
/// same instant.
pub(super) struct Picked {
    pub(super) modes: SessionModeState,
    pub(super) options: Vec<SessionConfigOption>,
}

/// Picks a persisted conversation back up, and reports the mode and the
/// settings it is in.
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
) -> Result<Picked, Error> {
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

    let settings = {
        let run = session.lock_turn().await;
        let commands = crate::available_commands(&run.context().templates);
        let settings = options::options(&run.context().model, run.effort());
        let updates = match replay {
            Replay::Yes => history::replay(run.text_history()),
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

        settings
    };

    Ok(Picked {
        modes: session.modes().state(),
        options: settings,
    })
}

/// Changes one of the session's settings, and tells the client it happened.
///
/// Spawned rather than answered inline, unlike `session/set_mode` next door.
/// The reason is the turn lock: the model and the effort live on the
/// [`PreparedRun`](basis::PreparedRun) — where mentra persists them, so they
/// survive `session/load` — and reaching them means taking the lock a running
/// turn holds while it waits for the client to answer a permission request.
/// Taking that from the dispatch loop is ADR-0007's deadlock by its second
/// route.
///
/// Waiting for the turn costs nothing a client can observe. Both settings take
/// effect from the *next* turn either way: mentra reads them when it builds
/// each model request, so a turn already in flight was never going to change
/// under this.
pub(super) async fn set_config_option(
    sessions: &SessionRegistry,
    connection: &ConnectionTo<Client>,
    request: SetSessionConfigOptionRequest,
) -> Result<SetSessionConfigOptionResponse, Error> {
    let session = sessions
        .get(&request.session_id)
        .ok_or_else(|| Error::invalid_params().data("unknown session"))?;

    // Read before the lock: an id basis never advertised is a client error, and
    // making it queue behind a running turn to hear so would be a slow no.
    let change = options::change(&request.config_id, &request.value)
        .map_err(|error| Error::invalid_params().data(error.to_string()))?;

    let settings = {
        let mut run = session.lock_turn().await;

        match change {
            options::Change::Effort(effort) => run.set_effort(effort),
            options::Change::Model(model) => run.set_model(model),
        }
        .map_err(|error: RunError| Error::internal_error().data(error.to_string()))?;

        options::options(&run.context().model, run.effort())
    };

    // The client that asked already knows, but a second view of the same
    // session does not — ACP models these as agent state, not as one client's
    // setting. The change has happened either way, so a failed notification
    // must not be reported as a failed change.
    let _ = notify(
        connection,
        &request.session_id,
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(settings.clone())),
    );

    Ok(SetSessionConfigOptionResponse::new(settings))
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

/// Removes a conversation from the store for good, closing it first if this
/// process was holding it open.
///
/// Always called from a spawned task: it takes the turn lock, which a running
/// turn holds while waiting for the client.
///
/// The order is the whole of it, and it is what makes this a deletion rather
/// than a suggestion. mentra's delete removes rows; an agent still live in
/// memory keeps running and writes its row back on the next persist. So the
/// session leaves the registry first, so nothing can start a turn on it; then
/// its turn lock is taken, which is how a turn already running is waited out
/// rather than raced; then the last handle this process holds goes; and only
/// then is the row deleted. A client that never opened the conversation skips
/// straight to the last step, which is the ordinary case — `session/delete`
/// arrives from a list, not from a session.
///
/// Deleting a conversation nothing knows about succeeds. Unlike
/// `session/close`, which is about a session this process is holding and can
/// honestly say it has never heard of, this is about a row: a client deleting
/// by an id it read from a list is racing anyone else holding that store, and
/// "it is gone" is the outcome both of them asked for.
pub(super) async fn delete_session(
    config: &ServeConfig,
    sessions: &SessionRegistry,
    request: DeleteSessionRequest,
) -> Result<DeleteSessionResponse, Error> {
    if !config.source.deletes_sessions() {
        return Err(Error::method_not_found());
    }

    if let Some(session) = sessions.remove(&request.session_id) {
        session.cancel();
        // Taken and dropped: what is wanted is the *wait*, not the guard.
        drop(session.lock_turn().await);
        drop(session);
    }

    config
        .source
        .delete(&request.session_id.0)
        .await
        .map_err(setup_failed)?;

    Ok(DeleteSessionResponse::new())
}

/// Turns a setup failure into the error a client can act on.
///
/// Two failures have a remedy the client can carry out; the rest are basis's.
///
/// A missing credential is the one failure with a remedy the protocol has a
/// name for. basis advertises no auth method to fix it with — there is no login,
/// only an environment variable — so the message carries the variable's name,
/// which is the actionable part.
///
/// A conversation that is already open is the other. mentra leases an agent to
/// the session holding it, for as long as that session lives, so a
/// `session/load` of a conversation another connection to this process — or
/// another process — is holding is refused by the runtime itself. That is the
/// attach discipline of ADR-0019 at the protocol edge, and basis keeps no set
/// of its own beside it. What basis owes the client is the reading: reported
/// as `-32603` it looked like basis breaking, when the fix is to close the
/// conversation where it is open.
pub(super) fn setup_failed(error: basis::RunError) -> Error {
    if is_missing_credential(&error) {
        return Error::auth_required().data(error.to_string());
    }

    if error.is_open_elsewhere() {
        return Error::invalid_params().data(format!(
            "this conversation is already open — on another connection to this process, or in \
             another process — and one connection drives it at a time; close it there first \
             ({error})"
        ));
    }

    Error::internal_error().data(error.to_string())
}

fn is_missing_credential(error: &basis::RunError) -> bool {
    matches!(
        error,
        RunError::Provider(ProviderError::NoCredential | ProviderError::MissingCredential { .. })
    )
}
