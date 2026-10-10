//! A task records the agent-to-agent hop its work arrived at (ADR-0030 §9),
//! durably, for its own delegations to read.

use std::time::Duration;

use basis_tasks::{Approve, RunSpec, Tasks};

fn spec() -> RunSpec {
    RunSpec::new("hello")
        .with_approve(Approve::Always)
        // Never driven: this is about what spawn records.
        .with_deadline(Duration::from_secs(60))
        .detached()
}

#[test]
fn a_task_records_the_hop_it_was_spawned_at_and_absent_is_zero() {
    let data = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    let tasks = Tasks::open_at(data.path(), workspace.path()).expect("opens");

    let served = tasks.spawn(spec().with_hop(3)).expect("spawns");
    assert_eq!(tasks.hop(&served).expect("reads"), 3);

    let local = tasks.spawn(spec()).expect("spawns");
    assert_eq!(tasks.hop(&local).expect("reads"), 0);
}
