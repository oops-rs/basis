//! A task's tool and skill rosters are its own: recorded at spawn, applied
//! by whichever process attaches, and restated onto a conversation the task
//! continues rather than left as that conversation's first task had them.
//!
//! The endpoint is the loopback `chat/completions` listener the other
//! integration tests here script a provider with, keeping every request so a
//! test can read what the model was offered.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::Duration,
};

use basis::{SkillRoster, ToolRoster};
use basis_tasks::{Approve, Continuation, RunSpec, TaskHandle, Tasks, Terminal, WaitOutcome};
use serde_json::{Value, json};

/// What one model request offered: its tool names, sorted, and its system
/// prompt.
struct Offered {
    tools: Vec<String>,
    system: String,
}

struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&requests);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let requests = Arc::clone(&shared);
                thread::spawn(move || answer(stream, &requests));
            }
        });
        Self {
            base_url: format!("http://{address}/"),
            requests,
        }
    }

    /// What the most recent request offered the model.
    fn last(&self) -> Offered {
        let requests = self.requests.lock().unwrap();
        let body = requests.last().expect("the model was asked");
        let mut tools: Vec<String> = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
            .collect();
        tools.sort();
        let system = body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|message| message["role"] == "system" || message["role"] == "developer")
            .filter_map(|message| message["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Offered { tools, system }
    }
}

fn answer(mut stream: TcpStream, requests: &Mutex<Vec<Value>>) {
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
    let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    let index = {
        let mut requests = requests.lock().unwrap();
        requests.push(serde_json::from_str(body).unwrap_or(Value::Null));
        requests.len()
    };
    let id = format!("chatcmpl_{index}");
    let events = [
        json!({"id": id, "model": "test-model", "choices": [{"index": 0, "delta": {"role": "assistant", "content": format!("reply-{index}")}}]}),
        json!({"id": id, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
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

/// Keeps the test off the developer's own credentials and config.
fn isolate_environment() {
    static CONFIG: OnceLock<tempfile::TempDir> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config = tempfile::tempdir().unwrap();
        // SAFETY: set once, before any test here reads the environment.
        unsafe {
            std::env::set_var("BASIS_API_KEY", "test-key");
            std::env::set_var("BASIS_CONFIG_DIR", config.path());
        }
        config
    });
}

fn spec(prompt: &str, endpoint: &ScriptedEndpoint) -> RunSpec {
    RunSpec::new(prompt)
        .with_base_url(&endpoint.base_url)
        .with_model("test-model")
        .with_deadline(Duration::from_secs(120))
        .with_approve(Approve::Never)
}

async fn settle(tasks: &Tasks, spec: RunSpec) -> TaskHandle {
    let handle = tasks.spawn(spec).expect("spawn");
    let outcome = tasks
        .wait(&handle, None, Duration::from_secs(60), None)
        .await
        .expect("wait completes");
    let WaitOutcome::Terminal(terminal) = outcome else {
        panic!("a scripted endpoint always answers inside the timeout");
    };
    assert!(
        matches!(terminal.terminal, Some(Terminal::Succeeded { .. })),
        "{terminal:?}"
    );
    handle
}

fn write_skill(workspace: &std::path::Path, name: &str, description: &str) {
    let skill = workspace.join(".basis/skills").join(name);
    std::fs::create_dir_all(&skill).expect("create skill directory");
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nSteps.\n"),
    )
    .expect("write skill");
}

#[tokio::test]
async fn a_task_is_offered_its_own_rosters_and_a_continued_conversation_takes_the_new_tasks() {
    isolate_environment();
    let endpoint = ScriptedEndpoint::start();
    let data_dir = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    write_skill(workspace.path(), "build", "Runs builds");
    write_skill(workspace.path(), "ask", "Answers questions");
    let tasks = Tasks::open_at(data_dir.path(), workspace.path()).expect("opens");

    // The conversation opens for a caller who may build.
    let first = settle(
        &tasks,
        spec("one", &endpoint)
            .with_tool_roster(ToolRoster::only(["read", "load_skill"]))
            .with_skill_roster(SkillRoster::only(["build"])),
    )
    .await;
    let offered = endpoint.last();
    assert_eq!(offered.tools, ["load_skill", "read"]);
    assert!(
        offered.system.contains("- build: Runs builds"),
        "{}",
        offered.system
    );
    assert!(!offered.system.contains("- ask:"), "{}", offered.system);

    // The same conversation, continued for one who may only ask: the new
    // task's rosters, not the ones the conversation's first run stored.
    let second = settle(
        &tasks,
        spec("two", &endpoint)
            .continuing(Continuation::Named(first))
            .with_tool_roster(ToolRoster::only(["grep", "load_skill"]))
            .with_skill_roster(SkillRoster::only(["ask"])),
    )
    .await;
    let offered = endpoint.last();
    assert_eq!(offered.tools, ["grep", "load_skill"]);
    assert!(
        offered.system.contains("- ask: Answers questions"),
        "{}",
        offered.system
    );
    assert!(!offered.system.contains("- build:"), "{}", offered.system);

    // A task that states none leaves the conversation as it found it.
    settle(
        &tasks,
        spec("three", &endpoint).continuing(Continuation::Named(second)),
    )
    .await;
    assert_eq!(endpoint.last().tools, ["grep", "load_skill"]);
}

#[tokio::test]
async fn a_task_that_states_no_roster_gets_the_workspaces() {
    isolate_environment();
    let endpoint = ScriptedEndpoint::start();
    let data_dir = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    let tasks = Tasks::open_at(data_dir.path(), workspace.path()).expect("opens");

    settle(&tasks, spec("one", &endpoint)).await;

    let tools = endpoint.last().tools;
    for name in ["read", "grep", "write"] {
        assert!(tools.contains(&name.to_string()), "{tools:?}");
    }
}
