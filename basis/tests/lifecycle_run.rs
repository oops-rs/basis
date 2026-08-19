//! A prepared agent run under the generic lifecycle supervisor.
//!
//! The lifecycle unit tests prove ownership mechanics with small futures. This
//! suite crosses the actual run boundary: a Mentra session is spawned, waited
//! on, and cancelled through a `TaskHandle`, with no provider or network.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use basis::{
    AllowAll, ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, CollectingSink,
    ContextConfig, Event, FnSink, RunConfig, Supervisor, TaskState, approval::ApprovalGate,
    run::prepare_with_session,
};
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};
use tokio::sync::oneshot;

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "be concise").expect("write context");
    dir
}

fn config(workspace: &Path, prompt: &str) -> RunConfig {
    RunConfig::new(workspace, prompt).with_context(ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

fn prepared(mock: &MockRuntime, workspace: &Path, prompt: &str) -> basis::PreparedRun {
    let session = mock
        .runtime()
        .create_session_with_config(
            "test",
            mock.model(),
            mentra::agent::AgentConfig {
                workspace: mentra::agent::WorkspaceConfig {
                    base_dir: workspace.to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session");

    prepare_with_session(session, &config(workspace, prompt), "openai", "mock-model")
        .expect("prepared")
}

#[tokio::test]
async fn a_prepared_run_spawns_and_returns_its_final_message() {
    let workspace = workspace();
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .stream_text(["hello", " world"])
        .build()
        .expect("mock runtime builds");
    let supervisor = Supervisor::new();

    let task = prepared(&mock, workspace.path(), "say hello")
        .spawn(&supervisor, None, false, CollectingSink::new(), AllowAll)
        .await
        .expect("run spawns");

    assert_eq!(
        task.wait(Duration::from_secs(1)).await.expect("finishes"),
        TaskState::Succeeded(b"hello world".to_vec())
    );
}

#[tokio::test]
async fn a_prepared_run_can_be_an_attached_child() {
    let workspace = workspace();
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .text("child done")
        .build()
        .expect("mock runtime builds");
    let supervisor = Supervisor::new();
    let (release_parent, parent_gate) = oneshot::channel();
    let parent = supervisor
        .spawn(None, false, move |_context| async move {
            parent_gate
                .await
                .map_err(|_| "parent gate closed".to_string())?;
            Ok(b"parent done".to_vec())
        })
        .await
        .expect("parent spawns");

    let child = prepared(&mock, workspace.path(), "finish the child")
        .spawn(
            &supervisor,
            Some(&parent),
            false,
            CollectingSink::new(),
            AllowAll,
        )
        .await
        .expect("child attaches");

    assert_eq!(
        child.wait(Duration::from_secs(1)).await.expect("finishes"),
        TaskState::Succeeded(b"child done".to_vec())
    );
    release_parent.send(()).expect("parent is waiting");
    assert_eq!(
        parent
            .wait(Duration::from_secs(1))
            .await
            .expect("parent finishes"),
        TaskState::Succeeded(b"parent done".to_vec())
    );
}

struct HeldApproval {
    started: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
}

#[async_trait]
impl Approver for HeldApproval {
    async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
        self.started
            .take()
            .expect("asked once")
            .send(())
            .expect("test is waiting");
        let _ = self.release.take().expect("one release").await;
        ApprovalDecision::Allow.into()
    }
}

#[tokio::test]
async fn cancelling_a_spawned_run_closes_its_event_stream() {
    let workspace = workspace();
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(RuntimePolicy::permissive())
        .with_tool_authorizer(ApprovalGate::new())
        .tool_calls([MockToolCall::new(
            "files",
            serde_json::json!({
                "operations": [{"op": "create", "path": "made.txt", "content": "hi"}]
            }),
        )])
        .text("done")
        .build()
        .expect("mock runtime builds");
    let supervisor = Supervisor::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let sink = FnSink::new(move |event| {
        observed.lock().expect("not poisoned").push(event);
        Ok(())
    });
    let (approval_started, approval_seen) = oneshot::channel();
    let (release_approval, approval_gate) = oneshot::channel();

    let task = prepared(&mock, workspace.path(), "make a file")
        .spawn(
            &supervisor,
            None,
            false,
            sink,
            HeldApproval {
                started: Some(approval_started),
                release: Some(approval_gate),
            },
        )
        .await
        .expect("run spawns");

    approval_seen.await.expect("run reaches approval");
    task.cancel().await.expect("cancellation accepted");
    release_approval.send(()).expect("approver is waiting");

    assert_eq!(
        task.wait(Duration::from_secs(1)).await.expect("settles"),
        TaskState::Cancelled
    );
    assert!(
        matches!(
            events.lock().expect("not poisoned").last(),
            Some(Event::RunFinished { .. })
        ),
        "cooperative cancellation must close the independent event path"
    );
}
