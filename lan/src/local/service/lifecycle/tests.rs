//! Lifecycle tests: wait-edge policy, cancellation trees, and settling.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use lan_core::CancellationToken;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, watch};

use super::{
    WaitGraph, WaitLease, begin_wait, cancel_task, duration_from_ms, finish_failed,
    message_payload_for_dispatch, orphan_running,
    policy::validate_wait_edge,
    send_next_hint, settle_or_take_message,
    transition::{
        apply_completion, request_cancel_tree, request_cancel_tree_locked, transition_controls,
    },
    wait::{DEFAULT_WAIT, MAX_WAIT, watch_snapshot, watch_task},
};
use crate::local::{
    protocol::VERSION,
    registry::{Descriptor, Registry, canonical_workspace, workspace_key},
    service::Shared,
    store::{DurableState, Journal, PendingTerminal, TaskRecord},
};

fn record(id: &str, parent: Option<&str>) -> TaskRecord {
    TaskRecord::new(
        id.to_string(),
        parent.map(str::to_string),
        false,
        "/repo".to_string(),
        String::new(),
        None,
    )
}

fn test_shared() -> (TempDir, Shared) {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Registry::from_path(dir.path().join("registry")).expect("registry");
    let workspace = canonical_workspace(dir.path()).expect("workspace");
    let (changed, _) = watch::channel(0_u64);
    let descriptor = Descriptor {
        version: VERSION,
        instance: workspace_key(&workspace),
        workspace: workspace.to_string_lossy().into_owned(),
        endpoint: "127.0.0.1:1".to_string(),
        token: "token".to_string(),
        pid: std::process::id(),
    };
    let shared = Shared {
        registry,
        descriptor,
        workspace,
        journal: Arc::new(Mutex::new(Journal::new())),
        controls: Arc::new(Mutex::new(HashMap::new())),
        persist_gate: Arc::new(AsyncMutex::new(())),
        changed,
        waits: Arc::new(Mutex::new(WaitGraph::default())),
    };
    (dir, shared)
}

#[test]
fn wait_edges_allow_descendants_and_independent_roots_only() {
    let (_dir, shared) = test_shared();
    {
        let mut journal = shared.journal.lock().expect("journal");
        journal.insert("root".to_string(), record("root", None));
        journal.insert("child".to_string(), record("child", Some("root")));
        journal.insert("peer".to_string(), record("peer", Some("root")));
        journal.insert("other".to_string(), record("other", None));
    }

    assert!(validate_wait_edge(&shared, Some("root"), "child").is_ok());
    assert!(validate_wait_edge(&shared, Some("root"), "other").is_ok());
    assert!(validate_wait_edge(&shared, Some("child"), "root").is_err());
    assert!(validate_wait_edge(&shared, Some("child"), "peer").is_err());
    assert!(validate_wait_edge(&shared, Some("root"), "root").is_err());
}

#[test]
fn cancellation_stays_inside_the_attached_tree() {
    let (_dir, shared) = test_shared();
    {
        let mut journal = shared.journal.lock().expect("journal");
        journal.insert("root".to_string(), record("root", None));
        journal.insert("child".to_string(), record("child", Some("root")));
        let mut independent = record("independent", None);
        independent.detached = true;
        journal.insert("independent".to_string(), independent);
    }

    request_cancel_tree(&shared, "root", true).expect("cancel tree");
    let journal = shared.journal.lock().expect("journal");
    assert!(journal["root"].cancel_requested);
    assert!(journal["child"].cancel_requested);
    assert!(!journal["independent"].cancel_requested);
}

#[test]
fn terminal_watch_snapshot_carries_a_next_action() {
    let (_dir, shared) = test_shared();
    let mut terminal = record("task", None);
    terminal.state = DurableState::Succeeded {
        result: "done".to_string(),
    };
    shared
        .journal
        .lock()
        .expect("journal")
        .insert("task".to_string(), terminal);

    let snapshot = watch_snapshot(&shared, "task", 0).expect("snapshot");
    assert_eq!(
        snapshot["result"]["next"],
        "lan watch task or lan inbox task"
    );
}

#[test]
fn opposite_independent_wait_edges_are_rejected() {
    let mut graph = WaitGraph::default();
    graph
        .try_acquire("left".to_string(), "right".to_string())
        .expect("first edge");
    let error = graph
        .try_acquire("right".to_string(), "left".to_string())
        .expect_err("opposite edge would deadlock");
    assert!(error.contains("cycle"), "{error}");
}

