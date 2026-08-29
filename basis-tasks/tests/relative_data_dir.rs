//! A relative data-directory root names one place, not one per directory.
//!
//! `BASIS_DATA_DIR` is a path a person types, and a path a person types can be
//! relative. Left relative, it is re-resolved against whatever the current
//! directory happens to be at the moment it is read — so a host that changes
//! directory mid-process, and a child `basis` that inherits the variable and
//! resolves it from its own cwd (ADR-0022 decision 6 publishes that
//! inheritance as a contract), each see a *different* data directory under one
//! spelling. Resolving once, at [`Tasks::open_at`]'s construction of the root,
//! against the cwd that was current then, is what makes it one place.
//!
//! This file owns its own process because it moves the process's current
//! directory, which no test sharing a process with it could survive. One test,
//! deliberately: a second here would race it for the same global.

use std::time::Duration;

use basis_tasks::{Approve, RunSpec, Tasks};

#[test]
fn a_relative_root_stays_where_it_was_opened_after_the_process_changes_directory() {
    let home = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(home.path().join("data")).expect("create the relative root");

    std::env::set_current_dir(home.path()).expect("cd home");
    let tasks = Tasks::open_at("data", workspace.path()).expect("opens at a relative root");
    let handle = tasks
        .spawn(
            RunSpec::new("hello")
                .with_approve(Approve::Always)
                // Never driven: this test is about where the task's files
                // live, not about a turn.
                .with_deadline(Duration::from_secs(60))
                .detached(),
        )
        .expect("spawns");

    // Then move the process, the way a host that opened `Tasks` once and then
    // changed directory does.
    std::env::set_current_dir(elsewhere.path()).expect("cd elsewhere");

    let listed = tasks.list().expect("lists");
    assert!(
        listed.iter().any(|task| task.task == handle.as_str()),
        "the task minted under the relative root must still be found from \
         another directory, not looked for under a second root of the same \
         spelling: {listed:?}"
    );
    assert!(!tasks.is_attached(&handle).expect("probes"));
    assert_eq!(
        tasks.workspace_of(&handle).expect("resolves"),
        std::fs::canonicalize(workspace.path()).expect("canonical workspace")
    );
}
