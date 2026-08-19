//! How deep a delegation already is, and where it stops.
//!
//! mentra has a floor of its own, and it does not apply here: a disposable
//! subagent gets `task` added to its hidden set by name
//! (`DisposableSubagentTemplate::spawn`), which stops *mentra's* delegation
//! tool from nesting and says nothing about a tool basis registered. ADR-0016
//! therefore makes the guard `spawn`'s own.
//!
//! # Why a ledger rather than the agent's name
//!
//! A subagent is named `{parent}::task` by mentra, so counting `::task` in
//! `agent_name()` would give the depth for free — and would fail *open* the
//! day mentra renames its subagents, which is the wrong direction for a guard
//! to fail in. This keys on agent ids instead, which are the identity mentra
//! promises. One [`SpawnTool`](super::SpawnTool) is registered per runtime and
//! every subagent shares that runtime's registry, so one ledger sees the whole
//! tree.
//!
//! Entries live exactly as long as the delegated run: `spawn` owns the child's
//! whole lifetime — it awaits `Agent::run` — so [`Depth::entered`] hands back a
//! guard that removes the entry on drop, including on an early return or a
//! panic. A long session therefore holds one entry per delegation *in flight*,
//! not one per delegation ever made.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// How many levels of delegation `spawn` will start.
///
/// Zero would be mentra's answer for `task` — a subagent that cannot delegate
/// at all. Two is the smallest bound that leaves delegation compositional (a
/// subagent may split its own work once) while keeping runaway recursion
/// structurally impossible rather than merely unlikely. The root run is depth
/// 0, so the deepest agent that can still delegate is depth 1.
pub const MAX_DEPTH: usize = 2;

/// The delegation depth of every agent with a delegation in flight.
///
/// Absent means depth 0: a root run has never been recorded by anything.
#[derive(Debug, Default, Clone)]
pub(crate) struct Depth {
    by_agent: Arc<Mutex<HashMap<String, usize>>>,
}

impl Depth {
    /// The caller's own depth, or the refusal the model should read instead.
    ///
    /// Asked before a delegation and never before a command: depth bounds
    /// *nesting*, and an agent at the floor is still allowed to do the work
    /// itself, which is exactly what running a command is.
    pub(crate) fn authorize_delegation(&self, agent_id: &str) -> Result<usize, String> {
        let depth = self.of(agent_id);
        if depth >= MAX_DEPTH {
            return Err(refusal(depth));
        }

        Ok(depth)
    }

    /// Records `agent_id` as running at `depth` until the returned guard drops.
    pub(crate) fn entered(&self, agent_id: &str, depth: usize) -> Entered {
        self.lock().insert(agent_id.to_string(), depth);

        Entered {
            by_agent: Arc::clone(&self.by_agent),
            agent_id: agent_id.to_string(),
        }
    }

    fn of(&self, agent_id: &str) -> usize {
        self.lock().get(agent_id).copied().unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.by_agent
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// Removes one agent's depth entry when the delegation that opened it ends.
pub(crate) struct Entered {
    by_agent: Arc<Mutex<HashMap<String, usize>>>,
    agent_id: String,
}

impl Drop for Entered {
    fn drop(&mut self) {
        self.by_agent
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.agent_id);
    }
}

/// What the model reads when the guard fires.
///
/// It names the floor and says what to do instead, because a refusal that only
/// says no is one the model answers by trying again.
fn refusal(depth: usize) -> String {
    format!(
        "this work is already {depth} levels of delegation deep and spawn goes no deeper than \
         {MAX_DEPTH}; do it here rather than handing it on"
    )
}
