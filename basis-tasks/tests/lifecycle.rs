//! The point of the extraction, proven from Rust: a host that is not the CLI
//! opens a workspace's tasks, spawns one, asks it something, waits for the
//! durable terminal, and lists it back — with no `basis` binary anywhere in
//! the process tree.
//!
//! The endpoint below follows the same pattern `basis-cli/tests/attach.rs`
//! and `basis/tests/workspace.rs` script a provider with: a loopback
//! `chat/completions`-speaking listener, answered from a fixed script. It is
//! reimplemented here rather than shared, the way it already is between
//! those two — each crate's integration tests are their own process.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use basis_tasks::{RunSpec, Tasks, WaitOutcome};
use serde_json::json;

/// `Runtime::builder()`'s default credential source when no explicit key is
/// given — read once per test process, harmlessly redundant if more than one
/// test sets it, since every caller here wants the same fake value.
fn stub_api_key() {
    // SAFETY: this test binary is single-purpose and every test that reaches
    // a scripted endpoint wants the same fake key; nothing here reads a real
    // credential.
    unsafe { std::env::set_var("BASIS_API_KEY", "test-key") };
}

#[tokio::test]
async fn spawn_ask_wait_and_list_from_rust_with_no_binary_involved() {
    stub_api_key();
    let endpoint = ScriptedEndpoint::start();
    let data_dir = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");

    let tasks =
        Tasks::open_at(data_dir.path(), workspace.path()).expect("opens with no env dependency");

    let spec = RunSpec::new("hello")
        .with_base_url(&endpoint.base_url)
        .with_model("test-model")
        .with_deadline(Duration::from_secs(60))
        // No `PromptHost` is configured on `tasks` below — this host answers
        // for itself rather than asking, which is the ordinary shape for a
        // process with no terminal (or other approving party) behind it.
        .with_approve(basis_tasks::Approve::Always);
    let handle = tasks.spawn(spec).expect("spawn mints a handle immediately");

    // Nothing has attached yet: the handle is good, but nothing has run.
    assert!(
        tasks
            .terminal(&handle)
            .expect("reads the terminal")
            .is_none(),
        "a freshly spawned task is resumable, not settled"
    );

    // `ask` attaches (nothing else holds the lock), drives the task's first
    // turn and then this message's, and returns the correlated reply.
    let reply = tasks
        .ask(
            &handle,
            None,
            "what should I do next?",
            Duration::from_secs(30),
        )
        .await
        .expect("ask completes");
    let WaitOutcome::Terminal(reply_payload) = reply.outcome else {
        panic!("a scripted endpoint always answers inside the timeout");
    };
    assert_eq!(reply_payload["state"], "succeeded", "{reply_payload}");
    assert_eq!(reply_payload["message"], reply.message_id);
    assert!(
        reply_payload["result"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "{reply_payload}"
    );

    // The task settles once its inbox empties — repeatable, and readable
    // without attaching, the way `basis wait` on a settled task is.
    let outcome = tasks
        .wait(&handle, None, Duration::from_secs(5), None)
        .await
        .expect("wait completes");
    let WaitOutcome::Terminal(terminal) = outcome else {
        panic!("a settled task's terminal is read straight off disk");
    };
    assert_eq!(terminal["state"], "succeeded");

    // And it is the workspace's task: `list` finds it, in the terminal state
    // the files say it is in, without ever touching an attach lock.
    let summaries = tasks.list().expect("list scans the workspace");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].task, handle.as_str());
    assert_eq!(summaries[0].state, "succeeded");

    assert_eq!(
        endpoint.served(),
        2,
        "the initial prompt, then the asked message"
    );
}

