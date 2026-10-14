//! A teammate: the one-off child basis already mints, kept instead of
//! dropped (ADR-0037 §5).
//!
//! `spawn "@reviewer look at this"` with no remote agent called `reviewer`
//! mints a child the way a plain task does — through the child policy, on
//! the parent's bounds — and keeps it under that name on the calling agent's
//! ledger row. Every later `@reviewer <task>` runs one more user turn on the
//! same agent, so it keeps its context; a reply that asks a question is
//! answered with the next `@reviewer <answer>`. Nothing outstanding, no
//! inbox: a turn ends when the teammate stops talking.
//!
//! The name is a label; the id is the identity (ADR-0037 §6). A teammate's id
//! is the mentra agent id it was minted with — already unique on the
//! runtime, already the id in every delegation edge — and `@<id>` reaches it
//! whatever it is called.
//!
//! Teammates live as long as the row that holds them: the run for a session,
//! the delegation for a one-off child, the parent teammate for a teammate's
//! own. A session resumed after a restart has none; the next `@<name> <task>`
//! makes a fresh one, and the reply shows a new id.

use std::sync::{Arc, Mutex};

use crate::runtime::agents::AgentRow;

/// One kept child: who it is, and the agent itself behind the one lock a
/// turn takes.
pub(crate) struct Teammate {
    id: String,
    name: String,
    /// `tokio`'s mutex, because the agent is held across the whole of a turn
    /// — every await in it — and a turn is the one thing that runs on it. A
    /// second caller does not wait: it is refused as *still working*, which
    /// is the remote rule (ADR-0029 §5) said once.
    agent: tokio::sync::Mutex<mentra::agent::Agent>,
    /// The ledger row that makes the teammate a basis agent for as long as it
    /// lives: judged by the parent's guard, and able to keep teammates of its
    /// own. `None` on a runtime with no ledger, which keeps no teammates.
    _row: Option<AgentRow>,
}

impl Teammate {
    pub(crate) fn new(name: &str, agent: mentra::agent::Agent, row: Option<AgentRow>) -> Self {
        Self {
            id: agent.id().to_string(),
            name: name.to_string(),
            agent: tokio::sync::Mutex::new(agent),
            _row: row,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The agent, for one turn — or `None` while another turn holds it.
    pub(crate) fn take(&self) -> Option<tokio::sync::MutexGuard<'_, mentra::agent::Agent>> {
        self.agent.try_lock().ok()
    }
}

/// Name and id only: an agent's `Debug` is its whole conversation.
impl std::fmt::Debug for Teammate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Teammate")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// One agent's teammates, in the order they were made.
#[derive(Debug, Default)]
pub(crate) struct Teammates {
    kept: Mutex<Vec<Arc<Teammate>>>,
}

impl Teammates {
    /// The teammate `key` names: by id first, then by name, so `@<id>` is
    /// never ambiguous and a name reaches the one made under it.
    pub(crate) fn find(&self, key: &str) -> Option<Arc<Teammate>> {
        let kept = self
            .kept
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        kept.iter()
            .find(|teammate| teammate.id() == key)
            .or_else(|| kept.iter().find(|teammate| teammate.name() == key))
            .map(Arc::clone)
    }

    /// Keeps `teammate` for as long as this lives.
    pub(crate) fn keep(&self, teammate: Arc<Teammate>) {
        self.kept
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(teammate);
    }

    /// Every teammate's name and id, in the order they were made — what
    /// `spawn` teaches.
    pub(crate) fn listed(&self) -> Vec<(String, String)> {
        self.kept
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|teammate| (teammate.name().to_string(), teammate.id().to_string()))
            .collect()
    }
}
