//! The attach executor's semantic pins, driven without a model: recorded
//! completions, the settle pass's ordering and containment, cancel and
//! deadline boundaries, and the runtime recipe.

use super::*;
use crate::local::state::RunOptions;
use serde_json::{Value, json};

fn handle(index: u8) -> String {
    format!("0123456789abcdef/{index:032x}")
}

fn record(
    data: &DataDir,
    task: &str,
    parent: Option<&str>,
    detached: bool,
    pending: Option<PendingTerminal>,
) -> AgentPaths {
    let paths = data.agent_dir(task).unwrap();
    std::fs::create_dir_all(paths.dir()).unwrap();
    let mut meta = TaskMeta::new(
        task.to_string(),
        parent.map(str::to_string),
        detached,
        "/repo".to_string(),
        "do not run".to_string(),
        RunOptions {
            provider: Some("not-a-provider".to_string()),
            approve: "never".to_string(),
            ..RunOptions::default()
        },
        None,
    );
    meta.pending_terminal = pending;
    save_meta(&paths, &meta).unwrap();
    paths
}

fn data() -> (tempfile::TempDir, DataDir) {
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::from_path(dir.path()).unwrap();
    (dir, data)
}

async fn drive_attached(data: &DataDir, task: &str) -> Value {
    let paths = data.agent_dir(task).unwrap();
    let guard = try_attach(&paths).unwrap().expect("lock is free");
    drive(data, task, guard).await.expect("drives to terminal")
}

#[tokio::test]
async fn a_recorded_completion_resumes_to_the_same_terminal() {
    let (_dir, data) = data();
    let task = handle(1);
    // The kill window: the worker finished (pending recorded) but died
    // before the terminal write. The next attach skips the model.
    let paths = record(
        &data,
        &task,
        None,
        true,
        Some(PendingTerminal::Succeeded {
            result: "done".to_string(),
        }),
    );

    let payload = drive_attached(&data, &task).await;
    assert_eq!(payload, json!({"state": "succeeded", "result": "done"}));
    assert_eq!(read_terminal(&paths).unwrap().unwrap(), payload);
    // Repeatable: another attach observes, never reruns.
    assert_eq!(drive_attached(&data, &task).await, payload);
}

#[tokio::test]
async fn successful_parent_finalizes_only_after_attached_child() {
    let (_dir, data) = data();
    let (parent, child) = (handle(1), handle(2));
    let parent_paths = record(
        &data,
        &parent,
        None,
        true,
        Some(PendingTerminal::Succeeded {
            result: "parent".to_string(),
        }),
    );
    let child_paths = record(
        &data,
        &child,
        Some(&parent),
        false,
        Some(PendingTerminal::Succeeded {
            result: "child".to_string(),
        }),
    );

    let payload = drive_attached(&data, &parent).await;
    assert_eq!(payload["result"], "parent");
    assert_eq!(
        read_terminal(&child_paths).unwrap().unwrap()["result"],
        "child",
        "the settle pass drives the child before the parent's terminal"
    );
    assert!(read_terminal(&parent_paths).unwrap().is_some());
    assert!(
        !cancel_requested(&child_paths),
        "a successful parent does not cancel its children"
    );
}

#[tokio::test]
async fn failed_parent_cancels_children_but_not_detached_work() {
    let (_dir, data) = data();
    let (parent, child, independent) = (handle(1), handle(2), handle(3));
    record(
        &data,
        &parent,
        None,
        true,
        Some(PendingTerminal::Failed {
            error: "boom".to_string(),
        }),
    );
    let child_paths = record(&data, &child, Some(&parent), false, None);
    let independent_paths = record(&data, &independent, Some(&parent), true, None);

    let payload = drive_attached(&data, &parent).await;
    assert_eq!(payload, json!({"state": "failed", "error": "boom"}));
    assert_eq!(
        read_terminal(&child_paths).unwrap().unwrap(),
        json!({"state": "cancelled"}),
        "a failing parent cancels and settles its attached child"
    );
    assert!(
        read_terminal(&independent_paths).unwrap().is_none()
            && !cancel_requested(&independent_paths),
        "detached work does not hold or inherit parent scope"
    );
}

#[tokio::test]
async fn cancel_before_any_turn_settles_without_a_model() {
    let (_dir, data) = data();
    let task = handle(1);
    // The bogus provider guarantees this test fails loudly if the
    // executor ever reaches runtime construction.
    let paths = record(&data, &task, None, true, None);
    request_cancel(&paths, None).unwrap();

    let payload = drive_attached(&data, &task).await;
    assert_eq!(payload, json!({"state": "cancelled"}));
}