#[test]
fn duplicate_wait_leases_are_counted_until_each_drops() {
    let graph = Arc::new(Mutex::new(WaitGraph::default()));
    let first = {
        let mut guard = graph.lock().expect("graph");
        guard
            .try_acquire("caller".to_string(), "target".to_string())
            .expect("first edge");
        WaitLease {
            graph: Some(graph.clone()),
            caller: Some("caller".to_string()),
            target: "target".to_string(),
        }
    };
    let second = {
        let mut guard = graph.lock().expect("graph");
        guard
            .try_acquire("caller".to_string(), "target".to_string())
            .expect("duplicate edge");
        WaitLease {
            graph: Some(graph.clone()),
            caller: Some("caller".to_string()),
            target: "target".to_string(),
        }
    };
    drop(first);
    assert_eq!(graph.lock().expect("graph").edges["caller"]["target"], 1);
    drop(second);
    assert!(graph.lock().expect("graph").edges.is_empty());
}

#[test]
fn send_hint_does_not_recommend_a_dynamic_wait_cycle() {
    let (_dir, shared) = test_shared();
    {
        let mut journal = shared.journal.lock().expect("journal");
        journal.insert("left".to_string(), record("left", None));
        journal.insert("right".to_string(), record("right", None));
    }
    let lease = begin_wait(&shared, Some("right"), "left").expect("initial wait edge");

    assert_eq!(
        send_next_hint(&shared, Some("left"), "right", "message-1"),
        "lan inbox right"
    );

    drop(lease);
    assert_eq!(
        send_next_hint(&shared, Some("left"), "right", "message-1"),
        "lan wait right --message message-1"
    );
}

#[tokio::test]
async fn watch_rejects_an_ancestor_target_before_waiting() {
    let (_dir, shared) = test_shared();
    {
        let mut journal = shared.journal.lock().expect("journal");
        journal.insert("root".to_string(), record("root", None));
        journal.insert("child".to_string(), record("child", Some("root")));
    }
    let error = watch_task(&shared, Some("child"), "root", 0, Duration::from_millis(1))
        .await
        .expect_err("watching an ancestor is an unsafe wait edge");
    assert!(error.contains("ancestor"), "{error}");
}

#[tokio::test]
async fn watch_returns_terminal_ancestor_without_a_wait_edge() {
    let (_dir, shared) = test_shared();
    let mut root = record("root", None);
    root.state = DurableState::Succeeded {
        result: "already done".to_string(),
    };
    {
        let mut journal = shared.journal.lock().expect("journal");
        journal.insert("root".to_string(), root);
        journal.insert("child".to_string(), record("child", Some("root")));
    }

    let snapshot = watch_task(&shared, Some("child"), "root", 0, Duration::from_millis(1))
        .await
        .expect("terminal snapshot does not require a live wait edge");
    assert_eq!(snapshot["terminal"], true);
    assert_eq!(snapshot["result"]["result"], "already done");
}

#[test]
fn duration_from_ms_clamps_untrusted_timeout() {
    assert_eq!(duration_from_ms(Some(u64::MAX)), MAX_WAIT);
    assert_eq!(duration_from_ms(None), DEFAULT_WAIT);
}

#[test]
fn successful_parent_finalizes_only_after_attached_child() {
    let mut journal = Journal::new();
    journal.insert("root".to_string(), record("root", None));
    journal.insert("child".to_string(), record("child", Some("root")));

    let parent = apply_completion(
        &mut journal,
        "root",
        PendingTerminal::Succeeded {
            result: "parent".to_string(),
        },
    )
    .expect("parent completion");
    assert!(parent.finalized.is_empty());
    assert!(matches!(journal["root"].state, DurableState::Running));
    assert!(!journal["root"].accepts_work());

    let child = apply_completion(
        &mut journal,
        "child",
        PendingTerminal::Succeeded {
            result: "child".to_string(),
        },
    )
    .expect("child completion");
    assert_eq!(child.finalized, ["child", "root"]);
    assert!(matches!(
        journal["child"].state,
        DurableState::Succeeded { ref result } if result == "child"
    ));
    assert!(matches!(
        journal["root"].state,
        DurableState::Succeeded { ref result } if result == "parent"
    ));
}

