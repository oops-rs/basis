//! The sessions a long-lived host is holding open.
//!
//! # Two locks, deliberately
//!
//! A session holds its [`PreparedRun`] behind an **async** mutex, held for the
//! whole turn — one conversation runs one turn at a time, so a second prompt
//! waits rather than interleaving.
//!
//! The cancellation token sits outside that lock, behind its own **sync**
//! mutex that is never held across an await. It has to: cancellation arrives
//! *while* a turn is running and therefore while the turn lock is held.
//! Putting the token inside would mean cancel waits for the turn it is trying
//! to cancel — a deadlock that only shows up when someone presses stop.
//!
//! The session's approval policy lives outside the turn lock for the same
//! reason, and is its own type — see [`SessionApproval`].
//!
//! # Why cancelling is two signals
//!
//! mentra's token is a flag the runner reads at round boundaries, and a turn
//! waiting on `session/request_permission` is not at one: it is parked inside
//! a tool call until the approver answers, and the approver is waiting on the
//! client. A stop pressed then would trip a flag nothing reads until an answer
//! arrives that the person who pressed stop is no longer going to give. So
//! the token travels with an [`Interrupt`] — an awaitable the approver selects
//! against — and [`cancel`](HostSession::cancel) trips both. The flag is what
//! ends the turn; the interrupt is what gets the turn to the boundary where
//! the flag is read.
//!
//! # Which id identifies a session
//!
//! basis uses mentra's **agent id**, not mentra's ephemeral session id.
//! mentra persists agents; a `Session` is one process's view of one. Keying
//! on the agent id is what makes resume free — it is exactly the handle
//! [`Workspace::resume`](basis::Workspace::resume) takes, so a host can
//! reconnect to a conversation this process never saw.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::SessionApproval;
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

/// What [`HostSession::begin_turn`] arms: mentra's flag and the interrupt that
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

/// One open conversation on a long-lived host.
#[derive(Clone)]
pub struct HostSession {
    /// Held for the duration of a turn.
    run: Arc<tokio::sync::Mutex<PreparedRun>>,
    /// Set while a turn is in flight. Reachable without the turn lock, which
    /// is the entire point — see the module docs.
    cancel: Arc<Mutex<Option<Armed>>>,
    /// Also reachable without the turn lock: a host may change policy while
    /// the agent is generating.
    approval: SessionApproval,
    agent_id: String,
}

impl HostSession {
    /// Opens a session with the approval state its host presents.
    pub fn new(run: PreparedRun, approval: SessionApproval) -> Self {
        Self {
            agent_id: run.agent_id().to_string(),
            run: Arc::new(tokio::sync::Mutex::new(run)),
            cancel: Arc::new(Mutex::new(None)),
            approval,
        }
    }

    /// The persisted conversation id for this session: mentra's agent id.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// This conversation's approval state, shared with its active turn.
    pub fn approval(&self) -> &SessionApproval {
        &self.approval
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A session over a scripted runtime, for tests that arm turns without
    /// running one.
    async fn session() -> HostSession {
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
        HostSession::new(run, SessionApproval::new(crate::ApprovalPolicy::Prompt))
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
}
