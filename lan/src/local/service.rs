//! The long-lived owner behind asynchronous lifecycle commands.
//!
//! One service owns one canonical workspace. Clients submit bounded JSON
//! requests over loopback TCP, but neither sockets nor serialization leak into
//! `lan-core`: this is an adapter owned by the binary. Task graph mutations are
//! short synchronous critical sections; model work, disk writes, and client
//! waits happen outside them, so the control plane remains responsive.

mod lifecycle;
mod task;

use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use lan_core::CancellationToken;
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, watch},
    time,
};

use self::{
    lifecycle::{
        accepted_payload, await_task, cancel_task, duration_from_ms, enqueue_message, inbox,
        orphan_running, task_parent, validate_wait_edge, watch_task,
    },
    task::spawn_task,
};
use super::{
    protocol::{Operation, Request, VERSION, error, ok, read_frame, write_frame},
    registry::{
        Descriptor, Registry, canonical_workspace, new_token, workspace_key, write_descriptor,
    },
    store::{self, Journal},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

fn notify_changed(shared: &Shared) {
    shared
        .changed
        .send_modify(|version| *version = version.saturating_add(1));
}

#[derive(Clone)]
struct Shared {
    registry: Registry,
    descriptor: Descriptor,
    workspace: PathBuf,
    journal: Arc<Mutex<Journal>>,
    controls: Arc<Mutex<HashMap<String, CancellationToken>>>,
    persist_gate: Arc<AsyncMutex<()>>,
    changed: watch::Sender<u64>,
}

/// Runs the hidden per-workspace service. A filesystem lease selects exactly
/// one owner, while the actual operations remain capability-scoped.
pub(crate) async fn run_daemon(workspace: PathBuf, registry: PathBuf) -> Result<(), String> {
    let workspace = canonical_workspace(&workspace)
        .map_err(|error| format!("resolve workspace {}: {error}", workspace.display()))?;
    let registry = Registry::from_path(registry)
        .map_err(|error| format!("open lan service registry: {error}"))?;
    let _reservation = match registry.acquire(&workspace) {
        Ok(reservation) => reservation,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(format!("reserve lan service: {error}")),
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("bind lan service: {error}"))?;
    let instance = workspace_key(&workspace);
    let descriptor = Descriptor {
        version: VERSION,
        instance: instance.clone(),
        workspace: workspace.to_string_lossy().into_owned(),
        endpoint: listener
            .local_addr()
            .map_err(|error| format!("read lan service address: {error}"))?
            .to_string(),
        token: new_token(),
        pid: std::process::id(),
    };
    let journal =
        store::load(&registry, &instance).map_err(|error| format!("load task journal: {error}"))?;
    let (changed, _) = watch::channel(0_u64);
    let shared = Shared {
        registry: registry.clone(),
        descriptor: descriptor.clone(),
        workspace: workspace.clone(),
        journal: Arc::new(Mutex::new(journal)),
        controls: Arc::new(Mutex::new(HashMap::new())),
        persist_gate: Arc::new(AsyncMutex::new(())),
        changed,
    };

    write_descriptor(&registry.workspace_descriptor(&workspace), &descriptor)
        .map_err(|error| format!("publish lan service: {error}"))?;
    write_descriptor(&registry.instance_descriptor(&instance), &descriptor)
        .map_err(|error| format!("publish lan service handle: {error}"))?;

    let result = accept_loop(listener, shared.clone()).await;
    orphan_running(&shared).await;
    let _ = registry.remove_descriptor(&descriptor);
    result
}

async fn accept_loop(listener: TcpListener, shared: Shared) -> Result<(), String> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|error| format!("accept lan client: {error}"))?;
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, shared).await;
                });
            }
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| format!("listen for shutdown: {error}"))?;
                return Ok(());
            }
        }
    }
}

async fn handle_connection(mut stream: TcpStream, shared: Shared) -> Result<(), String> {
    let request = time::timeout(HANDSHAKE_TIMEOUT, read_frame::<_, Request>(&mut stream))
        .await
        .map_err(|_| "lan client did not finish its request in time".to_string())?
        .map_err(|error| format!("read lan request: {error}"))?;
    let id = request.id;
    let response = if request.version != VERSION {
        error(
            id,
            format!("unsupported local protocol version {}", request.version),
        )
    } else if request.token != shared.descriptor.token {
        error(id, "invalid lan service capability")
    } else {
        match dispatch(request.operation, &shared).await {
            Ok(payload) => ok(id, payload),
            Err(message) => error(id, message),
        }
    };
    write_frame(&mut stream, &response)
        .await
        .map_err(|error| format!("write lan response: {error}"))
}

