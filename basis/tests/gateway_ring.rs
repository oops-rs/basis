//! A ring of gateways through basis's supported builder seam.
//!
//! Two scripted loopback endpoints speaking the Responses wire: the preferred
//! one refuses every request with `401`, the way a gateway handed its
//! neighbour's key does, and the second one answers. A run against the ring
//! completes on the second gateway, with that gateway's own key, and the
//! observer says so — the whole reason a host would wire one.
//!
//! This test deliberately uses only `basis` exports.

use std::{
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use basis::{
    AllowAll, CollectingSink, ModelSelector, RunOutcome, Runtime, Workspace,
    runtime::{FailureKind, GatewayMember, GatewayRingEvent, GatewayRingPolicy, Wire},
};

#[tokio::test]
async fn a_run_completes_on_the_next_gateway_when_the_preferred_one_rejects_it() {
    let sick = ScriptedEndpoint::start(Answer::Refuse(401));
    let well = ScriptedEndpoint::start(Answer::Serve);
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);

    let runtime = Runtime::builder()
        .with_wire(Wire::Responses)
        .with_gateway_ring([
            GatewayMember::new(&sick.base_url).with_api_key("sick-key"),
            GatewayMember::new(&well.base_url).with_api_key("well-key"),
        ])
        .with_gateway_ring_policy(GatewayRingPolicy::sticky())
        .with_gateway_ring_observer(move |event| seen.lock().expect("events").push(event))
        .with_ephemeral_history();

    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = Workspace::builder(dir.path())
        .with_runtime_builder(runtime)
        .with_model(ModelSelector::Id("test-model".to_string()))
        .without_discovery()
        .open()
        .await
        .expect("opens");
    let report = workspace
        .prepare("use whichever gateway answers")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(report.provider, "openai");
    assert_eq!(report.final_message.as_deref(), Some("reply-1"));

    // The model lookup was the first call through the ring: the preferred
    // gateway rejected it and was left at once; the turn never went there.
    assert_eq!(sick.requests().len(), 1);
    assert!(sick.requests()[0].starts_with("GET "));
    let well_requests = well.requests();
    assert_eq!(well_requests.len(), 2);
    assert!(well_requests[1].starts_with("POST "));

    // Each member carries its own key.
    assert_eq!(bearer(&sick.requests()[0]).as_deref(), Some("sick-key"));
    assert!(
        well_requests
            .iter()
            .all(|request| bearer(request).as_deref() == Some("well-key"))
    );

    let events = events.lock().expect("events").clone();
    assert!(matches!(
        &events[0],
        GatewayRingEvent::MemberFailed { member, kind: FailureKind::Immediate, .. }
            if member.index == 0 && member.label.contains(&sick.base_url)
    ));
    assert!(matches!(
        &events[1],
        GatewayRingEvent::Rotated { from, to, after_failures: 0 }
            if from.index == 0 && to.index == 1 && to.label.contains(&well.base_url)
    ));
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn a_member_reads_its_key_from_a_file_and_hears_the_model_without_its_namespace() {
    let relay = ScriptedEndpoint::start(Answer::Serve);
    let dir = tempfile::tempdir().expect("tempdir");
    let key_file = dir.path().join("relay-key");
    std::fs::write(&key_file, "relay-key\n").expect("write key");

    let runtime = Runtime::builder()
        .with_wire(Wire::Responses)
        .with_gateway_ring([GatewayMember::new(&relay.base_url)
            .with_api_key_file(&key_file)
            .with_model_prefix_stripped("vendor/")])
        .with_ephemeral_history();
    let workspace = Workspace::builder(dir.path())
        .with_runtime_builder(runtime)
        .with_model(ModelSelector::Id("vendor/test-model".to_string()))
        .without_discovery()
        .open()
        .await
        .expect("opens");
    let report = workspace
        .prepare("answer under the relay's own id")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    let turn = relay
        .requests()
        .into_iter()
        .find(|request| request.starts_with("POST "))
        .expect("a turn reached the relay");
    assert!(turn.contains(r#""model":"test-model""#), "{turn}");
    assert_eq!(bearer(&turn).as_deref(), Some("relay-key"));
}

#[tokio::test]
async fn request_fields_from_the_runtime_and_the_member_both_reach_the_turn() {
    let gateway = ScriptedEndpoint::start(Answer::Serve);
    let runtime = Runtime::builder()
        .with_wire(Wire::Responses)
        .with_request_fields(fields(r#"{"metadata_tag": "runtime"}"#))
        .with_gateway_ring([GatewayMember::new(&gateway.base_url)
            .with_api_key("key")
            .with_request_fields(fields(
                r#"{"extra_body": {"thinking": {"type": "disabled"}}}"#,
            ))])
        .with_ephemeral_history();

    let body = one_turn(runtime).await;
    let turn = turn_body(&gateway);

    assert_eq!(body, "reply-1");
    assert_eq!(turn["metadata_tag"], "runtime");
    assert_eq!(turn["extra_body"]["thinking"]["type"], "disabled");
}

#[tokio::test]
async fn request_fields_reach_a_lone_base_url() {
    let gateway = ScriptedEndpoint::start(Answer::Serve);
    let runtime = Runtime::builder()
        .with_wire(Wire::Responses)
        .with_base_url(&gateway.base_url)
        .with_api_key("key")
        .with_request_fields(fields(
            r#"{"extra_body": {"thinking": {"type": "disabled"}}}"#,
        ))
        .with_ephemeral_history();

    one_turn(runtime).await;

    assert_eq!(
        turn_body(&gateway)["extra_body"]["thinking"]["type"],
        "disabled"
    );
}

#[test]
fn a_request_field_named_by_the_runtime_and_a_member_is_refused_at_build() {
    let error = Runtime::builder()
        .with_wire(Wire::Responses)
        .with_request_fields(fields(r#"{"extra_body": {}}"#))
        .with_gateway_ring([
            GatewayMember::new("http://127.0.0.1:1/v1"),
            GatewayMember::new("http://127.0.0.1:2/v1")
                .with_request_fields(fields(r#"{"extra_body": {"x": 1}}"#)),
        ])
        .with_ephemeral_history()
        .build()
        .expect_err("neither value may silently win");

    let message = error.to_string();
    assert!(message.contains("gateway #1"), "{message}");
    assert!(message.contains("`extra_body`"), "{message}");
}

#[test]
fn request_fields_for_a_preset_with_no_base_url_are_refused_rather_than_dropped() {
    let error = Runtime::builder()
        .with_provider(basis::BuiltinProvider::OpenAI)
        .with_api_key("key")
        .with_request_fields(fields(r#"{"extra_body": {}}"#))
        .with_ephemeral_history()
        .build()
        .expect_err("basis does not build a preset's requests");

    assert!(error.to_string().contains("no base URL"), "{error}");
}

fn fields(json: &str) -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_str(json).expect("an object")
}

/// Runs one plain turn on `runtime` and returns its final message.
async fn one_turn(runtime: basis::RuntimeBuilder) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = Workspace::builder(dir.path())
        .with_runtime_builder(runtime)
        .with_model(ModelSelector::Id("test-model".to_string()))
        .without_discovery()
        .open()
        .await
        .expect("opens");
    let report = workspace
        .prepare("say anything")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");
    assert!(matches!(report.outcome, RunOutcome::Ok));
    report.final_message.expect("a reply")
}

/// The JSON body of the first turn `endpoint` was sent.
fn turn_body(endpoint: &ScriptedEndpoint) -> serde_json::Value {
    let turn = endpoint
        .requests()
        .into_iter()
        .find(|request| request.starts_with("POST "))
        .expect("a turn reached the endpoint");
    let body = turn.split("\r\n\r\n").nth(1).expect("a body");
    serde_json::from_str(body).expect("a JSON body")
}

/// The token after `Authorization: Bearer`, or `None` when the request sent
/// no such header.
fn bearer(request: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().strip_prefix("Bearer ").map(str::to_string))
            .flatten()
    })
}

#[derive(Clone, Copy)]
enum Answer {
    /// A model listing to a `GET`, a finished Responses turn to a `POST`.
    Serve,
    /// This status to everything.
    Refuse(u16),
}

struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ScriptedEndpoint {
    fn start(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        listener
            .set_nonblocking(true)
            .expect("make test endpoint nonblocking");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let recorded = Arc::clone(&requests);
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut turns = 0_usize;
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("accepted request stream should be blocking");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("accepted request stream should have a read timeout");
                        let request = read_http_request(&mut stream);
                        recorded.lock().expect("requests").push(request.clone());
                        match answer {
                            Answer::Refuse(status) => refuse(&mut stream, status),
                            Answer::Serve if request.starts_with("GET ") => {
                                answer_models(&mut stream);
                            }
                            Answer::Serve => {
                                turns += 1;
                                answer_turn(&mut stream, turns);
                            }
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept test request: {error}"),
                }
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

impl Drop for ScriptedEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("test endpoint stops");
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let read = stream.read(&mut buffer).expect("read request");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if header_end.is_none()
            && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = index + 4;
            header_end = Some(end);
            let headers = String::from_utf8_lossy(&bytes[..end]);
            content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or_default();
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            break;
        }
    }

    String::from_utf8(bytes).expect("request should be utf8")
}

fn refuse(stream: &mut TcpStream, status: u16) {
    let body = r#"{"error":{"message":"not your key","type":"invalid_request_error"}}"#;
    let response = format!(
        "HTTP/1.1 {status} Refused\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("write refusal");
}

fn answer_models(stream: &mut TcpStream) {
    let body = r#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#;
    answer(stream, "application/json", body);
}

fn answer_turn(stream: &mut TcpStream, index: usize) {
    let body = [
        format!(
            r#"{{"type":"response.created","response":{{"id":"resp_{index}","model":"test-model","status":"in_progress"}}}}"#
        ),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","content":[]}}"#.to_string(),
        format!(
            r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"message","content":[{{"type":"output_text","text":"reply-{index}"}}]}}}}"#
        ),
        format!(
            r#"{{"type":"response.completed","response":{{"id":"resp_{index}","model":"test-model","status":"completed"}}}}"#
        ),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>();
    answer(stream, "text/event-stream", &body);
}

fn answer(stream: &mut TcpStream, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("write response");
}
