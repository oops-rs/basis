//! ACP identity and mode presentation over basis-host's session discipline.
//!
//! [`HostSession`] owns the turn lock and cancellation/interrupt pair. This
//! adapter keeps only what is wire-specific: [`SessionId`] conversion and the
//! ACP mode picker beside that host session.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::schema::v1::SessionId;

use crate::mode::{ApprovalMode, SessionModes};
use basis::{PreparedRun, run::TurnOptions};
use basis_host::{HostSession, SessionRegistry as HostRegistry};

pub use basis_host::Interrupt;

/// One open conversation.
#[derive(Clone)]
pub struct AcpSession {
    host: HostSession,
    modes: SessionModes,
}

impl AcpSession {
    /// Opens a session at `initial_mode`, which is where the client's mode
    /// picker starts.
    pub fn new(run: PreparedRun, initial_mode: ApprovalMode) -> Self {
        let modes = SessionModes::new(initial_mode);
        Self {
            host: HostSession::new(run, modes.approval().clone()),
            modes,
        }
    }

    /// The ACP session id for this conversation: mentra's persisted agent id.
    pub fn id(&self) -> SessionId {
        SessionId::new(self.host.agent_id())
    }

    /// This conversation's mode, shared with whatever turn is running.
    pub fn modes(&self) -> &SessionModes {
        &self.modes
    }

    /// Takes the turn lock. Held until the returned guard drops.
    pub async fn lock_turn(&self) -> tokio::sync::MutexGuard<'_, PreparedRun> {
        self.host.lock_turn().await
    }

    /// Arms a fresh cancellation token for a turn about to start.
    pub fn begin_turn(&self) -> TurnOptions {
        self.host.begin_turn()
    }

    /// The interrupt armed for the turn in flight, for whatever is about to
    /// wait on the client on that turn's behalf. `None` between turns.
    pub fn interrupt(&self) -> Option<Interrupt> {
        self.host.interrupt()
    }

    /// Disarms after a turn ends, so a late `session/cancel` cannot cancel the
    /// *next* turn.
    pub fn end_turn(&self) {
        self.host.end_turn();
    }

    /// Trips the in-flight turn's token and wakes anything waiting on the
    /// client for it. `false` when no turn is running.
    pub fn cancel(&self) -> bool {
        self.host.cancel()
    }
}

/// Every conversation this connection is holding.
///
/// Cloneable: each clone shares one map, which is what lets a spawned prompt
/// task and the dispatch loop reach the same session.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    sessions: HostRegistry,
    modes: Arc<Mutex<HashMap<String, SessionModes>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Files a session under its own id and hands that id back.
    pub fn insert(&self, session: AcpSession) -> SessionId {
        let id = session.id();
        self.mode_lock()
            .insert(id.0.to_string(), session.modes.clone());
        self.sessions.insert(session.host);
        id
    }

    pub fn get(&self, id: &SessionId) -> Option<AcpSession> {
        let host = self.sessions.get(&id.0)?;
        let modes = self.mode_lock().get(&*id.0).cloned()?;
        Some(AcpSession { host, modes })
    }

    pub fn remove(&self, id: &SessionId) -> Option<AcpSession> {
        let host = self.sessions.remove(&id.0)?;
        let modes = self.mode_lock().remove(&*id.0)?;
        Some(AcpSession { host, modes })
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn mode_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionModes>> {
        // A poisoned registry means some other task panicked mid-update. The
        // map itself is still structurally sound, and refusing to serve every
        // later request over it would turn one panic into a dead connection.
        self.modes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
