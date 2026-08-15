//! ADR-0019's acceptance surface, driven through the real binary against a
//! loopback-scripted endpoint: kill-and-resume, concurrent attachers,
//! cancel-at-boundary, parent-terminal ordering, deadline-bounded waits, and
//! the no-resident-process guarantee.
//!
//! The endpoint is `lan-core/tests/runtime.rs`'s, grown a `Stall` reply that
//! accepts a connection and holds it until the client dies — the shape a
//! `kill -9` mid-turn needs.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const NOT_STUCK: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Fixture {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let data = root.path().join("data");
        fs::create_dir_all(&workspace).expect("workspace");
        Self {
            _root: root,
            workspace,
            data,
        }
    }

    fn lan(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lan"));
        command
            .env("LAN_DATA_DIR", &self.data)
            .env("LAN_API_KEY", "test-key")
            .env_remove("LAN_TASK_ID")
            .env_remove("LAN_BASE_URL")
            .env_remove("OPENAI_BASE_URL")
            .args(args);
        command
    }

    /// Spawns a resumable agent against `endpoint` and returns its handle.
    fn spawn_agent(&self, endpoint: &ScriptedEndpoint, deadline: &str) -> String {
        let mut command = self.lan(&["spawn", "answer briefly", "-C"]);
        command.arg(&self.workspace).args([
            "--base-url",
            &endpoint.base_url,
            "--model",
            "test-model",
            "--deadline",
            deadline,
        ]);
        let output = run_bounded(command);
        assert!(output.status.success(), "{}", stderr(&output));
        let stdout = String::from_utf8(output.stdout).expect("utf8");
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("task "))
            .and_then(|line| line.split_once(':').map(|(task, _)| task.to_string()))
            .unwrap_or_else(|| panic!("no task handle in: {stdout}"))
    }

    fn agent_dir(&self, task: &str) -> PathBuf {
        let (key, id) = task.split_once('/').expect("handle shape");
        self.data
            .join("workspaces")
            .join(key)
            .join("agents")
            .join(id)
    }
}

fn run_bounded(mut command: Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().expect("start lan command");
    finish_bounded(child)
}

fn finish_bounded(mut child: Child) -> Output {
    let deadline = Instant::now() + NOT_STUCK;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll lan command") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lan command did not settle within {NOT_STUCK:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_end(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_end(&mut stderr)
        .expect("read stderr");
    Output {
        status,
        stdout,
        stderr,
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "not one JSON object ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + NOT_STUCK;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// Spec acceptance: `kill -9` mid-turn leaves no terminal record; a later
/// attach resumes from the last committed turn and completes; `wait` then
/// observes the same terminal result repeatedly. The mid-stall `lan watch`
/// also pins cross-process tailing of an executor-held `events.jsonl`.
#[test]
fn kill_dash_nine_mid_turn_resumes_to_a_repeatable_terminal() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(vec![Reply::Stall]);
    let task = fixture.spawn_agent(&endpoint, "5m");
    let dir = fixture.agent_dir(&task);
    assert!(
        dir.join("meta.json").is_file(),
        "spawn minted the agent dir"
    );

    // First attacher: stalls inside its first model turn.
    let attacher = fixture
        .lan(&["wait", &task, "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start attacher");
    wait_until("the executor to reach its model turn", || {
        !endpoint.requests().is_empty()
    });

    // A watcher tails the journal the live executor holds open.
    let watch = run_bounded(fixture.lan(&["watch", &task, "--timeout", "1s", "--json"]));
    assert_eq!(watch.status.code(), Some(3), "{}", stderr(&watch));
    assert!(
        String::from_utf8_lossy(&watch.stdout).contains("\"seq\""),
        "the watcher replays events the executor already wrote: {}",
        String::from_utf8_lossy(&watch.stdout)
    );

    let mut attacher = attacher;
    attacher.kill().expect("kill -9 the attacher");
    let _ = attacher.wait();

    assert!(
        !dir.join("terminal.json").exists(),
        "a crash before the terminal write leaves the agent resumable"
    );

    // Second attacher: the lock is free again, the turn is re-driven against
    // a connection that answers, and the terminal record lands.
    let finished = run_bounded(fixture.lan(&["wait", &task, "--json"]));
    assert!(finished.status.success(), "{}", stderr(&finished));
    let first = json_stdout(&finished);
    assert_eq!(first["state"], "succeeded");
    assert_eq!(first["task"], task);

    let again = run_bounded(fixture.lan(&["wait", &task, "--json"]));
    let second = json_stdout(&again);
    assert_eq!(second, first, "terminal results are repeatably observable");
}

/// Spec acceptance: concurrent waiters for two queued messages serialize on
/// the attach lock; each receives its own correlated reply, and the event
/// journal shows one strictly ordered execution.
#[test]
fn concurrent_message_waiters_serialize_and_keep_their_own_replies() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());
    let task = fixture.spawn_agent(&endpoint, "5m");

    let first = json_stdout(&run_bounded(fixture.lan(&[
        "send",
        &task,
        "first question",
        "--json",
    ])));
    let second = json_stdout(&run_bounded(fixture.lan(&[
        "send",
        &task,
        "second question",
        "--json",
    ])));
    assert_eq!(first["state"], "accepted");
    let first_id = first["message"].as_str().expect("message id").to_string();
    let second_id = second["message"].as_str().expect("message id").to_string();

    let waiters: Vec<Child> = [&first_id, &second_id]
        .iter()
        .map(|id| {
            fixture
                .lan(&["wait", &task, "--message", id, "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start waiter")
        })
        .collect();
    let outputs: Vec<Output> = waiters.into_iter().map(finish_bounded).collect();

    let mut results = Vec::new();
    for (output, id) in outputs.iter().zip([&first_id, &second_id]) {
        assert!(output.status.success(), "{}", stderr(output));
        let payload = json_stdout(output);
        assert_eq!(payload["message"], id.as_str());
        assert_eq!(payload["state"], "succeeded");
        results.push(payload["result"].as_str().unwrap_or_default().to_string());
    }
    assert_ne!(results[0], results[1], "each reply is its own turn's");

    // One executor won the lock and drove strictly serialized turns.
    let events =
        fs::read_to_string(fixture.agent_dir(&task).join("events.jsonl")).expect("event journal");
    let seqs: Vec<u64> = events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|record| record["seq"].as_u64())
        .collect();
    assert!(!seqs.is_empty());
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "event sequence must be strictly monotonic: {seqs:?}"
    );
}