#[test]
fn pending_parent_preserves_live_wait_edges_until_finalization() {
    let (_dir, shared) = test_shared();
    {
        let mut journal = shared.journal.lock().expect("journal");
        journal.insert("root".to_string(), record("root", None));
        journal.insert("child".to_string(), record("child", Some("root")));
        journal.insert("other".to_string(), record("other", None));
    }
    let _lease = begin_wait(&shared, Some("other"), "root").expect("initial wait edge");
    let effects = {
        let mut journal = shared.journal.lock().expect("journal");
        apply_completion(
            &mut journal,
            "root",
            PendingTerminal::Succeeded {
                result: "parent".to_string(),
            },
        )
        .expect("parent worker completion")
    };
    assert!(effects.finalized.is_empty());

    transition_controls(&shared, Some("root"), &effects);

    let error = begin_wait(&shared, Some("root"), "other")
        .err()
        .expect("the reciprocal edge must still be rejected");
    assert!(error.contains("cycle"), "{error}");
}

#[test]
fn failed_parent_cancels_children_and_waits_for_them() {
    let mut journal = Journal::new();
    journal.insert("root".to_string(), record("root", None));
    journal.insert("child".to_string(), record("child", Some("root")));

    let parent = apply_completion(
        &mut journal,
        "root",
        PendingTerminal::Failed {
            error: "boom".to_string(),
        },
    )
    .expect("parent completion");
    assert_eq!(parent.cancel, ["child"]);
    assert!(parent.finalized.is_empty());
    assert!(journal["child"].cancel_requested);
    assert!(matches!(journal["root"].state, DurableState::Running));

    let child = apply_completion(&mut journal, "child", PendingTerminal::Cancelled)
        .expect("child cancellation");
    assert_eq!(child.finalized, ["child", "root"]);
    assert!(matches!(journal["child"].state, DurableState::Cancelled));
    assert!(matches!(
        journal["root"].state,
        DurableState::Failed { ref error } if error == "boom"
    ));
}

#[test]
fn detached_work_does_not_hold_or_inherit_parent_scope() {
    let mut journal = Journal::new();
    journal.insert("root".to_string(), record("root", None));
    let mut detached = record("detached", Some("root"));
    detached.detached = true;
    journal.insert("detached".to_string(), detached);

    let parent = apply_completion(
        &mut journal,
        "root",
        PendingTerminal::Succeeded {
            result: "done".to_string(),
        },
    )
    .expect("parent completion");
    assert_eq!(parent.finalized, ["root"]);
    assert!(matches!(journal["detached"].state, DurableState::Running));

    let cancelled = request_cancel_tree_locked(&mut journal, "root", true)
        .expect("cancel terminal root is harmless");
    assert!(cancelled.is_empty());
    assert!(!journal["detached"].cancel_requested);
}

#[tokio::test]
async fn each_message_keeps_its_own_reply() {
    let (_dir, shared) = test_shared();
    let mut task = record("task", None);
    let first = task
        .add_message("first".to_string())
        .expect("first message");
    task.start_next_message().expect("first in flight");
    shared
        .journal
        .lock()
        .expect("journal")
        .insert("task".to_string(), task);

    settle_or_take_message(
        &shared,
        "task",
        Some(&first),
        "first reply".to_string(),
        None,
    )
    .await
    .expect("settle first reply");

    let payload = message_payload_for_dispatch(&shared, "task", &first)
        .expect("message lookup")
        .expect("reply is durable");
    assert_eq!(payload["message"], first);
    assert_eq!(payload["result"], "first reply");
    assert_eq!(payload["state"], "succeeded");
}

#[tokio::test]
async fn queued_messages_keep_distinct_replies_before_task_completion() {
    let (_dir, shared) = test_shared();
    let mut task = record("task", None);
    let first = task
        .add_message("first".to_string())
        .expect("first message");
    let second = task
        .add_message("second".to_string())
        .expect("second message");
    task.start_next_message().expect("first in flight");
    shared
        .journal
        .lock()
        .expect("journal")
        .insert("task".to_string(), task);

    let next = settle_or_take_message(&shared, "task", Some(&first), "reply one".to_string(), None)
        .await
        .expect("first turn")
        .expect("second turn is queued");
    assert_eq!(next.0, second);
    let next = settle_or_take_message(
        &shared,
        "task",
        Some(&second),
        "reply two".to_string(),
        None,
    )
    .await
    .expect("second turn");
    assert!(next.is_none());

    assert_eq!(
        message_payload_for_dispatch(&shared, "task", &first)
            .expect("first lookup")
            .expect("first reply")["result"],
        "reply one"
    );
    assert_eq!(
        message_payload_for_dispatch(&shared, "task", &second)
            .expect("second lookup")
            .expect("second reply")["result"],
        "reply two"
    );
}

