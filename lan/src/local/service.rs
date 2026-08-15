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
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, Semaphore, watch},
    time,
};

use self::{
    lifecycle::{
        WaitGraph, accepted_payload, await_message, await_task, begin_wait, cancel_task,
        duration_from_ms, enqueue_message, inbox, orphan_running, send_next_hint, watch_task,
    },
    task::{SpawnRequest, spawn_task},
};
use super::{
    protocol::{Operation, Request, VERSION, error, ok, read_frame, write_frame},
    registry::{
        Descriptor, Registry, canonical_workspace, new_token, workspace_key, write_descriptor,
    },
    store::{self, Journal},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 128;

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
    waits: Arc<Mutex<WaitGraph>>,
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
        waits: Arc::new(Mutex::new(WaitGraph::default())),
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
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|error| format!("accept lan client: {error}"))?;
                let Ok(permit) = connections.clone().try_acquire_owned() else {
                    // A bounded control plane must fail closed under load. The
                    // client can retry; no handler is allowed to consume an
                    // unbounded task or socket slot for thirty minutes.
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _permit = permit;
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
    let (mut reader, mut writer) = stream.into_split();
    let response = if request.version != VERSION {
        error(
            id,
            format!("unsupported local protocol version {}", request.version),
        )
    } else if request.token != shared.descriptor.token {
        error(id, "invalid lan service capability")
    } else {
        // Keep the request future inline rather than detaching it. If the CLI
        // disappears while waiting, EOF drops the future and its WaitLease;
        // a cancelled client cannot retain a wait-graph edge for 30 minutes.
        let dispatch = dispatch(request.operation, &shared);
        tokio::pin!(dispatch);
        let mut eof_probe = [0_u8; 1];
        let result = tokio::select! {
            biased;
            result = &mut dispatch => Some(result),
            read = reader.read(&mut eof_probe) => {
                match read {
                    Ok(0) | Err(_) => None,
                    Ok(_) => None,
                }
            }
        };
        let Some(result) = result else {
            return Ok(());
        };
        match result {
            Ok(payload) => ok(id, payload),
            Err(message) => error(id, message),
        }
    };
    write_frame(&mut writer, &response)
        .await
        .map_err(|error| format!("write lan response: {error}"))
}

