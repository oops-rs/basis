//! A served agent, called over the wire the way any A2A client calls it:
//! the card fetched from its well-known path, JSON-RPC posted to the
//! endpoint the card names, and a scripted model behind it.
//!
//! The model endpoint follows the pattern `basis-tasks/tests/lifecycle.rs`
//! scripts a provider with — a loopback `chat/completions` listener answered
//! from a fixed script — reimplemented here because each crate's integration
//! tests are their own process. It also keeps every turn's request body, so
//! a test can see what the model was sent.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use basis_a2a::{
    A2aServer, A2aService, AgentIdentity, BearerToken, RunDefaults, ServeConfig, a2a::error_code,
};
use serde_json::{Value, json};

const TOKEN: &str = "test-token";

/// Everything one served agent needs, kept alive for the test's length.
struct Served {
    endpoint: String,
    card_url: String,
    model: ScriptedEndpoint,
    http: reqwest::Client,
    _data: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

impl Served {
    async fn start() -> Self {
        Self::start_with(ScriptedEndpoint::start(false)).await
    }

    async fn start_with(model: ScriptedEndpoint) -> Self {
        isolate_environment();
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = ServeConfig::new(
            workspace.path(),
            AgentIdentity::new("helper", "Answers questions in tests."),
        )
        .with_data_dir(data.path())
        .with_token(BearerToken::new(TOKEN).unwrap())
        .with_run_defaults(
            RunDefaults::default()
                .with_base_url(&model.base_url)
                .with_model("test-model")
                .with_deadline(Duration::from_secs(60)),
        );
        let service = A2aService::open(config).expect("opens");
        let server = A2aServer::bind("127.0.0.1:0".parse().unwrap(), service)
            .await
            .expect("binds loopback");
        let endpoint = server.endpoint().to_string();
        let card_url = format!(
            "http://{}{}",
            server.local_addr(),
            basis_a2a::AGENT_CARD_PATH
        );
        tokio::spawn(server.serve());
        Self {
            endpoint,
            card_url,
            model,
            http: reqwest::Client::new(),
            _data: data,
            _workspace: workspace,
        }
    }

