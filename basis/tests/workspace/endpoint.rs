//! A scripted loopback HTTP endpoint, and every test that drives a real turn
//! against one.
//!
//! Loopback is not "the network": no packet leaves the machine, no name is
//! resolved, and the port is whichever one the OS hands out. The endpoint
//! speaks just enough of the OpenAI `chat/completions` wire format to complete
//! a turn with no tool calls in it — which is the wire a custom base URL gets,
//! and so the wire these runs actually send. One test scripts the Responses
//! wire instead, and asks for it.
//!
//! Every workspace here is opened against a closed port with an explicit model
//! id, so nothing is contacted until a turn is actually sent — which is itself
//! evidence that opening a workspace does not talk to the provider.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use basis::{AllowAll, CollectingSink, RunOutcome, Runtime, runtime::Wire};

use super::{offline, offline_runtime, write};

#[tokio::test]
async fn an_ephemeral_workspace_runs_a_turn_and_resumes_its_own_conversation() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let endpoint = ScriptedEndpoint::start();
    let workspace = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_base_url(&endpoint.base_url))
        .open()
        .await
        .expect("opens");

    // Scoped so the run is dropped before the resume: a live run holds the
    // agent's lease, and that is true of every store rather than anything
    // this knob changed.
    let agent_id = {
        let mut run = workspace.prepare("go").expect("mints");
        let agent_id = run.agent_id().to_string();
        let report = run
            .execute_with_approver(CollectingSink::default(), AllowAll)
            .await
            .expect("the run completes");

        assert!(matches!(report.outcome, RunOutcome::Ok));
        agent_id
    };

    assert_eq!(
        workspace
            .resume(&agent_id, "again")
            .expect("the store is alive as long as the workspace is")
            .agent_id(),
        agent_id,
        "inside its workspace an ephemeral conversation behaves like any other"
    );
}

#[tokio::test]
async fn two_runs_from_one_workspace_are_driven_concurrently() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let endpoint = ScriptedEndpoint::start();
    let workspace = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_base_url(&endpoint.base_url))
        .open()
        .await
        .expect("opens");

    let mut first = workspace.prepare("one").expect("mints");
    let mut second = workspace.prepare("two").expect("mints");

    let (left, right) = tokio::join!(
        first.execute_with_approver(CollectingSink::default(), AllowAll),
        second.execute_with_approver(CollectingSink::default(), AllowAll),
    );
    let left = left.expect("the first run completes");
    let right = right.expect("the second run completes");

    assert!(matches!(left.outcome, RunOutcome::Ok));
    assert!(matches!(right.outcome, RunOutcome::Ok));
    assert_eq!(
        endpoint.served(),
        2,
        "each run makes its own request rather than sharing one"
    );

    // The endpoint answers each connection differently, so identical replies
    // would mean the two runs were somehow reading one another's turn.
    let mut answers = [
        left.final_message.expect("a final message"),
        right.final_message.expect("a final message"),
    ];
    answers.sort();
    assert_eq!(answers, ["reply-1".to_string(), "reply-2".to_string()]);
}

/// What a base URL means, and it is the question every `--base-url` user hits
/// first.
///
/// "OpenAI-compatible" in the wild means `chat/completions`: Ollama, LM Studio,
/// vLLM, llama.cpp, and every gateway in front of them serve that and nothing
/// else. `v1/responses` is OpenAI's own, and an endpoint that does not serve it
/// answers 404 to the first turn — with an error that reads like a mistyped
/// URL rather than like a wire mismatch, which is why the assertion here is on
/// the path and not only on the answer.
#[tokio::test]
async fn a_custom_endpoint_is_addressed_on_the_chat_completions_wire() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let endpoint = ScriptedEndpoint::start();
    let workspace = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_base_url(&endpoint.base_url))
        .open()
        .await
        .expect("opens");

    let report = workspace
        .prepare("go")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(report.final_message.as_deref(), Some("reply-1"));
    assert_eq!(endpoint.paths(), ["/v1/chat/completions"]);
}

/// And the way back to OpenAI's own wire, for the proxy that speaks it.
///
/// A Responses-speaking gateway was reachable by base URL before
/// `chat/completions` became the default, so there has to be a word for it —
/// otherwise the default is not a default but a removal.
#[tokio::test]
async fn a_responses_speaking_endpoint_is_reached_by_asking_for_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let endpoint = ScriptedEndpoint::start_with(responses_sse_body);
    let workspace = offline(dir.path())
        .with_runtime_builder(
            offline_runtime()
                .with_base_url(&endpoint.base_url)
                .with_wire(Wire::Responses),
        )
        .open()
        .await
        .expect("opens");

    let report = workspace
        .prepare("go")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(report.final_message.as_deref(), Some("reply-1"));
    assert_eq!(endpoint.paths(), ["/v1/responses"]);
}

