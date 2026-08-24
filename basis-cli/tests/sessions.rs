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
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
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

    /// Rewrites a task's `meta.json`, which is how a record written by an
    /// older basis is staged: drop a field this version writes and the loader
    /// has to cope with exactly what an upgrade will hand it.
    fn set_meta(&self, task: &str, meta: &Value) {
        let bytes = serde_json::to_vec(meta).expect("meta is JSON");
        fs::write(self.agent_dir(task).join("meta.json"), bytes).expect("rewrite meta.json");
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

/// The handle a `--resumable` spawn prints, which is its whole stdout.
fn resumable_task(output: &Output) -> String {
    stdout(output)
        .lines()
        .find_map(|line| line.strip_prefix("task "))
        .and_then(|line| line.split_once(':').map(|(task, _)| task.to_string()))
        .unwrap_or_else(|| panic!("no handle in: {}", stdout(output)))
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
    let task = resumable_task(&spawned);

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

/// `--continue` means *the conversation I was just in*, and the task that
/// started last is not always the one you were just in. Two tasks minted in
/// order, then driven in the opposite order: the second handle is younger, the
/// first handle is where the work happened, and the work is what wins.
///
/// `list` is asserted alongside it, because the two verbs share one scan: a
/// listing whose top row is not the row `--continue` takes would be an index
/// that disagrees with the command it exists to feed.
#[test]
fn continue_takes_the_conversation_last_worked_in_not_the_one_started_last() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());

    let first = resumable_task(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["remember the number 7", "--resumable"],
    )));
    let second = resumable_task(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["remember the number 9", "--resumable"],
    )));

    // Driven youngest first, so the older handle is the one last worked in.
    for task in [&second, &first] {
        let driven = run_bounded(fixture.basis(&["wait", task, "--json"]));
        assert!(driven.status.success(), "{}", stderr(&driven));
    }

    let listed = run_bounded(fixture.list(&fixture.workspace, &["--json"]));
    let rows = rows(&listed);
    assert_eq!(
        rows[0]["task"],
        first.as_str(),
        "the workspace's index leads with the task last worked in: {rows:?}"
    );

    let continued = resumable_task(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["--continue", "and then?", "--resumable"],
    )));
    assert_eq!(
        fixture.meta(&continued)["continues"],
        fixture.meta(&first)["agent_id"],
        "the resumed conversation is the one last worked in, not the one started last"
    );
}

/// A message is work in a conversation even when nothing was attached to run
/// it. `send` reaches a task that has a dialogue and no terminal record —
/// what an executor killed mid-turn leaves behind — and putting a message
/// there is the person saying which conversation they are in.
#[test]
fn a_sent_message_is_activity_in_the_conversation_it_was_sent_to() {
    let fixture = Fixture::new();
    // One stalled turn: the abandoned executor, held open until it is killed.
    let endpoint = ScriptedEndpoint::start(vec![Reply::Stall]);

    let first = resumable_task(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["remember the number 7", "--resumable"],
    )));
    let mut abandoned = fixture
        .basis(&["wait", &first, "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the executor");
    wait_until("the executor to mint its conversation", || {
        !endpoint.requests().is_empty()
    });
    abandoned.kill().expect("kill the executor");
    let _ = abandoned.wait();
    assert_ne!(
        fixture.meta(&first)["agent_id"],
        "",
        "a killed executor leaves the conversation it opened"
    );

    // A younger task, driven to a terminal record: the row `--continue` used
    // to take, because it started last.
    let second = task_in_hint(&stderr(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["remember the number 9"],
    ))));

    let sent = run_bounded(fixture.basis(&["send", &first, "keep going", "--json"]));
    assert!(sent.status.success(), "{}", stderr(&sent));

    let continued = resumable_task(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["--continue", "and then?", "--resumable"],
    )));
    assert_eq!(
        fixture.meta(&continued)["continues"],
        fixture.meta(&first)["agent_id"],
        "the message decided which conversation was live, not the younger handle"
    );
    assert_ne!(
        fixture.meta(&continued)["continues"],
        fixture.meta(&second)["agent_id"]
    );
}

/// A `meta.json` written before this version records no activity at all. It
/// has to load — a task that vanishes from `list` after an upgrade is worse
/// than a task ordered by the wrong clock — and it has to resolve, which it
/// does by falling back to when the task started.
#[test]
fn tasks_that_record_no_activity_resolve_by_when_they_started() {
    let fixture = Fixture::new();
    let endpoint = ScriptedEndpoint::start(Vec::new());

    let first = task_in_hint(&stderr(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["remember the number 7"],
    ))));
    let second = task_in_hint(&stderr(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["remember the number 9"],
    ))));

    for task in [&first, &second] {
        let mut meta = fixture.meta(task);
        meta.as_object_mut()
            .expect("an object")
            .remove("updated_ms");
        fixture.set_meta(task, &meta);
    }

    let listed = run_bounded(fixture.list(&fixture.workspace, &["--json"]));
    assert_eq!(
        rows(&listed).len(),
        2,
        "an upgrade hides nothing: {}",
        stdout(&listed)
    );

    let continued = resumable_task(&run_bounded(fixture.run(
        &fixture.workspace,
        &endpoint,
        &["--continue", "and then?", "--resumable"],
    )));
    assert_eq!(
        fixture.meta(&continued)["continues"],
        fixture.meta(&second)["agent_id"],
        "with no activity recorded anywhere, the newest start is the best answer there is"
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
    let task = resumable_task(&spawned);

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
        let script = Arc::new(script);
        let turns = Arc::new(AtomicUsize::new(0));
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let script = Arc::clone(&script);
                let turns = Arc::clone(&turns);
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || answer(stream, &script, &turns, &recorded));
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

/// A pinned model is looked up in the provider's listing before the first
/// turn (mentra `bfe952b`), which is one `GET …/models` per run that is
/// neither a turn nor scripted. Answered with a listing that names the test
/// model, so the lookup succeeds the way a real provider's would, and never
/// counted or recorded as a turn.
fn model_listing(request: &str) -> Option<String> {
    let line = request.lines().next()?;
    let target = line.split_whitespace().nth(1)?;
    (line.starts_with("GET ") && target.ends_with("/models")).then(|| {
        let body = r#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#;
        format!(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    })
}

fn answer(
    mut stream: TcpStream,
    script: &[Reply],
    turns: &AtomicUsize,
    recorded: &Mutex<Vec<String>>,
) {
    let request = read_http_request(&mut stream);
    if let Some(listing) = model_listing(&request) {
        let _ = stream.write_all(listing.as_bytes());
        return;
    }
    let index = turns.fetch_add(1, Ordering::SeqCst) + 1;
    let reply = &script.get(index - 1).cloned().unwrap_or(Reply::Text);
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

/// The smallest `chat/completions` stream that is a finished turn, with the
/// usage report a real provider sends in answer to
/// `stream_options.include_usage`.
fn sse_body(index: usize) -> String {
    let id = format!("chatcmpl_{index}");

    [
        json!({
            "id": id, "model": "test-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": format!("reply-{index}")}}]
        }),
        json!({
            "id": id,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        }),
        json!({
            "id": id, "choices": [],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}
        }),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_string()))
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