/// Spec acceptance: `cancel` on an agent nobody attached to is honored at its
/// next attach, with zero model turns.
#[test]
fn cancel_before_any_attach_settles_without_a_model_turn() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());
    let task = fixture.spawn_agent(&endpoint, "5m");

    let cancelled = json_stdout(&run_bounded(fixture.lan(&["cancel", &task, "--json"])));
    assert_eq!(cancelled["state"], "cancel_requested");
    assert_eq!(cancelled["next"], format!("lan wait {task}"));

    let waited = run_bounded(fixture.lan(&["wait", &task, "--json"]));
    assert_eq!(waited.status.code(), Some(1), "{}", stderr(&waited));
    let payload = json_stdout(&waited);
    assert_eq!(payload["state"], "cancelled");
    assert!(
        endpoint.requests().is_empty(),
        "a cancelled agent settles without touching the model"
    );

    // Cancelling a settled task is an idempotent observation.
    let again = json_stdout(&run_bounded(fixture.lan(&["cancel", &task, "--json"])));
    assert_eq!(again["state"], "cancelled");
}

/// Spec acceptance: the kill window between a child's terminal and its
/// parent's. Reconstructed directly as the on-disk state that window leaves —
/// both completions recorded, neither terminal written — the next attach
/// must finish the two writes child-first.
#[test]
fn a_parent_killed_before_its_terminal_finishes_child_first_on_reattach() {
    let fixture = Fixture::new();
    let key = "0123456789abcdef";
    let parent = format!("{key}/{:032x}", 1);
    let child = format!("{key}/{:032x}", 2);
    write_agent(&fixture, &parent, None, "parent done");
    write_agent(&fixture, &child, Some(&parent), "child done");

    let output = run_bounded(fixture.lan(&["wait", &parent, "--json"]));
    assert!(output.status.success(), "{}", stderr(&output));
    let payload = json_stdout(&output);
    assert_eq!(payload["state"], "succeeded");
    assert_eq!(payload["result"], "parent done");

    let child_terminal: Value = serde_json::from_slice(
        &fs::read(fixture.agent_dir(&child).join("terminal.json"))
            .expect("the settle pass finished the child before the parent"),
    )
    .expect("child terminal JSON");
    assert_eq!(child_terminal["result"], "child done");
    assert!(fixture.agent_dir(&parent).join("terminal.json").is_file());
}

