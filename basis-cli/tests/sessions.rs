//! A way back into a conversation from a shell: `basis list`, and the two
//! spellings that start a new task on an old dialogue.
//!
//! Driven through the real binary against a loopback-scripted endpoint, the
//! way `attach.rs` drives ADR-0019's acceptance surface. The endpoint here is
//! the same one, trimmed to what these tests need and grown a usage report —
//! the counts a finished task now records.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
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
    elsewhere: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let elsewhere = root.path().join("elsewhere");
        let data = root.path().join("data");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&elsewhere).expect("second workspace");
        Self {
            _root: root,
            workspace,
            elsewhere,
            data,
        }
    }

    fn basis(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_basis"));
        command
            .env("BASIS_DATA_DIR", &self.data)
            .env("BASIS_API_KEY", "test-key")
            .env_remove("BASIS_TASK_ID")
            .env_remove("BASIS_BASE_URL")
            .env_remove("OPENAI_BASE_URL")
            .args(args);
        command
    }

    /// A spawn against `endpoint`, in `workspace`, with the flags every test
    /// here needs and nothing else.
    fn run(&self, workspace: &Path, endpoint: &ScriptedEndpoint, args: &[&str]) -> Command {
        let mut command = self.basis(&["spawn"]);
        command.args(args).arg("-C").arg(workspace).args([
            "--base-url",
            &endpoint.base_url,
            "--model",
            "test-model",
            "--deadline",
            "5m",
        ]);
        command
    }

    fn list(&self, workspace: &Path, args: &[&str]) -> Command {
        let mut command = self.basis(&["list"]);
        command.args(args).arg("-C").arg(workspace);
        command
    }

    fn agent_dir(&self, task: &str) -> PathBuf {
        let (key, id) = task.split_once('/').expect("handle shape");
        self.data
            .join("workspaces")
            .join(key)
            .join("agents")
            .join(id)
    }

    fn meta(&self, task: &str) -> Value {
        let bytes = fs::read(self.agent_dir(task).join("meta.json")).expect("meta.json");
        serde_json::from_slice(&bytes).expect("meta is JSON")
    }
}

fn run_bounded(mut command: Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    finish_bounded(command.spawn().expect("start basis command"))
}

fn finish_bounded(mut child: Child) -> Output {
    let deadline = Instant::now() + NOT_STUCK;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll basis command") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("basis command did not settle within {NOT_STUCK:?}");
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The durable handle a settled run's hint names — the only place a shell
/// invocation prints it, since stdout is the answer.
fn task_in_hint(hints: &str) -> String {
    hints
        .lines()
        .find_map(|line| line.strip_prefix("next: use `basis watch "))
        .map(|rest| rest.trim_end_matches('`').to_string())
        .unwrap_or_else(|| panic!("no durable handle in: {hints}"))
}

fn rows(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
        .collect()
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

/// `list` is the index a shell user lost when they closed the terminal: every
/// task in this workspace, the state each is in, and the handle every other
/// verb takes. It writes nothing — a listing that minted a directory would be
/// a listing that changed its own answer.
#[test]
fn list_reports_a_workspace_task_in_the_state_the_files_say_it_is_in() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());

    let spawned =
        run_bounded(fixture.run(&fixture.workspace, &endpoint, &["hello", "--resumable"]));
    assert!(spawned.status.success(), "{}", stderr(&spawned));
    let task = stdout(&spawned)
        .lines()
        .find_map(|line| line.strip_prefix("task "))
        .and_then(|line| line.split_once(':').map(|(task, _)| task.to_string()))
        .expect("a handle");

    let listed = run_bounded(fixture.list(&fixture.workspace, &["--json"]));
    assert!(listed.status.success(), "{}", stderr(&listed));
    let minted = rows(&listed);
    assert_eq!(minted.len(), 1);
    assert_eq!(minted[0]["task"], task.as_str());
    assert_eq!(
        minted[0]["state"], "resumable",
        "nothing has attached to it yet"
    );
    assert_eq!(minted[0]["prompt"], "hello");
    assert_eq!(
        minted[0]["continuable"], false,
        "an agent nobody attached to has no conversation to continue"
    );

    // Driving it changes what the files say, and therefore what `list` says.
    let driven = run_bounded(fixture.basis(&["wait", &task, "--json"]));
    assert!(driven.status.success(), "{}", stderr(&driven));

    let listed = run_bounded(fixture.list(&fixture.workspace, &["--json"]));
    let settled = rows(&listed);
    assert_eq!(settled[0]["state"], "succeeded");
    assert_eq!(settled[0]["continuable"], true);
    assert_eq!(
        settled[0]["usage"]["input_tokens"], 100,
        "what the task spent survives the process that spent it: {}",
        settled[0]
    );

    // The human form carries the same facts, handle first, because the handle
    // is what the next command takes.
    let human = run_bounded(fixture.list(&fixture.workspace, &[]));
    let row = stdout(&human);
    assert!(row.starts_with(&task), "{row}");
    assert!(row.contains("succeeded") && row.contains("hello"), "{row}");
    assert!(
        stderr(&human).contains("basis spawn --continue"),
        "a workspace with a conversation in it says how to continue: {}",
        stderr(&human)
    );
}

