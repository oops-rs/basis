//! What every served-agent test needs: a scripted model, a served agent in
//! front of it, and a client calling that agent over the wire.
//!
//! The model endpoint follows the pattern `basis-tasks/tests/lifecycle.rs`
//! scripts a provider with — a loopback `chat/completions` listener answered
//! from a fixed script — reimplemented here because each crate's integration
//! tests are their own process. It also keeps every turn's request body, so
//! a test can see what the model was sent.
//!
//! Included by path from each test file, so not every helper is used by
//! every one of them.
#![allow(dead_code)]

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use basis_a2a::{A2aServer, A2aService, AgentIdentity, BearerToken, RunDefaults, ServeConfig};
use serde_json::{Value, json};

pub const TOKEN: &str = "test-token";

/// Everything one served agent needs, kept alive for the test's length.
pub struct Served {
    pub endpoint: String,
    pub base: String,
    pub card_url: String,
    pub model: ScriptedEndpoint,
    pub http: reqwest::Client,
    pub token: Option<String>,
    pub data: tempfile::TempDir,
    pub workspace: tempfile::TempDir,
}

impl Served {
    pub async fn start() -> Self {
        Self::start_with(ScriptedEndpoint::start(false), |config| config).await
    }

    /// A served agent whose config is the default one — a token, the
    /// scripted model, a minute's deadline — as `configure` changes it.
    pub async fn start_with(
        model: ScriptedEndpoint,
        configure: impl FnOnce(ServeConfig) -> ServeConfig,
    ) -> Self {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = configure(
            base_config(workspace.path(), data.path(), &model)
                .with_token(BearerToken::new(TOKEN).unwrap()),
        );
        Self::serve(model, config, data, workspace, Some(TOKEN.to_string())).await
    }

    /// A served agent with no token at all, on loopback.
    pub async fn start_anonymous(
        model: ScriptedEndpoint,
        configure: impl FnOnce(ServeConfig) -> ServeConfig,
    ) -> Self {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = configure(base_config(workspace.path(), data.path(), &model));
        Self::serve(model, config, data, workspace, None).await
    }

    /// A served agent opened from `config` exactly as given — a hosted one,
    /// say, whose state lives in `data` — called with `token`.
    pub async fn start_config(
        config: ServeConfig,
        data: tempfile::TempDir,
        token: Option<String>,
    ) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        Self::serve(
            ScriptedEndpoint::start(false),
            config,
            data,
            workspace,
            token,
        )
        .await
    }

    async fn serve(
        model: ScriptedEndpoint,
        config: ServeConfig,
        data: tempfile::TempDir,
        workspace: tempfile::TempDir,
        token: Option<String>,
    ) -> Self {
        isolate_environment();
        let service = A2aService::open(config).expect("opens");
        let server = A2aServer::bind("127.0.0.1:0".parse().unwrap(), service)
            .await
            .expect("binds loopback");
        let endpoint = server.endpoint().to_string();
        let base = format!("http://{}", server.local_addr());
        let card_url = format!("{base}{}", basis_a2a::AGENT_CARD_PATH);
        tokio::spawn(server.serve());
        Self {
            endpoint,
            base,
            card_url,
            model,
            http: reqwest::Client::new(),
            token,
            data,
            workspace,
        }
    }

    /// Calls as `token` rather than the default one.
    pub fn as_token(&self, token: &str) -> Caller<'_> {
        Caller {
            served: self,
            token: Some(token.to_string()),
        }
    }

    pub fn caller(&self) -> Caller<'_> {
        Caller {
            served: self,
            token: self.token.clone(),
        }
    }

    pub async fn post(&self, body: Value) -> reqwest::Response {
        self.caller().post(body).await
    }

    pub async fn call(&self, method: &str, params: Value) -> Value {
        self.caller().call(method, params).await
    }

    pub async fn result(&self, method: &str, params: Value) -> Value {
        self.caller().result(method, params).await
    }

    pub async fn error_code(&self, method: &str, params: Value) -> i32 {
        self.caller().error_code(method, params).await
    }

    pub async fn wait_for(&self, id: &str, state: &str) -> Value {
        self.caller().wait_for(id, state).await
    }

    pub async fn card(&self) -> Value {
        reqwest::get(&self.card_url)
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

/// One principal's view of a served agent.
pub struct Caller<'a> {
    pub served: &'a Served,
    pub token: Option<String>,
}

