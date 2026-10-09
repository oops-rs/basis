//! One basis agent calling another over A2A, end to end: a workspace whose
//! `.basis/a2a.json` names a live `A2aServer` is told about it in `spawn`'s
//! description, its model calls `spawn "@helper …"`, and the served agent's
//! answer comes back into the calling run as the tool's result (ADR-0028,
//! ADR-0029).
//!
//! Both models are scripted loopback `chat/completions` endpoints, the shape
//! `serve.rs` uses: the served agent's always answers with a fixed sentence,
//! and the caller's asks for the tool on its first turn and wraps up on its
//! second. Nothing about the agent in the middle is faked — the card is
//! fetched from its well-known path, the message is sent with the bearer
//! token the file expanded from the environment, and the task is polled to
//! completion by mentra's client.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use basis::{
    A2aAgentConfig, A2aConfig, AllowAll, CollectingSink, ContextConfig, Event, MemoryConfig,
    ModelSelector, Runtime, Workspace,
};
use basis_a2a::{A2aServer, A2aService, AgentIdentity, BearerToken, RunDefaults, ServeConfig};
use serde_json::{Value, json};

const TOKEN: &str = "test-token";
/// The environment variable the calling workspace's `a2a.json` reads the
/// served agent's token from — the convention's whole point is that the file
/// names the variable, never the credential.
const TOKEN_VARIABLE: &str = "BASIS_A2A_CLIENT_TEST_TOKEN";
const ANSWER: &str = "the answer is forty-two";

#[tokio::test]
async fn a_workspace_hands_work_to_a_served_agent_and_reads_its_answer() {
    isolate_environment();

    // The served agent.
    let served_model = ScriptedEndpoint::start(vec![Reply::Text(ANSWER.to_string())]);
    let served_data = tempfile::tempdir().unwrap();
    let served_workspace = tempfile::tempdir().unwrap();
    let service = A2aService::open(
        ServeConfig::new(
            served_workspace.path(),
            AgentIdentity::new("helper", "Answers questions in tests."),
        )
        .with_data_dir(served_data.path())
        .with_token(BearerToken::new(TOKEN).unwrap())
        .with_run_defaults(
            RunDefaults::default()
                .with_base_url(&served_model.base_url)
                .with_model("test-model")
                .with_deadline(Duration::from_secs(60)),
        ),
    )
    .expect("opens");
    let server = A2aServer::bind("127.0.0.1:0".parse().unwrap(), service)
        .await
        .expect("binds loopback");
    let base = format!("http://{}", server.local_addr());
    tokio::spawn(server.serve());

    // The calling workspace, configured the way an operator would: a file
    // naming the agent and the variable its token lives in.
    let caller_model = ScriptedEndpoint::start(vec![
        Reply::ToolCall {
            name: "spawn".to_string(),
            arguments: json!({"input": "@helper what is the answer?"}).to_string(),
        },
        Reply::Text("done".to_string()),
    ]);
    let caller = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(caller.path().join(".basis")).unwrap();
    std::fs::write(
        caller.path().join(".basis/a2a.json"),
        json!({"agents": {"helper": {
            "url": base,
            "headers": {"Authorization": format!("Bearer ${{{TOKEN_VARIABLE}}}")},
        }}})
        .to_string(),
    )
    .unwrap();

    let workspace = Workspace::builder(caller.path())
        .with_runtime_builder(
            Runtime::builder()
                .with_base_url(&caller_model.base_url)
                .with_api_key("test-key")
                .with_ephemeral_history(),
        )
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_context(ContextConfig::none())
        .with_memory(MemoryConfig::disabled())
        .open()
        .await
        .expect("the caller opens");
    assert_eq!(workspace.a2a_agents(), ["helper"]);

    // `AllowAll` stands in for the approver: a remote delegation declares
    // `External` side effects, so the call reaches the approval gate exactly
    // as an external MCP tool's does.
    let report = workspace
        .prepare("ask the helper")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");
    assert!(report.succeeded(), "{:?}", report.outcome);

    // The served agent's model saw the caller's message.
    assert!(
        served_model
            .requests()
            .iter()
            .any(|request| request.contains("what is the answer?")),
        "the message never reached the served agent"
    );

    // The caller's tool call came back with the served agent's answer, and
    // that answer is what the caller's model read on its next turn.
    let completed = report
        .sink
        .events()
        .iter()
        .find_map(|event| match event {
            Event::ToolCompleted {
                tool_name,
                is_error,
                ..
            } if tool_name == "spawn" => Some(*is_error),
            _ => None,
        })
        .expect("spawn ran");
    assert!(!completed, "the call succeeded");
    let requests = caller_model.requests();
    assert_eq!(requests.len(), 2, "one turn to call, one to wrap up");
    assert!(
        requests[0].contains("@<agent> <task>") && requests[0].contains("`helper`"),
        "the caller's model is taught the agent through spawn: {}",
        requests[0]
    );
    assert!(
        !requests[0].contains("a2a__"),
        "and no tool of its own: {}",
        requests[0]
    );
    assert!(
        requests[1].contains(ANSWER),
        "the served agent's answer is the tool result the model reads: {}",
        requests[1]
    );
}

