//! The static ownership policy behind wait, cancel, and hint decisions.

use super::graph::wait_edge_would_cycle;
use crate::local::{service::Shared, store::Journal};

pub(in crate::local::service) fn validate_wait_edge(
    shared: &Shared,
    caller: Option<&str>,
    target: &str,
) -> Result<(), String> {
    let Some(caller) = caller else {
        return Ok(());
    };
    if caller == target {
        return Err("a task cannot await itself".to_string());
    }
    let journal = shared.journal.lock().expect("task journal poisoned");
    if !journal.contains_key(target) {
        return Err(format!("task {target} does not exist"));
    }
    if !journal.contains_key(caller) {
        return Err(format!("caller task {caller} does not exist"));
    }
    if is_ancestor(&journal, target, caller) {
        return Err(format!(
            "task {caller} cannot await its ancestor {target}; send without --await instead"
        ));
    }
    if is_ancestor(&journal, caller, target) {
        return Ok(());
    }
    if root_of(&journal, caller) == root_of(&journal, target) {
        return Err(format!(
            "task {caller} cannot await peer {target}; only descendants or independent roots are safe"
        ));
    }
    Ok(())
}

pub(super) fn validate_cancel_target(
    shared: &Shared,
    caller: Option<&str>,
    target: &str,
) -> Result<(), String> {
    let journal = shared.journal.lock().expect("task journal poisoned");
    if !journal.contains_key(target) {
        return Err(format!("task {target} does not exist"));
    }
    let Some(caller) = caller else {
        return Ok(());
    };
    if !journal.contains_key(caller) {
        return Err(format!("caller task {caller} does not exist"));
    }
    if caller == target || is_ancestor(&journal, caller, target) {
        return Ok(());
    }
    if is_ancestor(&journal, target, caller) {
        return Err(format!("task {caller} cannot cancel its ancestor {target}"));
    }
    Err(format!(
        "task {caller} cannot cancel peer {target}; only itself or descendants are allowed"
    ))
}

/// Return a next action that is legal for the submitting task. Enqueue-only
/// sends intentionally do not acquire a wait lease; when the target is an
/// ancestor, peer, or self, suggest inspecting the target's inbox instead of
/// suggesting an impossible `lan wait` edge.
pub(in crate::local::service) fn send_next_hint(
    shared: &Shared,
    caller: Option<&str>,
    target: &str,
    message: &str,
) -> String {
    if let Some(caller) = caller
        && (validate_wait_edge(shared, Some(caller), target).is_err()
            || wait_edge_would_cycle(shared, caller, target))
    {
        return format!("lan inbox {target}");
    }
    format!("lan wait {target} --message {message}")
}

fn is_ancestor(journal: &Journal, ancestor: &str, descendant: &str) -> bool {
    let mut current = journal
        .get(descendant)
        .and_then(|record| record.parent.as_deref());
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = journal.get(id).and_then(|record| record.parent.as_deref());
    }
    false
}

fn root_of<'a>(journal: &'a Journal, task: &'a str) -> &'a str {
    let mut current = task;
    while let Some(parent) = journal
        .get(current)
        .and_then(|record| record.parent.as_deref())
    {
        current = parent;
    }
    current
}
