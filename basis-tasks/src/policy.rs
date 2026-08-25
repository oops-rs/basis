//! The static ownership policy behind wait and cancel.
//!
//! ADR-0017's rules, evaluated over `meta.json` parent chains instead of the
//! daemon's journal. The dynamic wait graph is gone (ADR-0019): a wait is a
//! process observing a file, a cycle is two observers, and the finite deadline
//! bounds it — so only the static tree shape is checked here.
//!
//! What next command a caller should be told to run instead — `basis inbox`
//! rather than an impossible `basis wait` — is not this module's business:
//! that is a hint, and hints are `basis-cli`'s (ADR-0015). [`Tasks::can_await`](crate::Tasks::can_await)
//! is the fact a host builds one from.

use super::{
    data_dir::DataDir,
    state::{MAX_TASKS, TaskMeta, load_meta},
};

fn meta_of(data: &DataDir, task: &str) -> Option<TaskMeta> {
    let paths = data.agent_dir(task)?;
    paths.exists().then(|| load_meta(&paths).ok()).flatten()
}

pub(crate) fn validate_wait_edge(
    data: &DataDir,
    caller: Option<&str>,
    target: &str,
) -> Result<(), String> {
    let Some(caller) = caller else {
        return Ok(());
    };
    if caller == target {
        return Err("a task cannot await itself".to_string());
    }
    if meta_of(data, target).is_none() {
        return Err(format!("task {target} does not exist"));
    }
    if meta_of(data, caller).is_none() {
        return Err(format!("caller task {caller} does not exist"));
    }
    if is_ancestor(data, target, caller) {
        return Err(format!(
            "task {caller} cannot await its ancestor {target}; send without --await instead"
        ));
    }
    if is_ancestor(data, caller, target) {
        return Ok(());
    }
    if root_of(data, caller) == root_of(data, target) {
        return Err(format!(
            "task {caller} cannot await peer {target}; only descendants or independent roots are safe"
        ));
    }
    Ok(())
}

pub(crate) fn validate_cancel_target(
    data: &DataDir,
    caller: Option<&str>,
    target: &str,
) -> Result<(), String> {
    if meta_of(data, target).is_none() {
        return Err(format!("task {target} does not exist"));
    }
    let Some(caller) = caller else {
        return Ok(());
    };
    if meta_of(data, caller).is_none() {
        return Err(format!("caller task {caller} does not exist"));
    }
    if caller == target || is_ancestor(data, caller, target) {
        return Ok(());
    }
    if is_ancestor(data, target, caller) {
        return Err(format!("task {caller} cannot cancel its ancestor {target}"));
    }
    Err(format!(
        "task {caller} cannot cancel peer {target}; only itself or descendants are allowed"
    ))
}

fn is_ancestor(data: &DataDir, ancestor: &str, descendant: &str) -> bool {
    let mut current = meta_of(data, descendant).and_then(|meta| meta.parent);
    // Bounded walk: corrupt metadata must not become an infinite loop.
    for _ in 0..MAX_TASKS {
        match current {
            Some(id) if id == ancestor => return true,
            Some(id) => current = meta_of(data, &id).and_then(|meta| meta.parent),
            None => return false,
        }
    }
    false
}

fn root_of(data: &DataDir, task: &str) -> String {
    let mut current = task.to_string();
    for _ in 0..MAX_TASKS {
        match meta_of(data, &current).and_then(|meta| meta.parent) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{RunOptions, save_meta};

    pub(crate) fn record(data: &DataDir, task: &str, parent: Option<&str>) {
        let paths = data.agent_dir(task).expect("well-formed handle");
        std::fs::create_dir_all(paths.dir()).unwrap();
        let meta = TaskMeta::new(
            task.to_string(),
            parent.map(str::to_string),
            false,
            "/repo".to_string(),
            "prompt".to_string(),
            RunOptions::default(),
            None,
        );
        save_meta(&paths, &meta).unwrap();
    }

    fn handle(index: u8) -> String {
        format!("0123456789abcdef/{:032x}", index)
    }

    fn tree() -> (tempfile::TempDir, DataDir, [String; 4]) {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::from_path(dir.path()).unwrap();
        let [root, child, peer, other] = [handle(1), handle(2), handle(3), handle(4)];
        record(&data, &root, None);
        record(&data, &child, Some(&root));
        record(&data, &peer, Some(&root));
        record(&data, &other, None);
        (dir, data, [root, child, peer, other])
    }

    #[test]
    fn wait_edges_allow_descendants_and_independent_roots_only() {
        let (_dir, data, [root, child, peer, other]) = tree();

        assert!(validate_wait_edge(&data, Some(&root), &child).is_ok());
        assert!(validate_wait_edge(&data, Some(&root), &other).is_ok());
        assert!(validate_wait_edge(&data, Some(&child), &root).is_err());
        assert!(validate_wait_edge(&data, Some(&child), &peer).is_err());
        assert!(validate_wait_edge(&data, Some(&root), &root).is_err());
    }

    #[test]
    fn cancellation_authority_only_flows_down_the_attached_tree() {
        let (_dir, data, [root, child, _peer, other]) = tree();

        let upward = validate_cancel_target(&data, Some(&child), &root)
            .expect_err("a child cannot cancel its owner");
        assert!(upward.contains("ancestor"), "{upward}");

        let sideways = validate_cancel_target(&data, Some(&root), &other)
            .expect_err("an attached task cannot cancel an independent root");
        assert!(sideways.contains("peer"), "{sideways}");

        assert!(validate_cancel_target(&data, Some(&root), &child).is_ok());
        assert!(validate_cancel_target(&data, Some(&root), &root).is_ok());
        assert!(validate_cancel_target(&data, None, &root).is_ok());
    }
}
