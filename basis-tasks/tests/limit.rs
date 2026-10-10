//! A full workspace is refused as one, so a host can tell its callers so
//! rather than report its own failure.

use std::{path::Path, time::Duration};

use basis_tasks::{Approve, MAX_TASKS, RunSpec, Tasks};

fn spec() -> RunSpec {
    RunSpec::new("hello")
        .with_approve(Approve::Always)
        .with_deadline(Duration::from_secs(60))
        .detached()
}

/// The directory a task's own directory sits in.
fn agents_dir(root: &Path, id: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if path.file_name().is_some_and(|name| name == id) {
            return path.parent().map(Path::to_path_buf);
        }
        if path.is_dir()
            && let Some(found) = agents_dir(&path, id)
        {
            return Some(found);
        }
    }
    None
}

#[test]
fn a_full_workspace_is_refused_as_a_task_limit() {
    let data = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    let tasks = Tasks::open_at(data.path(), workspace.path()).expect("opens");
    let first = tasks.spawn(spec()).expect("spawns");
    let id = first.as_str().rsplit_once('/').unwrap().1;
    let agents = agents_dir(data.path(), id).expect("the task has a directory");
    for filler in 1..MAX_TASKS {
        std::fs::create_dir(agents.join(format!("filler-{filler}"))).unwrap();
    }

    let refused = tasks.spawn(spec()).expect_err("the workspace is full");
    assert!(refused.is_task_limit(), "{refused}");
    let other = tasks
        .spawn(RunSpec::new("  "))
        .expect_err("an empty prompt is refused");
    assert!(!other.is_task_limit());
}
