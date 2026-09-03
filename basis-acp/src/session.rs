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

use crate::mode::{ApprovalMode, ModeGate, SessionModes};
use basis::{PreparedRun, run::TurnOptions};
use basis_host::HostSession;

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
    ///
    /// This is also where the session's [`ModeGate`] goes on, and it is the
    /// only place it can: mentra's attachment is live-only, so it has to be
    /// redone for every live session rather than once per conversation, and it
    /// has to be done before the first turn. Both `session/new` and
    /// `session/load` reach a conversation through here, and a session is
    /// filed in the registry — the only way a turn can find it — only after
    /// this returns. The gate holds the same [`SessionApproval`] the turn's
    /// approver reads, so one install follows every later `session/set_mode`.
    ///
    /// # No bound on the wait, and that is settled rather than overlooked
    ///
    /// Installing the gate *replaces* whatever authorizer the source's runtime
    /// carried, an [`ApprovalGate::with_timeout`](basis::ApprovalGate::with_timeout)
    /// included, and this constructor offers no way to restate one. Deliberate:
    /// a `ToolAuthorizer` timeout bounds mentra's wait, not the basis forwarder
    /// parked in the approver, so it cannot rescue a turn from a client that
    /// never answers — `PolicyGate`'s doc has the measurement. What can, and
    /// what ACP requires of a client abandoning a `session/request_permission`,
    /// is `session/cancel`; `tests/acp/permission.rs` pins that it ends the
    /// turn, along with close and delete.
    ///
    /// [`SessionApproval`]: basis_host::SessionApproval
    pub fn new(run: PreparedRun, initial_mode: ApprovalMode) -> Self {
        let modes = SessionModes::new(initial_mode);
        let run = run.with_tool_authorizer(ModeGate::new(modes.approval().clone()));

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
