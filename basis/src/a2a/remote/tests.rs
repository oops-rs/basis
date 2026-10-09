//! A remembered agent's conversations outlive the process that recorded them
//! (ADR-0029 §5); a delegated child's, and an ephemeral runtime's, do not.

use super::*;

fn working() -> Conversation {
    Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: Some(Outstanding {
            task_id: "task-1".to_string(),
            waiting_for: Waiting::Agent,
        }),
    }
}

#[test]
fn a_remembered_agent_finds_its_outstanding_task_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let before = RemoteAgents::unreachable_in(&["reviewer"], dir.path());
    before
        .record("agent-1", "reviewer", working(), true)
        .expect("written through");
    drop(before);

    // A new process: nothing in memory, the same directory on disk.
    let after = RemoteAgents::unreachable_in(&["reviewer"], dir.path());

    assert_eq!(after.conversation("agent-1", "reviewer", true), working());
    assert_eq!(
        after.conversation("agent-2", "reviewer", true),
        Conversation::default(),
        "another session's record is its own"
    );
}

#[test]
fn a_child_keeps_its_conversation_in_memory_only() {
    let dir = tempfile::tempdir().unwrap();
    let agents = RemoteAgents::unreachable_in(&["reviewer"], dir.path());

    agents
        .record("child-1", "reviewer", working(), false)
        .expect("nothing to write");

    assert_eq!(agents.conversation("child-1", "reviewer", false), working());
    assert!(
        std::fs::read_dir(dir.path()).is_err()
            || std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "a delegated child is never resumed, so nothing is written for it"
    );
}

#[test]
fn a_runtime_that_keeps_no_history_keeps_no_record() {
    let agents = RemoteAgents::unreachable(&["reviewer"]);

    agents
        .record("agent-1", "reviewer", working(), true)
        .expect("nothing to write");

    assert_eq!(agents.conversation("agent-1", "reviewer", true), working());
}

#[test]
fn forgetting_a_conversation_removes_its_record() {
    let dir = tempfile::tempdir().unwrap();
    let agents = RemoteAgents::unreachable_in(&["reviewer"], dir.path());
    agents
        .record("agent-1", "reviewer", working(), true)
        .expect("written through");

    forget(dir.path(), "agent-1").expect("removed");
    forget(dir.path(), "agent-1").expect("and absent is not an error");

    let after = RemoteAgents::unreachable_in(&["reviewer"], dir.path());
    assert_eq!(
        after.conversation("agent-1", "reviewer", true),
        Conversation::default()
    );
}

#[test]
fn an_id_that_is_not_a_file_name_is_never_made_into_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let agents = RemoteAgents::unreachable_in(&["reviewer"], dir.path());

    agents
        .record("../escape", "reviewer", working(), true)
        .expect("kept in memory");

    assert!(!dir.path().join("../escape.json").exists());
    assert_eq!(
        agents.conversation("../escape", "reviewer", true),
        working()
    );
}