/// The point of the feature: a second task, a new handle, and one
/// conversation. `send` cannot do this — a task holding a terminal record
/// refuses messages (ADR-0019) — so continuing a finished dialogue has to
/// mint a task that resumes the old one's agent.
#[test]
fn continue_mints_a_new_task_whose_turn_carries_the_first_ones_conversation() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());

    let first = run_bounded(fixture.run(&fixture.workspace, &endpoint, &["remember the number 7"]));
    assert!(first.status.success(), "{}", stderr(&first));
    let first_task = task_in_hint(&stderr(&first));

    let second = run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["--continue", "what number?"],
    ));
    assert!(second.status.success(), "{}", stderr(&second));
    let second_task = task_in_hint(&stderr(&second));

    assert_ne!(
        first_task, second_task,
        "a continued conversation is a new task, with its own handle and its own bounds"
    );
    assert_eq!(
        fixture.meta(&second_task)["continues"],
        fixture.meta(&first_task)["agent_id"],
        "the new task records the conversation it continues"
    );
    assert_eq!(
        fixture.meta(&second_task)["agent_id"],
        fixture.meta(&first_task)["agent_id"],
        "and attaches to exactly that one"
    );

    // The claim that matters is on the wire: the second run's request replays
    // the first run's exchange, so the model is answering in the same
    // conversation rather than starting a fresh one that merely looks alike.
    let requests = endpoint.requests();
    assert_eq!(requests.len(), 2, "one round each");
    assert!(
        requests[1].contains("remember the number 7") && requests[1].contains("what number?"),
        "the continued turn carries the history: {}",
        requests[1]
    );
    assert!(
        !requests[0].contains("what number?"),
        "and the first turn could not have"
    );
}

/// Two executors on one conversation is exactly what the attach lock exists
/// to prevent, so `--session` naming a task something is driving is a
/// refusal that names the state rather than a second resume of the same agent.
#[test]
fn a_session_something_is_driving_is_refused_by_the_state_it_is_in() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(vec![Reply::Stall]);

    let spawned = run_bounded(fixture.run(&fixture.workspace, &endpoint, &["hold", "--resumable"]));
    let task = stdout(&spawned)
        .lines()
        .find_map(|line| line.strip_prefix("task "))
        .and_then(|line| line.split_once(':').map(|(task, _)| task.to_string()))
        .expect("a handle");

    // An attacher that reaches its model turn and stays there, holding the
    // lock, is the live executor this refusal is about.
    let mut attacher = fixture
        .basis(&["wait", &task, "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start attacher");
    wait_until("the executor to reach its model turn", || {
        !endpoint.requests().is_empty()
    });

    let listed = run_bounded(fixture.list(&fixture.workspace, &["--json"]));
    assert_eq!(
        rows(&listed)[0]["state"],
        "running",
        "a held attach lock is the only evidence a live executor leaves"
    );

    let refused = run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["--session", &task, "join in"],
    ));
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("is running"),
        "the refusal names the state that caused it: {}",
        stderr(&refused)
    );

    attacher.kill().expect("kill the attacher");
    let _ = attacher.wait();
}

/// Half the handle is the workspace key, and a conversation belongs to the
/// workspace whose context and tools produced it. A handle from somewhere
/// else is an argument that will never be right here, which is what exit 2
/// says.
#[test]
fn a_session_from_another_workspace_is_refused_as_a_wrong_argument() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());

    let first = run_bounded(fixture.run(&fixture.workspace, &endpoint, &["over here"]));
    assert!(first.status.success(), "{}", stderr(&first));
    let task = task_in_hint(&stderr(&first));

    let refused = run_bounded(fixture.run(
        &fixture.elsewhere,
        &endpoint,
        &["--session", &task, "and over there"],
    ));
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("another workspace"),
        "{}",
        stderr(&refused)
    );

    // And the other workspace's own list is empty, which is the same fact
    // seen from the other side.
    let listed = run_bounded(fixture.list(&fixture.elsewhere, &["--json"]));
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(stdout(&listed).is_empty());
}

/// `--continue` in a workspace where nothing has ever run is a state, not a
/// bad command line: exit 1, and a next step that works.
#[test]
fn continuing_where_there_is_nothing_to_continue_says_so() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());

    let refused = run_bounded(fixture.run(&fixture.workspace, &endpoint, &["--continue", "more"]));

    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("no task in this workspace has a conversation to continue"),
        "{}",
        stderr(&refused)
    );
    assert!(
        endpoint.requests().is_empty(),
        "a refused spawn touches no model"
    );
}

// ---------------------------------------------------------------------------
// The endpoint — `attach.rs`'s, trimmed, plus a usage report.
// ---------------------------------------------------------------------------

/// What one connection answers with.
#[derive(Clone)]
enum Reply {
    /// A finished assistant message, numbered by connection.
    Text,
    /// Reads the request and then holds the connection open until the client
    /// goes away — what a live executor holding the attach lock looks like.
    Stall,
}

/// An OpenAI-compatible endpoint on loopback that keeps every request body it
/// was sent, which is where "did this turn carry the conversation" is decided.
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
        let mut sink = [0_u8; 64];
        while matches!(stream.read(&mut sink), Ok(read) if read > 0) {}
        return;
    }

    let body = sse_body(index);
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The smallest stream that is a finished turn, with the usage report a real
/// provider sends on the completed response.
fn sse_body(index: usize) -> String {
    [
        json!({
            "type": "response.created",
            "response": {"id": format!("resp_{index}"), "model": "test-model", "status": "in_progress"}
        }),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "content": []}
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "message", "content": [{"type": "output_text", "text": format!("reply-{index}")}]}
        }),
        json!({
            "type": "response.completed",
            "response": {"id": format!("resp_{index}"), "model": "test-model", "status": "completed",
                         "usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120}}
        }),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// Reads a request up to the end of its declared body. Reading to
/// end-of-stream would deadlock: the client holds the connection open waiting
/// for a response it has not been sent yet.
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