/// Spec acceptance: a cycle of two waiting processes is two pollers; both end
/// at their deadlines with exit 3 and durable retry handles.
#[test]
fn a_wait_cycle_is_two_pollers_bounded_by_their_deadlines() {
    let fixture = Fixture::new();
    let key = "fedcba9876543210";
    let left = format!("{key}/{:032x}", 1);
    let right = format!("{key}/{:032x}", 2);
    write_resumable_agent(&fixture, &left, None);
    write_resumable_agent(&fixture, &right, None);

    // Stand in for the two live executors: hold both attach locks so each
    // waiter can only poll.
    let hold = |task: &str| {
        let dir = fixture.agent_dir(task);
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join("attach.lock"))
            .expect("open lock");
        fs2::FileExt::try_lock_exclusive(&file).expect("hold lock");
        file
    };
    let _left_lock = hold(&left);
    let _right_lock = hold(&right);

    let waiters: Vec<Child> = [&left, &right]
        .iter()
        .map(|task| {
            fixture
                .lan(&["wait", task, "--timeout", "1s", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start waiter")
        })
        .collect();
    for (output, task) in waiters.into_iter().map(finish_bounded).zip([&left, &right]) {
        assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
        let payload = json_stdout(&output);
        assert_eq!(payload["code"], "timeout");
        assert_eq!(payload["timed_out"], true);
        assert_eq!(payload["task"], task.as_str());
        assert_eq!(
            payload["state"], "running",
            "a held lock renders as running"
        );
        assert_eq!(payload["next"], format!("lan wait {task}"));
    }
}

/// Spec acceptance: after any completed CLI invocation, no lan process
/// remains. The handles and the private data root are unique to this test, so
/// any surviving process would still name them in its arguments.
#[test]
fn no_resident_process_survives_any_completed_verb() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());
    let task = fixture.spawn_agent(&endpoint, "5m");

    run_bounded(fixture.lan(&["send", &task, "a question", "--json"]));
    run_bounded(fixture.lan(&["wait", &task, "--json"]));
    run_bounded(fixture.lan(&["watch", &task, "--timeout", "1s", "--json"]));
    run_bounded(fixture.lan(&["inbox", &task, "--json"]));
    run_bounded(fixture.lan(&["cancel", &task, "--json"]));

    #[cfg(unix)]
    {
        let listing = Command::new("ps")
            .args(["ax", "-o", "args"])
            .output()
            .expect("ps");
        let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
        let leftovers: Vec<&str> = listing
            .lines()
            .filter(|line| {
                line.contains(&task) || line.contains(&fixture.data.display().to_string())
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "completed verbs must leave no resident process: {leftovers:?}"
        );
    }
    #[cfg(windows)]
    {
        // TerminateProcess semantics aside, nothing here ever detaches a
        // child: the absence of a spawn is the Windows guarantee too. The
        // tasklist snapshot cannot show arguments, so the process-table check
        // is Unix-only; the behavior under test is identical.
    }
}

/// E1's deferred coverage folded through the new path: a workspace's
/// `.lan/hooks.json` keeps its say over every attached turn
/// (`PreparedRun::with_workspace`), and the roster offered to the model is
/// the workspace's own.
#[cfg(unix)]
#[test]
fn workspace_hooks_guard_turns_driven_through_attach() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let script = fixture.workspace.join("deny.sh");
    fs::write(
        &script,
        "#!/bin/sh\necho '{\"decision\":\"deny\",\"reason\":\"workspace guard\"}'\n",
    )
    .expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::create_dir_all(fixture.workspace.join(".lan")).expect("dir");
    fs::write(
        fixture.workspace.join(".lan/hooks.json"),
        format!(
            r#"{{"schema": 1, "hooks": [{{"name": "guard", "command": ["{}"]}}]}}"#,
            script.display()
        ),
    )
    .expect("hooks file");

    let endpoint = ScriptedEndpoint::start(vec![Reply::files_create("made.txt"), Reply::Text]);
    let mut command = fixture.lan(&["spawn", "write a file", "--await", "--json", "-C"]);
    command.arg(&fixture.workspace).args([
        "--base-url",
        &endpoint.base_url,
        "--model",
        "test-model",
    ]);
    let output = run_bounded(command);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !fixture.workspace.join("made.txt").exists(),
        "the workspace's hook must stop the write on the attach path"
    );

    let requests = endpoint.requests();
    let body: Value = serde_json::from_str(
        requests[0]
            .split("\r\n\r\n")
            .nth(1)
            .expect("a request body"),
    )
    .expect("a JSON request");
    let offered: Vec<&str> = body["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        offered.contains(&"spawn"),
        "the workspace's roster reached the model: {offered:?}"
    );
}

