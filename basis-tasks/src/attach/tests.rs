//! The attach executor's semantic pins, driven without a model: recorded
//! completions, the settle pass's ordering and containment, cancel and
//! deadline boundaries, and the runtime recipe.

use super::*;
use crate::approve::Approve;
use crate::state::{MessageState, RunOptions};
use serde_json::{Value, json};

fn handle(index: u8) -> String {
    format!("0123456789abcdef/{index:032x}")
}

fn record(
    data: &DataDir,
    task: &str,
    parent: Option<&str>,
    detached: bool,
    pending: Option<Terminal>,
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
            approve: Approve::Never,
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
    drive(data, task, guard, &DriveContext::default())
        .await
        .expect("drives without error")
        .expect("nothing else claims this conversation, so this attempt settles")
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
        Some(Terminal::Succeeded {
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
        Some(Terminal::Succeeded {
            result: "parent".to_string(),
        }),
    );
    let child_paths = record(
        &data,
        &child,
        Some(&parent),
        false,
        Some(Terminal::Succeeded {
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
        Some(Terminal::Failed {
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
        Some(Terminal::Succeeded {
            result: "done".to_string(),
        }),
    );
    request_cancel(&paths, None).unwrap();

    let payload = drive_attached(&data, &task).await;
    assert_eq!(payload, json!({"state": "cancelled"}));
}

#[tokio::test]
async fn a_duration_too_large_to_be_a_deadline_waits_rather_than_panics() {
    let (_dir, data) = data();
    let task = handle(1);
    record(
        &data,
        &task,
        None,
        true,
        Some(Terminal::Succeeded {
            result: "done".to_string(),
        }),
    );
    // `Instant::now() + Duration::MAX` panics; the deadline has to saturate
    // instead, and a task that already has a terminal record must still
    // return it immediately — proof the saturated deadline never blocks a
    // wait that had no need to.
    let outcome = wait_for_terminal(&data, &task, Duration::MAX, &DriveContext::default())
        .await
        .expect("does not panic");
    assert_eq!(
        outcome,
        WaitOutcome::Terminal(TerminalRecord::from_raw(
            json!({"state": "succeeded", "result": "done"})
        ))
    );
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
    meta.pending_terminal = Some(Terminal::Failed {
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

/// T1: the settle pass's two writes are ordered so a crash between them
/// leaves a *resumable* task, never a settled one with a message it can no
/// longer re-sweep. Reconstructed directly as the crash window itself —
/// `finish_unanswered_durably` returns before the terminal write it is
/// supposed to precede — rather than by actually killing a process.
#[tokio::test]
async fn a_crash_between_the_settle_pass_writes_recovers_on_the_next_attach() {
    let (_dir, data) = data();
    let task = handle(1);
    let paths = record(&data, &task, None, true, None);
    let stranded = inbox::enqueue(&paths, &task, "are you there".to_string()).unwrap();

    // What run_model has already done by the time settle is ever reached:
    // pending_terminal recorded, durably, before either of settle's writes.
    let mut meta = load_meta(&paths).unwrap();
    meta.pending_terminal = Some(Terminal::Succeeded {
        result: "done".to_string(),
    });
    save_meta(&paths, &meta).unwrap();

    // The crash window: only the inbox write lands. Dropping the guard
    // without writing the terminal is the "process died right here" this
    // test stands in for.
    drop(inbox::finish_unanswered_durably(&paths).unwrap());
    assert!(
        read_terminal(&paths).unwrap().is_none(),
        "no terminal record crossed the crash — the task is still resumable"
    );
    let swept = inbox::load(&paths).unwrap();
    assert_eq!(
        swept
            .iter()
            .find(|message| message.id == stranded)
            .unwrap()
            .state,
        MessageState::Delivered,
        "the inbox write landed before the crash"
    );

    // The next attach recovers without a model turn: pending_terminal
    // survived, so run_model is skipped and settle simply finishes what it
    // started — re-sweeping (idempotent) and completing the terminal write.
    let payload = drive_attached(&data, &task).await;
    assert_eq!(payload, json!({"state": "succeeded", "result": "done"}));

    // And the message the crash stranded resolves — terminal-tagged, since
    // it was never individually replied to.
    let terminal = read_terminal(&paths).unwrap().unwrap();
    let messages = inbox::load(&paths).unwrap();
    let resolved =
        inbox::message_payload_for_dispatch(&task, &messages, &stranded, Some(&terminal))
            .unwrap()
            .expect("no longer stranded");
    assert_eq!(resolved["state"], "succeeded");
    assert_eq!(resolved["message"], stranded);
}

/// T2(b): a conversation already claimed — another lock holder standing in
/// for a sibling task already inside `Workspace::resume` on the same agent
/// id — is observed, not raced: `drive` backs off with `None` rather than
/// attempting the same resume, settles nothing, and releases the task's own
/// attach lock so a later attempt can retry once the conversation frees up.
#[tokio::test]
async fn a_claimed_conversation_is_observed_not_raced() {
    let (_dir, data) = data();
    let task = handle(1);
    let paths = record(&data, &task, None, true, None);
    let mut meta = load_meta(&paths).unwrap();
    meta.continues = Some("conversation-1".to_string());
    save_meta(&paths, &meta).unwrap();

    let (key, _) = valid_task_handle(&task).unwrap();
    let held = try_conversation(&data, key, "conversation-1")
        .unwrap()
        .expect("nobody else holds it yet");

    let guard = try_attach(&paths).unwrap().expect("lock is free");
    let outcome = drive(&data, &task, guard, &DriveContext::default())
        .await
        .expect("backing off is not an error");
    assert!(
        outcome.is_none(),
        "a claimed conversation is observed, not driven"
    );
    assert!(
        read_terminal(&paths).unwrap().is_none(),
        "nothing settled while the conversation was claimed elsewhere"
    );
    assert!(
        try_attach(&paths).unwrap().is_some(),
        "backing off releases this task's own attach lock for a later attempt"
    );

    drop(held);
    drive_attached(&data, &task).await;
    assert!(
        read_terminal(&paths).unwrap().is_some(),
        "once the conversation frees up, the next attempt makes progress"
    );
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
    assert!(printed.contains("BASIS_TASK_ID"), "{printed}");
    assert!(printed.contains("BASIS_DATA_DIR"), "{printed}");
    assert!(printed.contains("BASIS_PARENT_TASK_ID"), "{printed}");

    meta.parent = None;
    let root = format!("{:?}", task_runtime(&data, &child, &meta).unwrap());
    assert!(
        !root.contains("BASIS_PARENT_TASK_ID"),
        "a root task has no parent to name"
    );
}

#[test]
fn an_unknown_provider_fails_the_task_rather_than_going_unread() {
    let (_dir, data) = data();
    let task = handle(1);
    let paths = record(&data, &task, None, true, None);
    let meta = load_meta(&paths).unwrap();
    let runtime = task_runtime(&data, &task, &meta).expect("base runtime");
    assert!(run_parts(&meta, runtime).is_err());
}