    async fn post(&self, body: Value) -> reqwest::Response {
        self.http
            .post(&self.endpoint)
            .bearer_auth(TOKEN)
            .header("A2A-Version", "1.0")
            .json(&body)
            .send()
            .await
            .expect("the server answers")
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let response = self
            .post(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }

    async fn result(&self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params).await;
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    async fn error_code(&self, method: &str, params: Value) -> i32 {
        let reply = self.call(method, params).await;
        reply["error"]["code"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok())
            .unwrap_or_else(|| panic!("{method} should have failed: {reply}"))
    }

    /// Polls `GetTask` until `id` reaches `state`.
    async fn wait_for(&self, id: &str, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let task = self.result("GetTask", json!({"id": id})).await;
            if task["status"]["state"] == state {
                return task;
            }
            assert!(Instant::now() < deadline, "never reached {state}: {task}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn user_message(text: &str, context_id: Option<&str>) -> Value {
    let mut message = json!({
        "messageId": format!("m-{text}"),
        "role": "ROLE_USER",
        "parts": [{"text": text}],
    });
    if let Some(context_id) = context_id {
        message["contextId"] = json!(context_id);
    }
    message
}

fn artifact_text(task: &Value) -> &str {
    task["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text artifact: {task}"))
}

#[tokio::test]
async fn the_card_is_public_and_names_the_endpoint_it_is_served_on() {
    let served = Served::start().await;
    let card: Value = reqwest::get(&served.card_url)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(card["name"], "helper");
    assert_eq!(card["supportedInterfaces"][0]["url"], served.endpoint);
    assert_eq!(card["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
    assert_eq!(card["capabilities"]["streaming"], true);
    assert!(card["securitySchemes"]["bearer"].is_object(), "{card}");
}

#[tokio::test]
async fn a_call_without_the_token_or_the_version_is_refused() {
    let served = Served::start().await;
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {}});

    let anonymous = served
        .http
        .post(&served.endpoint)
        .header("A2A-Version", "1.0")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    let unversioned: Value = served
        .http
        .post(&served.endpoint)
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        unversioned["error"]["code"],
        error_code::VERSION_NOT_SUPPORTED
    );
}

#[tokio::test]
async fn a_blocking_send_answers_with_the_completed_task_and_its_result() {
    let served = Served::start().await;
    let result = served
        .result(
            "SendMessage",
            json!({"message": user_message("hello", None)}),
        )
        .await;
    let task = &result["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
    assert_eq!(artifact_text(task), "reply-1");
    assert_eq!(task["history"][0]["messageId"], "m-hello");

    let id = task["id"].as_str().unwrap();
    let fetched = served.result("GetTask", json!({"id": id})).await;
    assert_eq!(artifact_text(&fetched), "reply-1");

    let listed = served.result("ListTasks", json!({})).await;
    assert_eq!(listed["totalSize"], 1);
    assert_eq!(listed["nextPageToken"], "");
    assert!(
        listed["tasks"][0].get("artifacts").is_none(),
        "artifacts are omitted unless asked for: {listed}"
    );
}

#[tokio::test]
async fn a_follow_up_in_the_same_context_continues_the_conversation() {
    let served = Served::start().await;
    let first = served
        .result(
            "SendMessage",
            json!({"message": user_message("first", None)}),
        )
        .await;
    let context_id = first["task"]["contextId"].as_str().unwrap().to_string();

    let second = served
        .result(
            "SendMessage",
            json!({"message": user_message("second", Some(&context_id))}),
        )
        .await;
    assert_eq!(second["task"]["contextId"], context_id.as_str());
    assert_ne!(second["task"]["id"], first["task"]["id"]);
    assert_eq!(second["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    let sent = served.model.requests();
    let last = sent.last().expect("the model was called");
    assert!(
        last.contains("first") && last.contains("reply-1") && last.contains("second"),
        "the follow-up's turn carries the earlier exchange: {last}"
    );

    let in_context = served
        .result("ListTasks", json!({"contextId": context_id}))
        .await;
    assert_eq!(in_context["totalSize"], 2);
}

#[tokio::test]
async fn an_unknown_context_or_a_reply_to_a_settled_task_is_refused() {
    let served = Served::start().await;
    assert_eq!(
        served
            .error_code(
                "SendMessage",
                json!({"message": user_message("hi", Some("not-a-context"))})
            )
            .await,
        error_code::INVALID_PARAMS
    );

    let done = served
        .result("SendMessage", json!({"message": user_message("hi", None)}))
        .await;
    let mut reply = user_message("again", None);
    reply["taskId"] = done["task"]["id"].clone();
    assert_eq!(
        served
            .error_code("SendMessage", json!({"message": reply}))
            .await,
        error_code::UNSUPPORTED_OPERATION
    );
    assert_eq!(
        served.error_code("GetTask", json!({"id": "nope"})).await,
        error_code::TASK_NOT_FOUND
    );
}

#[tokio::test]
async fn returning_immediately_hands_back_a_task_that_settles_later() {
    let served = Served::start_with(ScriptedEndpoint::start(true)).await;
    let result = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("slow", None),
                "configuration": {"returnImmediately": true},
            }),
        )
        .await;
    let task = &result["task"];
    let state = task["status"]["state"].as_str().unwrap();
    assert!(
        state == "TASK_STATE_SUBMITTED" || state == "TASK_STATE_WORKING",
        "{task}"
    );
    let id = task["id"].as_str().unwrap();

    // One conversation, one executor: a second message while the first is
    // still running is the caller's to wait out.
    let context_id = task["contextId"].as_str().unwrap();
    assert_eq!(
        served
            .error_code(
                "SendMessage",
                json!({"message": user_message("meanwhile", Some(context_id))})
            )
            .await,
        error_code::UNSUPPORTED_OPERATION
    );

    served.model.release();
    let settled = served.wait_for(id, "TASK_STATE_COMPLETED").await;
    assert_eq!(artifact_text(&settled), "reply-1");
}

#[tokio::test]
async fn a_streamed_send_opens_on_the_task_and_closes_on_its_terminal_state() {
    let served = Served::start().await;
    let response = served
        .post(json!({
            "jsonrpc": "2.0", "id": 7, "method": "SendStreamingMessage",
            "params": {"message": user_message("stream me", None)},
        }))
        .await;
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let body = response.text().await.unwrap();
    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data.trim()).unwrap())
        .collect();

    assert!(events.iter().all(|event| event["id"] == 7));
    assert!(events[0]["result"]["task"].is_object(), "{body}");
    let artifact = events
        .iter()
        .find_map(|event| event["result"].get("artifactUpdate"))
        .unwrap_or_else(|| panic!("the result is streamed: {body}"));
    assert_eq!(artifact["artifact"]["parts"][0]["text"], "reply-1");
    let last = &events.last().unwrap()["result"]["statusUpdate"];
    assert_eq!(last["status"]["state"], "TASK_STATE_COMPLETED", "{body}");
}

