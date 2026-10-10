//! A task whose process died mid tool round is continued on the next attach,
//! never settled on the turn's partial text and never asked again
//! (mentra#64).
//!
//! The crash has to be a real one, so the first life runs as a child process
//! that the test kills: its in-flight model request is held at the scripted
//! endpoint until the kill, exactly where a process dying mid-turn leaves it.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use basis_tasks::{Approve, RunSpec, TaskHandle, Tasks, Terminal, WaitOutcome};
use serde_json::{Value, json};

/// Where the crashing first life finds what the test set up for it.
const FIRST_LIFE: &str = "BASIS_TASKS_TEST_FIRST_LIFE";

/// One scripted model turn.
#[derive(Clone)]
enum Reply {
    Text(&'static str),
    Write {
        path: &'static str,
        content: &'static str,
    },
    /// Never answered: the process asking is killed while it waits.
    Hold,
}

/// A loopback `chat/completions` endpoint answering its `n`th turn request
/// with the script's `n`th reply, keeping every request body.
struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(script);
        let shared = Arc::clone(&requests);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let (requests, script) = (Arc::clone(&shared), Arc::clone(&script));
                thread::spawn(move || answer(stream, &requests, &script));
            }
        });
        Self {
            base_url: format!("http://{address}/"),
            requests,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn wait_for_requests(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.requests().len() < count {
            assert!(Instant::now() < deadline, "the model was never called");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn answer(mut stream: TcpStream, requests: &Mutex<Vec<String>>, script: &[Reply]) {
    let request = read_http_request(&mut stream);
    let first_line = request.lines().next().unwrap_or_default();
    if first_line.starts_with("GET ") && first_line.contains("/models") {
        respond(
            &mut stream,
            "application/json",
            r#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#,
        );
        return;
    }
    let index = {
        let mut requests = requests.lock().unwrap();
        requests.push(request);
        requests.len() - 1
    };
    let id = format!("chatcmpl_{index}");
    let (delta, finish) = match script.get(index).cloned().unwrap_or(Reply::Text("extra")) {
        Reply::Hold => loop {
            thread::sleep(Duration::from_secs(60));
        },
        Reply::Text(text) => (json!({"role": "assistant", "content": text}), "stop"),
        Reply::Write { path, content } => (
            json!({"role": "assistant", "tool_calls": [{
                "index": 0, "id": format!("call_{index}"), "type": "function",
                "function": {
                    "name": "write",
                    "arguments": json!({"path": path, "content": content}).to_string(),
                }
            }]}),
            "tool_calls",
        ),
    };
    let events = [
        json!({"id": id, "model": "test-model", "choices": [{"index": 0, "delta": delta}]}),
        json!({"id": id, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
    ];
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect();
    respond(&mut stream, "text/event-stream", &body);
}

fn respond(stream: &mut TcpStream, content_type: &str, body: &str) {
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: {content_type}\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
}

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

/// Keeps both lives off the developer's own credentials and config.
fn isolate_environment(config: &Path) {
    // SAFETY: set before any thread of this test reads the environment, and
    // every test here wants the same values.
    unsafe {
        std::env::set_var("BASIS_API_KEY", "test-key");
        std::env::set_var("BASIS_CONFIG_DIR", config);
    }
}

fn spec(prompt: &str, model: &str) -> RunSpec {
    RunSpec::new(prompt)
        .with_base_url(model)
        .with_model("test-model")
        .with_deadline(Duration::from_secs(120))
        .with_approve(Approve::Always)
}

/// The first life: drives the task until the test kills it. Prints the
/// handle (and a message id, when it sends one) before it starts driving.
#[test]
#[ignore = "run as a child process by the tests below"]
fn first_life_that_dies_mid_tool_round() {
    let Ok(setup) = std::env::var(FIRST_LIFE) else {
        return;
    };
    let setup: Value = serde_json::from_str(&setup).unwrap();
    let field = |name: &str| setup[name].as_str().unwrap().to_string();
    isolate_environment(Path::new(&field("config")));
    let tasks = Tasks::open_at(field("data"), field("workspace")).unwrap();
    let handle = tasks
        .spawn(spec(&field("prompt"), &field("model")))
        .unwrap();
    let message = setup["message"]
        .as_str()
        .map(|message| tasks.send(&handle, message).unwrap());
    println!("SPAWNED {handle} {}", message.unwrap_or_default());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let _ = tasks.wait(&handle, None, Duration::MAX, None).await;
    });
}

struct Crashed {
    handle: TaskHandle,
    message: Option<String>,
    tasks: Tasks,
    workspace: tempfile::TempDir,
    _data: tempfile::TempDir,
    _config: tempfile::TempDir,
}

/// Runs the first life against `model` until the endpoint has seen
/// `requests` turn requests, the last of them held, then kills it.
fn crash_after(model: &ScriptedEndpoint, message: Option<&str>, requests: usize) -> Crashed {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    isolate_environment(config.path());
    let setup = json!({
        "data": data.path(), "workspace": workspace.path(), "config": config.path(),
        "model": model.base_url, "prompt": "write the note", "message": message,
    });
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "first_life_that_dies_mid_tool_round",
            "--ignored",
            "--nocapture",
        ])
        .env(FIRST_LIFE, setup.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let spawned = lines
        .find_map(|line| line.ok()?.strip_prefix("SPAWNED ").map(str::to_owned))
        .expect("the first life spawns its task");
    let (handle, message) = spawned.split_once(' ').unwrap();
    model.wait_for_requests(requests);
    child.kill().unwrap();
    child.wait().unwrap();

    let tasks = Tasks::open_at(data.path(), workspace.path()).unwrap();
    let handle = TaskHandle::parse(handle).unwrap();
    assert!(
        tasks.terminal(&handle).unwrap().is_none(),
        "the first life died before settling anything"
    );
    Crashed {
        handle,
        message: (!message.is_empty()).then(|| message.to_string()),
        tasks,
        workspace,
        _data: data,
        _config: config,
    }
}

async fn settle(crashed: &Crashed) -> Terminal {
    let outcome = crashed
        .tasks
        .wait(&crashed.handle, None, Duration::from_secs(60), None)
        .await
        .unwrap();
    let WaitOutcome::Terminal(record) = outcome else {
        panic!("the resumed task settles: {outcome:?}");
    };
    record.terminal.expect("a typed terminal")
}

/// How many times the conversation sent to the model holds `text`.
fn occurrences(request: &str, text: &str) -> usize {
    request.matches(text).count()
}

#[tokio::test]
async fn a_prompt_cut_off_after_its_tool_ran_is_continued_not_settled() {
    let model = ScriptedEndpoint::start(vec![
        Reply::Write {
            path: "note.txt",
            content: "once",
        },
        // The model was being asked to answer the write's result.
        Reply::Hold,
        Reply::Text("the note is written"),
    ]);
    let crashed = crash_after(&model, None, 2);

    let terminal = settle(&crashed).await;

    assert_eq!(
        terminal,
        Terminal::Succeeded {
            result: "the note is written".to_string()
        },
        "the turn is finished by the model, not settled on the cut-off turn's text"
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "one more model call: the continuation");
    assert_eq!(
        occurrences(&requests[2], "write the note"),
        1,
        "the prompt is not asked a second time"
    );
    assert_eq!(
        std::fs::read_to_string(crashed.workspace.path().join("note.txt")).unwrap(),
        "once",
        "the write ran once, in the first life, and its result was kept"
    );
}

#[tokio::test]
async fn a_message_cut_off_mid_tool_round_gets_its_reply_from_the_continued_turn() {
    let model = ScriptedEndpoint::start(vec![
        Reply::Text("ready"),
        Reply::Write {
            path: "note.txt",
            content: "from the message",
        },
        Reply::Hold,
        Reply::Text("done with the message"),
    ]);
    let crashed = crash_after(&model, Some("now write the note"), 3);
    let message = crashed.message.clone().expect("a message was sent");

    let reply = crashed
        .tasks
        .wait_message(&crashed.handle, None, &message, Duration::from_secs(60))
        .await
        .unwrap();
    let WaitOutcome::Terminal(reply) = reply else {
        panic!("the message is answered: {reply:?}");
    };
    assert_eq!(
        reply.raw["result"], "done with the message",
        "{:?}",
        reply.raw
    );

    assert_eq!(
        settle(&crashed).await,
        Terminal::Succeeded {
            result: "done with the message".to_string()
        }
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        occurrences(&requests[3], "now write the note"),
        1,
        "the message is not sent a second time"
    );
}