async fn dispatch(operation: Operation, shared: &Shared) -> Result<Value, String> {
    match operation {
        Operation::Spawn {
            workspace,
            prompt,
            parent,
            detached,
            await_result,
            timeout_ms,
            options,
        } => {
            let task = spawn_task(shared, workspace, prompt, parent, detached, options).await?;
            if await_result {
                await_task(
                    shared,
                    &task,
                    task_parent(shared, &task)?,
                    duration_from_ms(timeout_ms),
                )
                .await
            } else {
                Ok(accepted_payload(&task))
            }
        }
        Operation::Send {
            task,
            message,
            caller,
            await_result,
            timeout_ms,
        } => {
            if await_result {
                validate_wait_edge(shared, caller.as_deref(), &task)?;
            }
            let message_id = enqueue_message(shared, &task, message).await?;
            if await_result {
                await_task(shared, &task, caller, duration_from_ms(timeout_ms)).await
            } else {
                Ok(json!({
                    "task": task,
                    "message": message_id,
                    "state": "accepted",
                    "next": format!("lan wait {task}"),
                }))
            }
        }
        Operation::Wait {
            task,
            caller,
            timeout_ms,
        } => {
            validate_wait_edge(shared, caller.as_deref(), &task)?;
            await_task(shared, &task, caller, Duration::from_millis(timeout_ms)).await
        }
        Operation::Cancel { task } => cancel_task(shared, &task).await,
        Operation::Watch {
            task,
            since,
            timeout_ms,
        } => watch_task(shared, &task, since, Duration::from_millis(timeout_ms)).await,
        Operation::Inbox { task } if task == "__probe__" => Ok(json!({"state": "ready"})),
        Operation::Inbox { task } => inbox(shared, &task),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::{
        protocol::{Response, ResponseKind, write_frame},
        store::TaskRecord,
    };
    use tempfile::TempDir;

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
        };
        (dir, shared)
    }

    #[tokio::test]
    async fn enqueue_only_send_to_an_ancestor_does_not_add_a_wait_edge() {
        let (_dir, shared) = test_shared();
        {
            let mut journal = shared.journal.lock().expect("journal");
            journal.insert("root".to_string(), record("root", None));
            journal.insert("child".to_string(), record("child", Some("root")));
        }

        let payload = dispatch(
            Operation::Send {
                task: "root".to_string(),
                message: "status update".to_string(),
                caller: Some("child".to_string()),
                await_result: false,
                timeout_ms: None,
            },
            &shared,
        )
        .await
        .expect("enqueue-only parent send is safe");
        assert_eq!(payload["state"], "accepted");

        let error = dispatch(
            Operation::Send {
                task: "root".to_string(),
                message: "blocking question".to_string(),
                caller: Some("child".to_string()),
                await_result: true,
                timeout_ms: Some(1),
            },
            &shared,
        )
        .await
        .expect_err("awaiting an ancestor would close the wait cycle");
        assert!(error.contains("ancestor"), "{error}");
    }

    #[tokio::test]
    async fn capability_is_checked_before_any_operation_mutates_state() {
        let (_dir, shared) = test_shared();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let descriptor = Descriptor {
            endpoint: listener.local_addr().expect("address").to_string(),
            token: "right-token".to_string(),
            ..shared.descriptor.clone()
        };
        let shared = Shared {
            descriptor: descriptor.clone(),
            ..shared
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            handle_connection(stream, shared.clone())
                .await
                .expect("respond");
            assert!(
                shared.journal.lock().expect("journal").is_empty(),
                "an unauthenticated request must not reach dispatch"
            );
        });

        let mut stream = TcpStream::connect(&descriptor.endpoint)
            .await
            .expect("connect");
        write_frame(
            &mut stream,
            &Request {
                version: VERSION,
                id: 7,
                token: "wrong-token".to_string(),
                operation: Operation::Inbox {
                    task: "__probe__".to_string(),
                },
            },
        )
        .await
        .expect("request");
        let response: Response = read_frame(&mut stream).await.expect("response");
        assert!(matches!(response.kind, ResponseKind::Error));
        assert_eq!(response.id, 7);
        server.await.expect("server joins");
    }
}
