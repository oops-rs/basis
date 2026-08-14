//! The dynamic wait graph and the counted leases held against it.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use super::policy::validate_wait_edge;
use crate::local::service::Shared;

/// In-memory edges represent waits held by live request handlers. They are
/// deliberately not journaled: a process restart drops every lease, and a
/// persisted task can never inherit a wait edge whose owner no longer exists.
#[derive(Debug, Default)]
pub(in crate::local::service) struct WaitGraph {
    pub(super) edges: HashMap<String, HashMap<String, usize>>,
}

/// A counted edge in [`WaitGraph`]. Dropping it releases exactly one edge,
/// including when a request returns an error, times out, or is cancelled.
pub(in crate::local::service) struct WaitLease {
    pub(super) graph: Option<Arc<Mutex<WaitGraph>>>,
    pub(super) caller: Option<String>,
    pub(super) target: String,
}

impl WaitLease {
    pub(in crate::local::service) fn detached() -> Self {
        Self {
            graph: None,
            caller: None,
            target: String::new(),
        }
    }
}

impl Drop for WaitLease {
    fn drop(&mut self) {
        let (Some(graph), Some(caller)) = (&self.graph, &self.caller) else {
            return;
        };
        let Ok(mut graph) = graph.lock() else {
            return;
        };
        let Some(targets) = graph.edges.get_mut(caller) else {
            return;
        };
        let Some(count) = targets.get_mut(&self.target) else {
            return;
        };
        if *count > 1 {
            *count -= 1;
        } else {
            targets.remove(&self.target);
            if targets.is_empty() {
                graph.edges.remove(caller);
            }
        }
    }
}

impl WaitGraph {
    fn reaches(&self, start: &str, goal: &str) -> bool {
        let mut pending = vec![start];
        let mut visited = HashSet::new();
        while let Some(current) = pending.pop() {
            if current == goal {
                return true;
            }
            if !visited.insert(current) {
                continue;
            }
            if let Some(next) = self.edges.get(current) {
                pending.extend(next.keys().map(String::as_str));
            }
        }
        false
    }

    pub(super) fn try_acquire(&mut self, caller: String, target: String) -> Result<(), String> {
        if self.reaches(&target, &caller) {
            return Err(format!(
                "wait edge {caller} -> {target} would create a cycle"
            ));
        }
        let count = self
            .edges
            .entry(caller)
            .or_default()
            .entry(target)
            .or_default();
        *count = count
            .checked_add(1)
            .ok_or_else(|| "wait edge count exhausted".to_string())?;
        Ok(())
    }
}

/// Validate the static ownership policy and acquire one dynamic wait edge.
/// The journal lock is always taken before the graph lock; no lock is held by
/// the returned lease across an await.
pub(in crate::local::service) fn begin_wait(
    shared: &Shared,
    caller: Option<&str>,
    target: &str,
) -> Result<WaitLease, String> {
    validate_wait_edge(shared, caller, target)?;
    let Some(caller) = caller else {
        return Ok(WaitLease::detached());
    };
    let mut graph = shared.waits.lock().expect("wait graph poisoned");
    graph.try_acquire(caller.to_string(), target.to_string())?;
    Ok(WaitLease {
        graph: Some(shared.waits.clone()),
        caller: Some(caller.to_string()),
        target: target.to_string(),
    })
}

pub(super) fn wait_edge_would_cycle(shared: &Shared, caller: &str, target: &str) -> bool {
    shared
        .waits
        .lock()
        .expect("wait graph poisoned")
        .reaches(target, caller)
}