#[tokio::test]
async fn terminal_failure_resolves_unanswered_messages() {
    let (_dir, shared) = test_shared();
    let mut task = record("task", None);
    let first = task
        .add_message("first".to_string())
        .expect("first message");
    let second = task
        .add_message("second".to_string())
        .expect("second message");
    task.start_next_message().expect("first in flight");
    shared
        .journal
        .lock()
        .expect("journal")
        .insert("task".to_string(), task);

    finish_failed(&shared, "task", "provider failed".to_string(), None).await;
    for id in [first, second] {
        let payload = message_payload_for_dispatch(&shared, "task", &id)
            .expect("message lookup")
            .expect("terminal result resolves message");
        assert_eq!(payload["state"], "failed");
        assert_eq!(payload["message"], id);
    }
}

#[tokio::test]
async fn daemon_shutdown_resolves_unanswered_messages_as_orphaned() {
    let (_dir, shared) = test_shared();
    let mut task = record("task", None);
    let in_flight = task
        .add_message("in flight".to_string())
        .expect("in-flight message");
    let pending = task
        .add_message("pending".to_string())
        .expect("pending message");
    task.start_next_message().expect("start first message");
    shared
        .journal
        .lock()
        .expect("journal")
        .insert("task".to_string(), task);

    orphan_running(&shared).await;

    for id in [in_flight, pending] {
        let payload = message_payload_for_dispatch(&shared, "task", &id)
            .expect("message lookup")
            .expect("orphan terminal resolves message wait");
        assert_eq!(payload["state"], "orphaned");
        assert_eq!(payload["message"], id);
    }
}

#[tokio::test]
async fn daemon_shutdown_releases_every_live_wait_edge() {
    let (_dir, shared) = test_shared();
    {
        let mut journal = shared.journal.lock().expect("journal");
        journal.insert("left".to_string(), record("left", None));
        journal.insert("right".to_string(), record("right", None));
    }
    let lease = begin_wait(&shared, Some("left"), "right").expect("initial wait edge");
    assert!(!shared.waits.lock().expect("graph").edges.is_empty());

    orphan_running(&shared).await;

    assert!(shared.waits.lock().expect("graph").edges.is_empty());
    drop(lease);
}

#[tokio::test]
async fn settle_cleans_up_controls_and_waiters_when_persist_fails() {
    let (dir, shared) = test_shared();
    let mut root = record("root", None);
    root.cancel_requested = true;
    let child = record("child", Some("root"));
    {
        let mut journal = shared.journal.lock().expect("journal");
        journal.insert(root.id.clone(), root);
        journal.insert(child.id.clone(), child);
    }
    let child_token = CancellationToken::default();
    shared
        .controls
        .lock()
        .expect("controls")
        .insert("child".to_string(), child_token.clone());
    let updates = shared.changed.subscribe();
    std::fs::remove_dir_all(dir.path().join("registry")).expect("remove registry");

    let error = settle_or_take_message(&shared, "root", None, "ignored".to_string(), None)
        .await
        .expect_err("missing registry makes persistence fail");
    assert!(error.contains("persist task journal"), "{error}");
    assert!(child_token.is_cancelled());
    assert!(updates.has_changed().expect("change notification"));
}

#[tokio::test]
async fn cancel_cleans_up_controls_and_waiters_when_persist_fails() {
    let (dir, shared) = test_shared();
    let task = record("task", None);
    shared
        .journal
        .lock()
        .expect("journal")
        .insert(task.id.clone(), task);
    let token = CancellationToken::default();
    shared
        .controls
        .lock()
        .expect("controls")
        .insert("task".to_string(), token.clone());
    let updates = shared.changed.subscribe();
    std::fs::remove_dir_all(dir.path().join("registry")).expect("remove registry");

    let error = cancel_task(&shared, None, "task")
        .await
        .expect_err("missing registry makes persistence fail");
    assert!(error.contains("persist task journal"), "{error}");
    assert!(token.is_cancelled());
    assert!(updates.has_changed().expect("change notification"));
}