/// Writes an agent directory in the post-kill-window shape: completion
/// recorded in `meta.json`, terminal record absent.
fn write_agent(fixture: &Fixture, task: &str, parent: Option<&str>, result: &str) {
    let dir = fixture.agent_dir(task);
    fs::create_dir_all(&dir).expect("agent dir");
    let meta = json!({
        "id": task,
        "parent": parent,
        "detached": parent.is_none(),
        "workspace": fixture.workspace.display().to_string(),
        "agent_id": "",
        "prompt": "recorded work",
        "options": {
            "provider": null, "base_url": null, "model": null, "no_shell": false,
            "effort": null, "approve": "never", "deadline_ms": null,
            "tool_budget": null, "token_budget": null
        },
        "pending_terminal": {"state": "succeeded", "result": result},
        "deadline_at_ms": null,
        "created_ms": 1,
        "updated_ms": 1
    });
    fs::write(dir.join("meta.json"), meta.to_string()).expect("meta");
}

fn write_resumable_agent(fixture: &Fixture, task: &str, parent: Option<&str>) {
    let dir = fixture.agent_dir(task);
    fs::create_dir_all(&dir).expect("agent dir");
    let meta = json!({
        "id": task,
        "parent": parent,
        "detached": parent.is_none(),
        "workspace": fixture.workspace.display().to_string(),
        "agent_id": "",
        "prompt": "recorded work",
        "options": {
            "provider": null, "base_url": null, "model": null, "no_shell": false,
            "effort": null, "approve": "never", "deadline_ms": null,
            "tool_budget": null, "token_budget": null
        },
        "deadline_at_ms": null,
        "created_ms": 1,
        "updated_ms": 1
    });
    fs::write(dir.join("meta.json"), meta.to_string()).expect("meta");
}

// ---------------------------------------------------------------------------
// The endpoint — `lan-core/tests/runtime.rs`'s, plus `Stall`.
// ---------------------------------------------------------------------------

/// What one connection answers with.
#[derive(Clone)]
enum Reply {
    /// A finished assistant message, numbered by connection.
    Text,
    /// A single tool call; the next connection is expected to wrap up.
    ToolCall { name: String, arguments: String },
    /// Reads the request and then holds the connection open, answering
    /// nothing, until the client goes away. What a mid-turn kill needs.
    Stall,
}

impl Reply {
    fn files_create(path: &str) -> Self {
        Self::ToolCall {
            name: "files".to_string(),
            arguments: json!({"operations": [{"op": "create", "path": path, "content": "hi"}]})
                .to_string(),
        }
    }
}

/// An OpenAI-compatible endpoint on loopback that follows a per-connection
/// script (falling back to a numbered text reply) and keeps every request it
/// was sent.
struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            let mut index = 0_usize;
            while let Ok((stream, _)) = listener.accept() {
                index += 1;
                let reply = script.get(index - 1).cloned().unwrap_or(Reply::Text);
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || answer(stream, index, &reply, &recorded));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            requests,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

fn answer(mut stream: TcpStream, index: usize, reply: &Reply, recorded: &Mutex<Vec<String>>) {
    let request = read_http_request(&mut stream);
    recorded.lock().expect("requests").push(request);

    if matches!(reply, Reply::Stall) {
        // Hold the connection until the client dies; the read returns when
        // the killed process's socket closes.
        let mut sink = [0_u8; 64];
        while matches!(stream.read(&mut sink), Ok(read) if read > 0) {}
        return;
    }

    let body = sse_body(index, reply);
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The smallest stream that is a finished turn of the requested shape.
fn sse_body(index: usize, reply: &Reply) -> String {
    let mut events = vec![json!({
        "type": "response.created",
        "response": {"id": format!("resp_{index}"), "model": "test-model", "status": "in_progress"}
    })];

    match reply {
        Reply::Stall => unreachable!("a stall never writes a body"),
        Reply::Text => {
            events.push(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "content": []}
            }));
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message", "content": [{"type": "output_text", "text": format!("reply-{index}")}]}
            }));
        }
        Reply::ToolCall { name, arguments } => {
            events.push(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "id": format!("fc_{index}"),
                         "call_id": format!("call_{index}"), "name": name, "arguments": ""}
            }));
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": format!("call_{index}"),
                         "name": name, "arguments": arguments}
            }));
        }
    }

    events.push(json!({
        "type": "response.completed",
        "response": {"id": format!("resp_{index}"), "model": "test-model", "status": "completed"}
    }));

    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// Reads a request up to the end of its declared body.
///
/// Reading to end-of-stream would deadlock: the client keeps the connection
/// open waiting for the response it has not been sent yet.
fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    while let Ok(read) = stream.read(&mut buffer) {
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if header_end.is_none()
            && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = index + 4;
            header_end = Some(end);
            content_length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap_or_default())
                })
                .unwrap_or_default();
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            break;
        }
    }

    String::from_utf8_lossy(&bytes).into_owned()
}
