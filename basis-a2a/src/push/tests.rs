use a2a::{TaskState, TaskStatus, TaskStatusUpdateEvent};

use super::*;

const TASK: &str = "0123456789abcdef0123456789abcdef";

fn config(id: &str) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        url: format!("https://example.com/{id}"),
        id: Some(id.to_string()),
        task_id: TASK.to_string(),
        token: None,
        authentication: None,
        tenant: None,
    }
}

fn update(state: TaskState) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: TASK.to_string(),
        context_id: "ctx".to_string(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        metadata: None,
    })
}

#[test]
fn a_change_is_pending_for_every_config_until_each_has_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = PushStore::open(dir.path()).unwrap();
    store
        .upsert(TASK, config("a"), Some("submitted".into()))
        .unwrap();
    store.upsert(TASK, config("b"), None).unwrap();
    assert!(
        !store
            .record(TASK, "submitted".into(), update(TaskState::Submitted))
            .unwrap(),
        "the status a config was created at is no change"
    );
    assert!(
        store
            .record(TASK, "working".into(), update(TaskState::Working))
            .unwrap()
    );
    assert_eq!(store.pending(TASK).len(), 2);

    store.delivered(TASK, "a", 1).unwrap();
    let pending = store.pending(TASK);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0.id.as_deref(), Some("b"));
}

#[test]
fn what_was_not_delivered_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = PushStore::open(dir.path()).unwrap();
        store
            .upsert(TASK, config("a"), Some("working".into()))
            .unwrap();
        store
            .record(TASK, "completed".into(), update(TaskState::Completed))
            .unwrap();
    }
    let reopened = PushStore::open(dir.path()).unwrap();
    let pending = reopened.pending(TASK);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].1.seq, 1);
    assert_eq!(reopened.last_status(TASK).as_deref(), Some("completed"));
    assert_eq!(reopened.tasks_with_configs(), vec![TASK.to_string()]);
}

#[test]
fn a_config_hears_only_of_changes_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = PushStore::open(dir.path()).unwrap();
    store
        .upsert(TASK, config("a"), Some("submitted".into()))
        .unwrap();
    store
        .record(TASK, "working".into(), update(TaskState::Working))
        .unwrap();
    store.upsert(TASK, config("late"), None).unwrap();
    let pending = store.pending(TASK);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0.id.as_deref(), Some("a"));
}

#[test]
fn configs_are_bounded_replaced_by_id_and_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = PushStore::open(dir.path()).unwrap();
    for index in 0..MAX_CONFIGS {
        assert!(
            store
                .upsert(TASK, config(&index.to_string()), None)
                .unwrap()
                .is_some()
        );
    }
    assert!(
        store
            .upsert(TASK, config("one-too-many"), None)
            .unwrap()
            .is_none()
    );
    let mut replaced = config("0");
    replaced.url = "https://example.com/elsewhere".into();
    assert!(store.upsert(TASK, replaced, None).unwrap().is_some());
    assert_eq!(
        store.get(TASK, "0").unwrap().url,
        "https://example.com/elsewhere"
    );
    assert!(store.delete(TASK, "0").unwrap());
    assert!(!store.delete(TASK, "0").unwrap());
    assert_eq!(store.list(TASK).len(), MAX_CONFIGS - 1);
}

#[test]
fn a_change_with_nobody_to_tell_is_not_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let store = PushStore::open(dir.path()).unwrap();
    assert!(
        !store
            .record(TASK, "working".into(), update(TaskState::Working))
            .unwrap()
    );
    assert!(store.pending(TASK).is_empty());
}
