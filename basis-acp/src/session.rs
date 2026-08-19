//! The sessions an ACP connection is holding open.
//!
//! # Two locks, deliberately
//!
//! A session holds its [`PreparedRun`] behind an **async** mutex, held for the
//! whole turn — one conversation runs one turn at a time, which is what ACP
//! assumes, so a second `session/prompt` waits rather than interleaving.
//!
//! The cancellation token sits outside that lock, behind its own **sync**
//! mutex that is never held across an await. It has to: `session/cancel`
//! arrives *while* a turn is running and therefore while the turn lock is
//! held. Putting the token inside would mean cancel waits for the turn it is
//! trying to cancel — a deadlock that only shows up when someone presses stop.
//!
//! The session's mode lives outside the turn lock for the same reason, and is
//! its own type — see [`mode`](crate::mode).
//!
//! # Which id is the session id
//!
//! basis uses mentra's **agent id**, not its session id. mentra persists agents;
//! a `Session` is one process's view of one. Keying on the agent id is what
//! makes `session/load` free — it is exactly the handle
//! [`Workspace::resume`](basis_core::Workspace::resume) takes, so a client can
//! reconnect to a conversation this process never saw.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::schema::v1::SessionId;

use crate::mode::{ApprovalMode, SessionModes};
use basis_core::{PreparedRun, run::TurnOptions};
use mentra::runtime::CancellationToken;

/// One open conversation.
#[derive(Clone)]
pub struct AcpSession {
    /// Held for the duration of a turn.
    run: Arc<tokio::sync::Mutex<PreparedRun>>,
    /// Set while a turn is in flight. Reachable without the turn lock, which
    /// is the entire point — see the module docs.
    cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// Also reachable without the turn lock: ACP says `session/set_mode` may
    /// arrive while the agent is generating.
    modes: SessionModes,
    id: SessionId,
}

impl AcpSession {
    /// Opens a session at `initial_mode`, which is where the client's mode
    /// picker starts.
    pub fn new(run: PreparedRun, initial_mode: ApprovalMode) -> Self {
        Self {
            id: SessionId::new(run.agent_id().to_string()),
            run: Arc::new(tokio::sync::Mutex::new(run)),
            cancel: Arc::new(Mutex::new(None)),
            modes: SessionModes::new(initial_mode),
        }
    }

    /// The ACP session id for this conversation: mentra's persisted agent id.
    pub fn id(&self) -> SessionId {
        self.id.clone()
    }

    /// This conversation's mode, shared with whatever turn is running.
    pub fn modes(&self) -> &SessionModes {
        &self.modes
    }

    /// Takes the turn lock. Held until the returned guard drops.
    pub async fn lock_turn(&self) -> tokio::sync::MutexGuard<'_, PreparedRun> {
        self.run.lock().await
    }

    /// Arms a fresh cancellation token for a turn about to start.
    pub fn begin_turn(&self) -> TurnOptions {
        let (options, token) = TurnOptions::cancellable();
        *self.cancel_slot() = Some(token);
        options
    }

    /// Disarms after a turn ends, so a late `session/cancel` cannot cancel the
    /// *next* turn.
    pub fn end_turn(&self) {
        *self.cancel_slot() = None;
    }

    /// Trips the in-flight turn's token. `false` when no turn is running.
    pub fn cancel(&self) -> bool {
        match self.cancel_slot().take() {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    fn cancel_slot(&self) -> std::sync::MutexGuard<'_, Option<CancellationToken>> {
        self.cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Every conversation this connection is holding.
///
/// Cloneable: each clone shares one map, which is what lets a spawned prompt
/// task and the dispatch loop reach the same session.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    sessions: Arc<Mutex<HashMap<SessionId, AcpSession>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Files a session under its own id and hands that id back.
    pub fn insert(&self, session: AcpSession) -> SessionId {
        let id = session.id();
        self.lock().insert(id.clone(), session);
        id
    }

    pub fn get(&self, id: &SessionId) -> Option<AcpSession> {
        self.lock().get(id).cloned()
    }

    pub fn remove(&self, id: &SessionId) -> Option<AcpSession> {
        self.lock().remove(id)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, AcpSession>> {
        // A poisoned registry means some other task panicked mid-update. The
        // map itself is still structurally sound, and refusing to serve every
        // later request over it would turn one panic into a dead connection.
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_session_is_not_found() {
        let registry = SessionRegistry::new();

        assert!(registry.is_empty());
        assert!(registry.get(&SessionId::new("nobody")).is_none());
        assert!(registry.remove(&SessionId::new("nobody")).is_none());
    }

    #[test]
    fn clones_share_one_map() {
        let registry = SessionRegistry::new();
        let clone = registry.clone();

        assert_eq!(registry.len(), clone.len());
        assert!(clone.is_empty());
    }
}