/// The published URL is the one to paste, on either wire.
///
/// Every gateway advertises itself with `/v1` on the end, because that is the
/// form the OpenAI SDKs take, and both of mentra's transports append their own
/// `v1/…`. Pasting the published URL would otherwise produce `/v1/v1/…` and a
/// 404 that names nothing.
#[tokio::test]
async fn a_published_url_ending_in_v1_is_not_doubled() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let endpoint = ScriptedEndpoint::start();
    let workspace = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_base_url(format!("{}v1", endpoint.base_url)))
        .open()
        .await
        .expect("opens");

    workspace
        .prepare("go")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");

    assert_eq!(endpoint.paths(), ["/v1/chat/completions"]);
}

/// The `Authorization` header carries exactly what resolution found, and
/// nothing when it found nothing. With no key passed here, resolution reads
/// the environment — so the expectation is read from the same place, which
/// keeps this true on a workstation that exports a key as well as on one
/// that does not, and in the second case pins the claim that matters: a
/// keyless base URL is asked with no header at all, not an empty bearer.
#[tokio::test]
async fn a_base_url_is_asked_with_the_key_resolution_found_or_no_header_at_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let endpoint = ScriptedEndpoint::start();
    let workspace = offline(dir.path())
        .with_runtime_builder(
            Runtime::builder()
                .with_base_url(&endpoint.base_url)
                .with_ephemeral_history(),
        )
        .open()
        .await
        .expect("opens");

    workspace
        .prepare("go")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");

    let exported = ["BASIS_API_KEY", "OPENAI_API_KEY"]
        .into_iter()
        .find_map(|var| std::env::var(var).ok().filter(|key| !key.trim().is_empty()));
    assert_eq!(endpoint.bearers(), [exported]);
}

/// An OpenAI-compatible endpoint on loopback that completes any turn.
///
/// Every connection gets its own numbered answer, which is what lets a test
/// tell two concurrent runs apart. The listener is dropped when the endpoint
/// is, and the accept loop ends with it.
///
/// It also keeps the path each request was addressed to, because *which* wire
/// basis speaks to a custom endpoint is not visible in the answer — a run that
/// asked the wrong URL fails as a 404 the harness would never notice, and the
/// path is the only place the choice shows.
pub(crate) struct ScriptedEndpoint {
    pub(crate) base_url: String,
    served: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// What one request told the endpoint about itself.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    /// The bearer token the request carried, or `None` when it sent no
    /// `Authorization` header — which is what a keyless endpoint must see,
    /// since an empty bearer is a 401 where no header is simply a request.
    bearer: Option<String>,
}

impl ScriptedEndpoint {
    /// Speaking `chat/completions`, which is what a custom base URL gets.
    pub(crate) fn start() -> Self {
        Self::start_with(sse_body)
    }

    /// The same endpoint answering each connection from a caller-chosen
    /// script, for the turns that need another shape — a tool call, or the
    /// Responses wire a host opted into.
    pub(crate) fn start_with(script: fn(usize) -> String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let served = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));

        let counted = Arc::clone(&served);
        let recorded = Arc::clone(&seen);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                // One thread per connection, so a second request that arrives
                // while the first is still being answered is not made to wait
                // — the point of the test is that both are in flight.
                let counted = Arc::clone(&counted);
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || answer(stream, script, &counted, &recorded));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            served,
            seen,
        }
    }

    pub(crate) fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    /// The paths this endpoint was asked for, in the order they arrived.
    pub(crate) fn paths(&self) -> Vec<String> {
        self.seen().into_iter().map(|seen| seen.path).collect()
    }

    /// The bearer token each request carried, in the order they arrived.
    pub(crate) fn bearers(&self) -> Vec<Option<String>> {
        self.seen().into_iter().map(|seen| seen.bearer).collect()
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen").clone()
    }
}

/// Reads one request, records what it said about itself, and writes one
/// completed response.
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
    script: fn(usize) -> String,
    turns: &AtomicUsize,
    recorded: &Mutex<Vec<Seen>>,
) {
    let request = read_http_request(&mut stream);
    if let Some(listing) = model_listing(&request) {
        let _ = stream.write_all(listing.as_bytes());
        return;
    }
    let body = script(turns.fetch_add(1, Ordering::SeqCst) + 1);
    recorded.lock().expect("seen").push(Seen {
        path: request_path(&request).to_string(),
        bearer: request_bearer(&request),
    });

    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The token after `Authorization: Bearer`, or `None` when the request sent
/// no such header.
fn request_bearer(request: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().strip_prefix("Bearer ").map(str::to_string))
            .flatten()
    })
}

/// The target of a request line — `POST /v1/chat/completions HTTP/1.1`.
fn request_path(request: &str) -> &str {
    request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
}

/// The smallest `chat/completions` stream that is a finished assistant turn:
/// one content delta, a finish reason, then `[DONE]`. No tool calls, so
/// nothing here depends on the runtime's policy or on an approver.
fn sse_body(index: usize) -> String {
    [
        format!(
            r#"{{"id":"chatcmpl_{index}","model":"test-model","choices":[{{"index":0,"delta":{{"role":"assistant","content":"reply-{index}"}}}}]}}"#
        ),
        format!(
            r#"{{"id":"chatcmpl_{index}","choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}]}}"#
        ),
        "[DONE]".to_string(),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// The same finished turn on OpenAI's own Responses wire, for the endpoint a
/// host reaches by asking for it.
fn responses_sse_body(index: usize) -> String {
    [
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