async fn dispatch(operation: Operation, shared: &Shared) -> Result<Value, String> {
    match operation {
        Operation::Spawn {
            workspace,
            prompt,
            parent,
            caller,
            detached,
            await_result,
            timeout_ms,
            options,
        } => {
            let (task, lease) = spawn_task(
                shared,
                SpawnRequest {
                    workspace,
                    prompt,
                    parent,
                    caller,
                    detached,
                    await_result,
                    options: *options,
                },
            )
            .await?;
            if await_result {
                await_task(shared, &task, duration_from_ms(timeout_ms), lease).await
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
            let lease = if await_result {
                Some(begin_wait(shared, caller.as_deref(), &task)?)
            } else {
                None
            };
            let message_id = enqueue_message(shared, &task, message).await?;
            if await_result {
                await_message(
                    shared,
                    &task,
                    &message_id,
                    duration_from_ms(timeout_ms),
                    lease.expect("await lease"),
                )
                .await
            } else {
                Ok(json!({
                    "task": task,
                    "message": message_id,
                    "state": "accepted",
                    "next": send_next_hint(shared, caller.as_deref(), &task, &message_id),
                }))
            }
        }
        Operation::Wait {
            task,
            caller,
            message,
            timeout_ms,
        } => {
            if let Some(message) = message {
                if let Some(payload) =
                    lifecycle::message_payload_for_dispatch(shared, &task, &message)?
                {
                    return Ok(payload);
                }
                let lease = begin_wait(shared, caller.as_deref(), &task)?;
                await_message(
                    shared,
                    &task,
                    &message,
                    duration_from_ms(Some(timeout_ms)),
                    lease,
                )
                .await
            } else {
                if let Some(payload) = lifecycle::terminal_payload(shared, &task)? {
                    return Ok(payload);
                }
                let lease = begin_wait(shared, caller.as_deref(), &task)?;
                await_task(shared, &task, duration_from_ms(Some(timeout_ms)), lease).await
            }
        }
        Operation::Cancel { task, caller } => cancel_task(shared, caller.as_deref(), &task).await,
        Operation::Watch {
            task,
            caller,
            since,
            timeout_ms,
        } => {
            watch_task(
                shared,
                caller.as_deref(),
                &task,
                since,
                duration_from_ms(Some(timeout_ms)),
            )
            .await
        }
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

    /// Shared with `task`'s tests, which need the same registry-backed service
    /// state; the `TempDir` is returned because dropping it deletes the
    /// registry underneath the `Shared`.
    pub(super) fn test_shared() -> (TempDir, Shared) {
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
        assert_eq!(
            payload["next"], "lan inbox root",
            "a child must not be told to wait on its ancestor"
        );

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
    async fn wait_returns_terminal_result_before_validating_an_upward_edge() {
        let (_dir, shared) = test_shared();
        let mut root = record("root", None);
        root.state = crate::local::store::DurableState::Succeeded {
            result: "already done".to_string(),
        };
        {
            let mut journal = shared.journal.lock().expect("journal");
            journal.insert("root".to_string(), root);
            journal.insert("child".to_string(), record("child", Some("root")));
        }

        let payload = dispatch(
            Operation::Wait {
                task: "root".to_string(),
                caller: Some("child".to_string()),
                message: None,
                timeout_ms: 1,
            },
            &shared,
        )
        .await
        .expect("terminal result does not require a live wait edge");
        assert_eq!(payload["state"], "succeeded");
        assert_eq!(payload["result"], "already done");
    }

    #[test]
    fn independent_wait_cycle_is_rejected_until_the_first_lease_drops() {
        let (_dir, shared) = test_shared();
        {
            let mut journal = shared.journal.lock().expect("journal");
            journal.insert("left".to_string(), record("left", None));
            journal.insert("right".to_string(), record("right", None));
        }

        let first = begin_wait(&shared, Some("left"), "right").expect("first wait edge");
        let error = begin_wait(&shared, Some("right"), "left")
            .err()
            .expect("the reverse edge would deadlock");
        assert!(error.contains("cycle"), "{error}");

        drop(first);
        let reverse =
            begin_wait(&shared, Some("right"), "left").expect("a completed wait releases its edge");
        drop(reverse);
    }

    #[tokio::test]
    async fn detached_spawn_keeps_ownership_separate_from_its_waiting_caller() {
        let (_dir, shared) = test_shared();
        shared
            .journal
            .lock()
            .expect("journal")
            .insert("caller".to_string(), record("caller", None));
        let options = crate::local::protocol::RunOptions {
            provider: Some("not-a-provider".to_string()),
            approve: "never".to_string(),
            ..crate::local::protocol::RunOptions::default()
        };

        let (task, lease) = spawn_task(
            &shared,
            SpawnRequest {
                workspace: shared.workspace.to_string_lossy().into_owned(),
                prompt: "do not run".to_string(),
                parent: None,
                caller: Some("caller".to_string()),
                detached: true,
                await_result: true,
                options,
            },
        )
        .await
        .expect("spawn detached task with an awaited caller edge");

        {
            let journal = shared.journal.lock().expect("journal");
            assert_eq!(journal[&task].parent, None);
            assert!(journal[&task].detached);
        }
        let reverse = begin_wait(&shared, Some(&task), "caller")
            .err()
            .expect("the detached caller edge participates in cycle prevention");
        assert!(reverse.contains("cycle"), "{reverse}");
        drop(lease);
        let released = begin_wait(&shared, Some(&task), "caller")
            .expect("dropping the detached caller wait releases its edge");
        drop(released);
    }

    #[tokio::test]
    async fn cancellation_authority_only_flows_down_the_attached_tree() {
        let (_dir, shared) = test_shared();
        {
            let mut journal = shared.journal.lock().expect("journal");
            journal.insert("root".to_string(), record("root", None));
            journal.insert("child".to_string(), record("child", Some("root")));
            journal.insert("other".to_string(), record("other", None));
        }

        let upward = dispatch(
            Operation::Cancel {
                task: "root".to_string(),
                caller: Some("child".to_string()),
            },
            &shared,
        )
        .await
        .expect_err("a child cannot cancel its owner");
        assert!(upward.contains("ancestor"), "{upward}");

        let sideways = dispatch(
            Operation::Cancel {
                task: "other".to_string(),
                caller: Some("root".to_string()),
            },
            &shared,
        )
        .await
        .expect_err("an attached task cannot cancel an independent root");
        assert!(sideways.contains("peer"), "{sideways}");
        {
            let journal = shared.journal.lock().expect("journal");
            assert!(
                !journal["root"].cancel_requested,
                "rejected upward cancellation must not mutate the target"
            );
            assert!(
                !journal["other"].cancel_requested,
                "rejected peer cancellation must not mutate the target"
            );
        }

        let accepted = dispatch(
            Operation::Cancel {
                task: "child".to_string(),
                caller: Some("root".to_string()),
            },
            &shared,
        )
        .await
        .expect("an owner can cancel its child");
        assert_eq!(accepted["state"], "cancel_requested");
        assert!(shared.journal.lock().expect("journal")["child"].cancel_requested);
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
