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
//! # Why cancelling is two signals
//!
//! mentra's token is a flag the runner reads at round boundaries, and a turn
//! waiting on `session/request_permission` is not at one: it is parked inside
//! a tool call until the approver answers, and the approver is waiting on the
//! client. A stop pressed then would trip a flag nothing reads until an answer
//! arrives that the person who pressed stop is no longer going to give. So
//! the token travels with an [`Interrupt`] — an awaitable the approver selects
//! against — and [`cancel`](AcpSession::cancel) trips both. The flag is what
//! ends the turn; the interrupt is what gets the turn to the boundary where
//! the flag is read.
//!
//! # Which id is the session id
//!
//! basis uses mentra's **agent id**, not its session id. mentra persists agents;
//! a `Session` is one process's view of one. Keying on the agent id is what
//! makes `session/load` free — it is exactly the handle
//! [`Workspace::resume`](basis::Workspace::resume) takes, so a client can
//! reconnect to a conversation this process never saw.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::schema::v1::SessionId;

use tokio::sync::watch;

use crate::mode::{ApprovalMode, SessionModes};
use basis::{CancellationToken, PreparedRun, run::TurnOptions};

/// The half of a cancellation an approver can wait on.
///
/// Cloneable, so a turn's approver and anything else parked on the client can
/// each hold one; [`wait`](Self::wait) resolves once for all of them when the
/// turn is cancelled, and resolves at once if it already was — a stop pressed
/// before the question was asked is still a stop.
#[derive(Clone, Debug)]
pub struct Interrupt {
    cancelled: watch::Receiver<bool>,
}

impl Interrupt {
    /// Resolves when the turn this belongs to is cancelled.
    ///
    /// Also resolves if the turn has already ended and disarmed its token —
    /// there is nothing left for a waiter to wait for, and an approver still
    /// asking after the turn is gone has no turn to answer.
    pub async fn wait(&mut self) {
        let _ = self.cancelled.wait_for(|cancelled| *cancelled).await;
    }
}

/// What [`AcpSession::begin_turn`] arms: mentra's flag and the interrupt that
/// wakes an approver to go and read it.
struct Armed {
    token: CancellationToken,
    interrupt: watch::Sender<bool>,
}

impl Armed {
    fn cancel(&self) {
        self.token.cancel();
        self.interrupt.send_replace(true);
    }
}

/// One open conversation.
#[derive(Clone)]
pub struct AcpSession {
    /// Held for the duration of a turn.
    run: Arc<tokio::sync::Mutex<PreparedRun>>,
    /// Set while a turn is in flight. Reachable without the turn lock, which
    /// is the entire point — see the module docs.
    cancel: Arc<Mutex<Option<Armed>>>,
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
        let (interrupt, _) = watch::channel(false);
        *self.cancel_slot() = Some(Armed { token, interrupt });
        options
    }

    /// The interrupt armed for the turn in flight, for whatever is about to
    /// wait on the client on that turn's behalf. `None` between turns.
    pub fn interrupt(&self) -> Option<Interrupt> {
        self.cancel_slot().as_ref().map(|armed| Interrupt {
            cancelled: armed.interrupt.subscribe(),
        })
    }

    /// Disarms after a turn ends, so a late `session/cancel` cannot cancel the
    /// *next* turn.
    pub fn end_turn(&self) {
        *self.cancel_slot() = None;
    }

    /// Trips the in-flight turn's token and wakes anything waiting on the
    /// client for it. `false` when no turn is running.
    pub fn cancel(&self) -> bool {
        match self.cancel_slot().take() {
            Some(armed) => {
                armed.cancel();
                true
            }
            None => false,
        }
    }

    fn cancel_slot(&self) -> std::sync::MutexGuard<'_, Option<Armed>> {
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

    /// A session over a scripted runtime, for tests that arm turns without
    /// running one.
    async fn session() -> AcpSession {
        let mock = mentra::test::MockRuntime::builder()
            .model("mock-model", "openai")
            .with_policy(mentra::RuntimePolicy::permissive())
            .text("unused")
            .build()
            .expect("mock runtime builds");
        let workspace = tempfile::tempdir().expect("tempdir");
        let mentra_session = mock
            .runtime()
            .create_session("test", mock.model())
            .expect("session");
        let run = basis::run::prepare_with_session(
            mentra_session,
            workspace.path(),
            "",
            &basis::ContextConfig {
                file_name: "AGENTS.md".to_string(),
                global_dir: None,
                walk_parents: false,
            },
            "openai",
            "mock-model",
        )
        .expect("prepared");
        AcpSession::new(run, ApprovalMode::Prompt)
    }

    #[tokio::test]
    async fn there_is_nothing_to_interrupt_between_turns() {
        let session = session().await;

        assert!(session.interrupt().is_none());
        assert!(!session.cancel(), "and nothing to cancel");
    }

    #[tokio::test]
    async fn cancelling_wakes_whoever_is_waiting_on_the_turn() {
        let session = session().await;
        let options = session.begin_turn();
        let mut interrupt = session.interrupt().expect("a turn is armed");

        assert!(session.cancel());

        // Both halves tripped: the flag mentra reads, and the wake-up that
        // gets the turn to where it reads it.
        assert!(options.cancel.expect("armed").is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(1), interrupt.wait())
            .await
            .expect("the interrupt must fire");
    }

    #[tokio::test]
    async fn a_turn_that_ended_leaves_no_one_waiting() {
        let session = session().await;
        let _options = session.begin_turn();
        let mut interrupt = session.interrupt().expect("a turn is armed");

        session.end_turn();

        tokio::time::timeout(std::time::Duration::from_secs(1), interrupt.wait())
            .await
            .expect("a waiter on a finished turn must not wait forever");
        assert!(session.interrupt().is_none());
    }

    #[test]
    fn clones_share_one_map() {
        let registry = SessionRegistry::new();
        let clone = registry.clone();

        assert_eq!(registry.len(), clone.len());
        assert!(clone.is_empty());
    }
}