impl Caller<'_> {
    pub fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let request = self
            .served
            .http
            .request(method, url)
            .header("A2A-Version", "1.0");
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    pub async fn post(&self, body: Value) -> reqwest::Response {
        self.request(reqwest::Method::POST, &self.served.endpoint)
            .json(&body)
            .send()
            .await
            .expect("the server answers")
    }

    pub async fn call(&self, method: &str, params: Value) -> Value {
        let response = self
            .post(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }

    pub async fn result(&self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params).await;
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    pub async fn error_code(&self, method: &str, params: Value) -> i32 {
        let reply = self.call(method, params).await;
        reply["error"]["code"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok())
            .unwrap_or_else(|| panic!("{method} should have failed: {reply}"))
    }

    /// Polls `GetTask` until `id` reaches `state`.
    pub async fn wait_for(&self, id: &str, state: &str) -> Value {
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

/// The config every served agent in these tests starts from.
pub fn base_config(workspace: &Path, data: &Path, model: &ScriptedEndpoint) -> ServeConfig {
    ServeConfig::new(
        workspace,
        AgentIdentity::new("helper", "Answers questions in tests."),
    )
    .with_data_dir(data)
    .with_run_defaults(
        RunDefaults::default()
            .with_base_url(&model.base_url)
            .with_model("test-model")
            .with_deadline(Duration::from_secs(60)),
    )
}

pub fn user_message(text: &str, context_id: Option<&str>) -> Value {
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

pub fn artifact_text(task: &Value) -> &str {
    task["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text artifact: {task}"))
}

/// The events of an SSE body, each `data:` line parsed.
pub fn sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data.trim()).unwrap())
        .collect()
}

/// The provider's credential is read from the environment, and the global
/// config directory would otherwise be the developer's own.
pub fn isolate_environment() {
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

/// One scripted model turn.
#[derive(Clone, Debug)]
pub enum Reply {
    /// `reply-<n>`, `n` counting every turn this endpoint has answered.
    Numbered,
    Text(String),
    ToolCall {
        name: String,
        arguments: Value,
    },
}

/// A loopback model endpoint answering turn `n` with the script's `n`th
/// reply, and [`Reply::Numbered`] once the script runs out.
pub struct ScriptedEndpoint {
    pub base_url: String,
    held: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    /// `held`: every turn waits at the endpoint until [`release`](Self::release).
    pub fn start(held: bool) -> Self {
        Self::scripted(held, Vec::new())
    }

    pub fn scripted(held: bool, script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let held = Arc::new(AtomicBool::new(held));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let turns = Arc::new(AtomicUsize::new(0));
        let script = Arc::new(script);
        let shared = (Arc::clone(&held), Arc::clone(&requests));
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let (held, requests) = (Arc::clone(&shared.0), Arc::clone(&shared.1));
                let (turns, script) = (Arc::clone(&turns), Arc::clone(&script));
                thread::spawn(move || answer(stream, &held, &requests, &turns, &script));
            }
        });
        Self {
            base_url: format!("http://{address}/"),
            held,
            requests,
        }
    }

    pub fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Waits until the model has been called `turns` times.
    pub async fn reached(&self, turns: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.requests().len() < turns {
            assert!(Instant::now() < deadline, "the model was never called");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn answer(
    mut stream: TcpStream,
    held: &AtomicBool,
    requests: &Mutex<Vec<String>>,
    turns: &AtomicUsize,
    script: &[Reply],
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
    requests.lock().unwrap().push(request);
    while held.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(10));
    }
    let index = turns.fetch_add(1, Ordering::SeqCst);
    let reply = script.get(index).cloned().unwrap_or(Reply::Numbered);
    let id = format!("chatcmpl_{index}");
    let (delta, finish) = match reply {
        Reply::Numbered => (
            json!({"role": "assistant", "content": format!("reply-{}", index + 1)}),
            "stop",
        ),
        Reply::Text(text) => (json!({"role": "assistant", "content": text}), "stop"),
        Reply::ToolCall { name, arguments } => (
            json!({"role": "assistant", "tool_calls": [{
                "index": 0, "id": format!("call_{index}"), "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
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
pub fn read_http_request(stream: &mut TcpStream) -> String {
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