/// A wait's deadline is honored at the turn boundary, not by cutting a turn
/// short (ADR-0019: cancellation and deadlines are boundary-granular). The
/// turn in flight when the wait's own deadline passes runs to its end under
/// the *task's* deadline; the next one does not start. The task is left
/// exactly as a crash between turns would leave it — no terminal record, the
/// attach lock free — and a later wait picks it up where the first stopped.
#[tokio::test]
async fn a_wait_past_its_deadline_stops_between_turns_and_leaves_the_task_resumable() {
    stub_api_key();
    // The endpoint holds every turn until told otherwise, so the test — not
    // the clock — decides that the wait's deadline passes while its first
    // turn is in flight: the check under test is the one at the boundary
    // after it.
    let endpoint = ScriptedEndpoint::start_held();
    let data_dir = tempfile::tempdir().expect("tempdir");
    let workspace = tempfile::tempdir().expect("tempdir");
    let tasks = Tasks::open_at(data_dir.path(), workspace.path()).expect("opens");

    let spec = RunSpec::new("hello")
        .with_base_url(&endpoint.base_url)
        .with_model("test-model")
        .with_deadline(Duration::from_secs(60))
        .with_approve(basis_tasks::Approve::Always);
    let handle = tasks.spawn(spec).expect("spawn mints a handle immediately");
    // Two more turns queued behind the prompt: three in all, and a wait that
    // ignored its own deadline would run every one of them.
    tasks.send(&handle, "one").expect("enqueue");
    tasks.send(&handle, "two").expect("enqueue");

    // Long enough for the attach to open the workspace and start its first
    // turn (no turn starts once the wait is already over — the rule under
    // test cuts both ways), short enough that the test holds that turn past
    // it without waiting long.
    let timeout = Duration::from_secs(2);
    let started = Instant::now();
    let waiter = {
        let tasks = tasks.clone();
        let handle = handle.clone();
        tokio::spawn(async move { tasks.wait(&handle, None, timeout, None).await })
    };
    // The first turn is in flight (its request has reached the endpoint) and
    // the wait's own deadline is behind it before the turn is allowed to end.
    while endpoint.arrived() == 0 {
        assert!(
            started.elapsed() < timeout,
            "the first turn never reached the endpoint inside the wait's timeout"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(timeout.saturating_sub(started.elapsed()) + Duration::from_millis(100))
        .await;
    endpoint.release();

    let outcome = waiter
        .await
        .expect("waiter task ends")
        .expect("wait completes");
    let elapsed = started.elapsed();
    assert_eq!(
        outcome,
        WaitOutcome::TimedOut { attached: false },
        "the wait ends at its own deadline, and releases the lock before saying so"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the wait returned at the first boundary after its deadline, not the task's \
         (took {elapsed:?})"
    );
    assert_eq!(
        endpoint.served(),
        1,
        "the in-flight turn finished; no other started"
    );
    assert!(
        tasks.terminal(&handle).expect("reads").is_none(),
        "a wait that timed out settles nothing: the task stays resumable"
    );
    assert!(
        !tasks.is_attached(&handle).expect("probes"),
        "the attach lock is released with the wait"
    );

    // The second wait resumes from the committed turn and finishes the job.
    let outcome = tasks
        .wait(&handle, None, Duration::from_secs(30), None)
        .await
        .expect("wait completes");
    let WaitOutcome::Terminal(terminal) = outcome else {
        panic!("a resumed task runs to its terminal inside a generous timeout");
    };
    assert_eq!(terminal["state"], "succeeded", "{terminal}");
    assert_eq!(terminal["result"], "reply-3", "{terminal}");
    assert_eq!(
        endpoint.served(),
        3,
        "the two queued messages ran on the second attach, and nothing was rerun"
    );
}

/// An endpoint scripted to answer every connection with a fixed assistant
/// message on the `chat/completions` wire — the smallest shape a finished
/// turn needs. The listener is dropped, and its accept loop ends, with the
/// endpoint.
struct ScriptedEndpoint {
    base_url: String,
    /// Turn requests that have reached the endpoint, held or not.
    arrived: Arc<AtomicUsize>,
    /// Turns answered.
    served: Arc<AtomicUsize>,
    /// While set, every turn waits at the endpoint for [`release`](Self::release).
    held: Arc<AtomicBool>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        Self::start_with(false)
    }

    /// As [`start`](Self::start), but every turn is held at the endpoint
    /// until [`release`](Self::release) — a turn that takes exactly as long
    /// as the test wants, for one that needs a wait to run out while a turn
    /// is in flight.
    fn start_held() -> Self {
        Self::start_with(true)
    }

    fn start_with(held: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let arrived = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(AtomicBool::new(held));

        let turns = Turns {
            arrived: Arc::clone(&arrived),
            served: Arc::clone(&served),
            held: Arc::clone(&held),
        };
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let turns = turns.clone();
                thread::spawn(move || answer(stream, &turns));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            arrived,
            served,
            held,
        }
    }

    fn arrived(&self) -> usize {
        self.arrived.load(Ordering::SeqCst)
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    /// Lets every held turn — and every turn after — answer.
    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }
}

/// The endpoint's turn accounting, shared with each connection's thread.
#[derive(Clone)]
struct Turns {
    arrived: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
    held: Arc<AtomicBool>,
}

/// A pinned model is looked up in the provider's listing before the first
/// turn, which is one `GET …/models` per run that is neither a turn nor
/// scripted. Answered with a listing that names the test model, and never
/// counted as one.
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

fn answer(mut stream: TcpStream, turns: &Turns) {
    let request = read_http_request(&mut stream);
    if let Some(listing) = model_listing(&request) {
        let _ = stream.write_all(listing.as_bytes());
        return;
    }
    turns.arrived.fetch_add(1, Ordering::SeqCst);
    while turns.held.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(10));
    }
    let index = turns.served.fetch_add(1, Ordering::SeqCst) + 1;
    let id = format!("chatcmpl_{index}");
    let events = [
        json!({
            "id": id, "model": "test-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": format!("reply-{index}")}}]
        }),
        json!({
            "id": id,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        }),
    ];
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
        .collect();
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// Reads a request up to the end of its declared body. Reading to
/// end-of-stream would deadlock: the client keeps the connection open
/// waiting for the response it has not been sent yet.
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