#[tokio::test]
async fn cancelling_a_running_task_settles_it_canceled() {
    let served = Served::start_with(ScriptedEndpoint::start(true)).await;
    let result = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("never mind", None),
                "configuration": {"returnImmediately": true},
            }),
        )
        .await;
    let id = result["task"]["id"].as_str().unwrap().to_string();

    // Cancellation lands at a turn boundary (ADR-0019): the held turn has to
    // end before the task can.
    let canceller = {
        let endpoint = served.endpoint.clone();
        let id = id.clone();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(&endpoint)
                .bearer_auth(TOKEN)
                .header("A2A-Version", "1.0")
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "CancelTask", "params": {"id": id}}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    served.model.release();
    let cancelled = canceller.await.unwrap();
    assert!(cancelled.get("error").is_none(), "{cancelled}");

    served.wait_for(&id, "TASK_STATE_CANCELED").await;
    assert_eq!(
        served.error_code("CancelTask", json!({"id": id})).await,
        error_code::TASK_NOT_CANCELABLE
    );
}

#[tokio::test]
async fn unsupported_features_are_refused_with_the_specs_own_errors() {
    let served = Served::start().await;
    assert_eq!(
        served
            .error_code(
                "CreateTaskPushNotificationConfig",
                json!({"taskId": "x", "url": "https://example.com"})
            )
            .await,
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
    );
    assert_eq!(
        served.error_code("GetExtendedAgentCard", json!({})).await,
        error_code::UNSUPPORTED_OPERATION
    );
    assert_eq!(
        served.error_code("NoSuchMethod", json!({})).await,
        error_code::METHOD_NOT_FOUND
    );
    let file = json!({
        "messageId": "m-file", "role": "ROLE_USER",
        "parts": [{"url": "https://example.com/a.png", "mediaType": "image/png"}],
    });
    assert_eq!(
        served
            .error_code("SendMessage", json!({"message": file}))
            .await,
        error_code::CONTENT_TYPE_NOT_SUPPORTED
    );
}

#[tokio::test]
async fn a_non_loopback_address_without_a_token_is_refused_before_listening() {
    isolate_environment();
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = ServeConfig::new(workspace.path(), AgentIdentity::new("open", "No token."))
        .with_data_dir(data.path());
    let service = A2aService::open(config).unwrap();
    let refused = A2aServer::bind("0.0.0.0:0".parse().unwrap(), service).await;
    assert!(matches!(
        refused,
        Err(basis_a2a::BindError::Unauthenticated(_))
    ));
}

/// The provider's credential is read from the environment, and the global
/// config directory would otherwise be the developer's own.
fn isolate_environment() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let config = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once, before any test in this binary reads either
        // variable, and every test wants the same values.
        unsafe {
            std::env::set_var("BASIS_API_KEY", "test-key");
            std::env::set_var("BASIS_CONFIG_DIR", config);
        }
    });
}

/// A loopback model endpoint answering every turn `reply-<n>`.
struct ScriptedEndpoint {
    base_url: String,
    held: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    /// `held`: every turn waits at the endpoint until [`release`](Self::release).
    fn start(held: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let held = Arc::new(AtomicBool::new(held));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let served = Arc::new(AtomicUsize::new(0));
        let turns = (Arc::clone(&held), Arc::clone(&requests), served);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let turns = (
                    Arc::clone(&turns.0),
                    Arc::clone(&turns.1),
                    Arc::clone(&turns.2),
                );
                thread::spawn(move || answer(stream, &turns.0, &turns.1, &turns.2));
            }
        });
        Self {
            base_url: format!("http://{address}/"),
            held,
            requests,
        }
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn answer(
    mut stream: TcpStream,
    held: &AtomicBool,
    requests: &Mutex<Vec<String>>,
    served: &AtomicUsize,
) {
    let request = read_http_request(&mut stream);
    let first_line = request.lines().next().unwrap_or_default();
    if first_line.starts_with("GET ") && first_line.contains("/models") {
        let body = r#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#;
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        return;
    }
    requests.lock().unwrap().push(request);
    while held.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(10));
    }
    let index = served.fetch_add(1, Ordering::SeqCst) + 1;
    let id = format!("chatcmpl_{index}");
    let events = [
        json!({
            "id": id, "model": "test-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": format!("reply-{index}")}}]
        }),
        json!({"id": id, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ];
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect();
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
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