/// Two opens of one directory share a tool audience, so ADR-0029 decides
/// reach per agent: the open that was handed the agent teaches it and routes
/// to it, and the sibling that was handed none is neither told about it nor
/// able to reach it by guessing the name.
#[tokio::test]
async fn a_sibling_open_of_the_same_directory_cannot_reach_the_agent() {
    isolate_environment();

    let served_model = ScriptedEndpoint::start(vec![Reply::Text(ANSWER.to_string())]);
    let served_data = tempfile::tempdir().unwrap();
    let served_workspace = tempfile::tempdir().unwrap();
    let service = A2aService::open(
        ServeConfig::new(
            served_workspace.path(),
            AgentIdentity::new("helper", "Answers questions in tests."),
        )
        .with_data_dir(served_data.path())
        .with_run_defaults(
            RunDefaults::default()
                .with_base_url(&served_model.base_url)
                .with_model("test-model")
                .with_deadline(Duration::from_secs(60)),
        ),
    )
    .expect("opens");
    let server = A2aServer::bind("127.0.0.1:0".parse().unwrap(), service)
        .await
        .expect("binds loopback");
    let base = format!("http://{}", server.local_addr());
    tokio::spawn(server.serve());

    // One model endpoint, one shared runtime: the owner's run is turns 0–1,
    // the sibling's turns 2–3, and each run's first turn guesses the agent.
    let guess = Reply::ToolCall {
        name: "spawn".to_string(),
        arguments: json!({"input": "@helper what is the answer?"}).to_string(),
    };
    let caller_model = ScriptedEndpoint::start(vec![
        guess.clone(),
        Reply::Text("done".to_string()),
        guess,
        Reply::Text("done".to_string()),
    ]);
    let runtime = Arc::new(
        Runtime::builder()
            .with_base_url(&caller_model.base_url)
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds"),
    );
    let caller = tempfile::tempdir().unwrap();
    let open = |a2a: A2aConfig| {
        Workspace::builder(caller.path())
            .with_runtime(Arc::clone(&runtime))
            .with_model(ModelSelector::Id("test-model".to_string()))
            .with_context(ContextConfig::none())
            .with_memory(MemoryConfig::disabled())
            .with_a2a(a2a)
            .open()
    };
    let no_files = || A2aConfig {
        global_dir: None,
        ..A2aConfig::default()
    };
    let owner = open(no_files().with_supplied(vec![A2aAgentConfig::new("helper", &base).into()]))
        .await
        .expect("the owner opens");
    let sibling = open(no_files())
        .await
        .expect("the same directory opens again");

    let owned = owner
        .prepare("ask the helper")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the owner's run completes");
    let guessed = sibling
        .prepare("ask the helper")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the sibling's run completes");

    let spawn_failed = |events: &[Event]| {
        events.iter().find_map(|event| match event {
            Event::ToolCompleted {
                tool_name,
                is_error,
                ..
            } if tool_name == "spawn" => Some(*is_error),
            _ => None,
        })
    };
    assert_eq!(spawn_failed(owned.sink.events()), Some(false));
    assert_eq!(
        spawn_failed(guessed.sink.events()),
        Some(true),
        "a guessed name reaches nothing"
    );

    let requests = caller_model.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].contains("`helper`"), "the owner is taught it");
    assert!(
        !requests[2].contains("@<agent>"),
        "the sibling is never taught a prefix that leads nowhere: {}",
        requests[2]
    );
    assert!(
        requests[3].contains("no remote agents"),
        "and its guess is refused before anything is sent: {}",
        requests[3]
    );
    assert_eq!(
        served_model.requests().len(),
        1,
        "only the owner's task reached the served agent"
    );
}

/// The provider's credential and the token the file names are read from the
/// environment, and the global config directory would otherwise be the
/// developer's own.
fn isolate_environment() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let config = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once, before any test in this binary reads these
        // variables, and every test wants the same values.
        unsafe {
            std::env::set_var("BASIS_API_KEY", "test-key");
            std::env::set_var("BASIS_CONFIG_DIR", config);
            std::env::set_var(TOKEN_VARIABLE, TOKEN);
        }
    });
}

#[derive(Clone)]
enum Reply {
    Text(String),
    ToolCall { name: String, arguments: String },
}

/// A loopback model endpoint answering turn `n` with the script's `n`th reply,
/// and the last one for every turn after it.
struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let turns = Arc::new(AtomicUsize::new(0));
        let script = Arc::new(script);
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let (script, turns, recorded) = (
                    Arc::clone(&script),
                    Arc::clone(&turns),
                    Arc::clone(&recorded),
                );
                thread::spawn(move || answer(stream, &script, &turns, &recorded));
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
}

fn answer(
    mut stream: TcpStream,
    script: &[Reply],
    turns: &AtomicUsize,
    recorded: &Mutex<Vec<String>>,
) {
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
    recorded.lock().unwrap().push(request);
    let index = turns.fetch_add(1, Ordering::SeqCst);
    let reply = script
        .get(index)
        .or_else(|| script.last())
        .cloned()
        .expect("a non-empty script");
    let id = format!("chatcmpl_{index}");
    let (delta, finish) = match reply {
        Reply::Text(text) => (json!({"role": "assistant", "content": text}), "stop"),
        Reply::ToolCall { name, arguments } => (
            json!({"role": "assistant", "tool_calls": [{
                "index": 0, "id": format!("call_{index}"), "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]}),
            "tool_calls",
        ),
    };
    let events: [Value; 2] = [
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

/// Reads a request up to the end of its declared body.
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