#[tokio::test]
async fn a_deadline_bounds_an_agent_nobody_attached_to_in_time() {
    let (_dir, data) = data();
    let task = handle(1);
    let paths = record(&data, &task, None, true, None);
    let mut meta = load_meta(&paths).unwrap();
    meta.deadline_at_ms = Some(now_ms().saturating_sub(1));
    save_meta(&paths, &meta).unwrap();

    let payload = drive_attached(&data, &task).await;
    assert_eq!(payload["state"], "failed");
    assert_eq!(payload["stopped_by"], "deadline");
}

#[tokio::test]
async fn a_late_cancel_replaces_a_pending_completion() {
    let (_dir, data) = data();
    let task = handle(1);
    let paths = record(
        &data,
        &task,
        None,
        true,
        Some(PendingTerminal::Succeeded {
            result: "done".to_string(),
        }),
    );
    request_cancel(&paths, None).unwrap();

    let payload = drive_attached(&data, &task).await;
    assert_eq!(payload, json!({"state": "cancelled"}));
}

#[tokio::test]
async fn terminal_failure_resolves_unanswered_messages() {
    let (_dir, data) = data();
    let task = handle(1);
    let paths = record(&data, &task, None, true, None);
    let first = inbox::enqueue(&paths, &task, "first".to_string()).unwrap();
    let second = inbox::enqueue(&paths, &task, "second".to_string()).unwrap();
    inbox::start_next(&paths).unwrap();
    // The worker fails after accepting the messages.
    let mut meta = load_meta(&paths).unwrap();
    meta.pending_terminal = Some(PendingTerminal::Failed {
        error: "provider failed".to_string(),
    });
    save_meta(&paths, &meta).unwrap();

    drive_attached(&data, &task).await;
    let messages = inbox::load(&paths).unwrap();
    let terminal = read_terminal(&paths).unwrap().unwrap();
    for id in [first, second] {
        let payload = inbox::message_payload_for_dispatch(&task, &messages, &id, Some(&terminal))
            .unwrap()
            .expect("terminal resolves the message");
        assert_eq!(payload["state"], "failed");
        assert_eq!(payload["message"], id);
    }
}

#[test]
fn attached_deadlines_can_only_narrow_the_parent() {
    assert_eq!(earlier_deadline(Some(20), Some(10)), Some(10));
    assert_eq!(earlier_deadline(None, Some(10)), Some(10));
    assert_eq!(earlier_deadline(Some(20), None), Some(20));
    assert_eq!(earlier_deadline(None, None), None);
}

/// Asserted through `Debug` because the recipe's fields are private and
/// its values are redacted; the names and the store directory are what a
/// regression would drop.
#[test]
fn a_task_runs_on_the_workspace_store_and_knows_which_task_it_is() {
    let (_dir, data) = data();
    let (parent, child) = (handle(1), handle(2));
    record(&data, &parent, None, true, None);
    let child_paths = record(&data, &child, Some(&parent), false, None);
    let mut meta = load_meta(&child_paths).unwrap();
    meta.options.provider = None;
    save_meta(&child_paths, &meta).unwrap();

    let printed = format!("{:?}", task_runtime(&data, &child, &meta).unwrap());
    let store = data.store_dir("0123456789abcdef");
    assert!(
        printed.contains(&format!("Directory({store:?})")),
        "{printed}"
    );
    assert!(printed.contains("LAN_TASK_ID"), "{printed}");
    assert!(printed.contains("LAN_DATA_DIR"), "{printed}");
    assert!(printed.contains("LAN_PARENT_TASK_ID"), "{printed}");

    meta.parent = None;
    let root = format!("{:?}", task_runtime(&data, &child, &meta).unwrap());
    assert!(
        !root.contains("LAN_PARENT_TASK_ID"),
        "a root task has no parent to name"
    );
}

#[test]
fn an_unknown_provider_fails_the_task_rather_than_going_unread() {
    let (_dir, data) = data();
    let task = handle(1);
    let paths = record(&data, &task, None, true, None);
    let meta = load_meta(&paths).unwrap();
    assert!(task_runtime(&data, &task, &meta).is_err());
}

/// ADR-0020: `prompt` is answerable exactly when a process is driving the
/// agent *and* has a terminal to ask at. The interactive half cannot be
/// integration-tested — a test harness has no TTY — so the rule is pinned
/// here, where both halves can be stated.
#[test]
fn prompt_approval_needs_a_driver_with_a_terminal() {
    for mode in ["always", "never"] {
        assert!(validate_approval(mode, false).is_ok(), "{mode} asks nobody");
        assert!(validate_approval(mode, true).is_ok(), "{mode} asks nobody");
    }

    assert!(
        validate_approval("prompt", true).is_ok(),
        "an attached terminal is exactly what `prompt` needs"
    );

    let refused = validate_approval("prompt", false)
        .expect_err("nobody attached means nobody to ask")
        .to_string();
    assert!(refused.contains("terminal"), "{refused}");

    assert!(
        validate_approval("sometimes", true).is_err(),
        "an unknown mode is not quietly treated as one of the known ones"
    );
}
