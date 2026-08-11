//! A workspace opened once, minting runs cheaply — and concurrently.
//!
//! Two claims are checked here, and they are the two ADR-0010 made when it
//! asked for this split:
//!
//! 1. **Discovery happens at open.** A run minted afterwards carries what the
//!    workspace found, not what the filesystem says at mint time. The test for
//!    that deletes the context file between the two and expects the run to be
//!    unaffected — a per-run discovery would notice.
//! 2. **Runs minted from one workspace are independent and can be driven
//!    together.** The concurrency test drives two of them against a scripted
//!    endpoint on loopback and expects each to get its own answer.
//!
//! Loopback is not "the network": no packet leaves the machine, no name is
//! resolved, and the port is whichever one the OS hands out. The endpoint
//! speaks just enough of the OpenAI Responses wire format to complete a turn
//! with no tool calls in it.
//!
//! Every workspace here is opened against a closed port with an explicit model
//! id, so nothing is contacted until a turn is actually sent — which is itself
//! evidence that opening a workspace does not talk to the provider.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use lan_core::{
    CollectingSink, ContextConfig, RunOutcome, Snapshot, Workspace, WorkspaceBuilder,
    hooks::HooksConfig, skills::SkillsConfig, templates::TemplatesConfig,
};
use mentra::ModelSelector;

mod common;

/// A port nothing listens on. Reaching it would be a test failure rather than a
/// hang, but no code path here should try.
const CLOSED_PORT: &str = "http://127.0.0.1:1/v1";

/// A builder that looks nowhere except where the test put something, and that
/// contacts nothing while opening.
///
/// The credential is supplied rather than read from the environment, so the
/// suite behaves the same whether or not the person running it has a key
/// exported. An explicit model id short-circuits model resolution, which is the
/// only part of opening a workspace that would otherwise make a request. The
/// store is the test's own, so nothing here writes to the database under the
/// user's data directory (see [`common::scratch_store`]).
fn offline(workspace: &Path) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_base_url(CLOSED_PORT)
        .with_api_key("test-key")
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_store_dir(common::scratch_store())
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: PathBuf::from(".lan/skills"),
            global_dir: None,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".lan/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".lan/hooks.json"),
            global_dir: None,
        })
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

#[tokio::test]
async fn context_is_discovered_at_open_not_at_mint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let agents = dir.path().join("AGENTS.md");
    write(&agents, "house rules");

    let workspace = offline(dir.path()).open().await.expect("opens");

    // If minting re-discovered, this deletion would empty the run's context.
    std::fs::remove_file(&agents).expect("remove");
    let run = workspace.prepare("go").expect("mints");

    let documents = run.context().context.documents();
    assert_eq!(documents.len(), 1, "the run keeps what the open found");
    assert!(documents[0].content.contains("house rules"));
}

#[tokio::test]
async fn every_run_from_one_workspace_reports_the_same_resolution() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let workspace = offline(dir.path()).open().await.expect("opens");
    let first = workspace.prepare("one").expect("mints");
    let second = workspace.prepare("two").expect("mints");

    assert_eq!(first.context().model, second.context().model);
    assert_eq!(first.context().provider, second.context().provider);
    assert_eq!(first.context().workspace, second.context().workspace);
    assert_eq!(first.context().prompt, "one");
    assert_eq!(second.context().prompt, "two");
    assert_ne!(
        first.session_id(),
        second.session_id(),
        "two runs are two conversations"
    );
    assert_ne!(
        first.agent_id(),
        second.agent_id(),
        "and two persisted agents"
    );
}

#[tokio::test]
async fn a_spec_bounds_only_the_run_it_was_given_to() {
    use std::time::Duration;

    use lan_core::RunSpec;

    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let workspace = offline(dir.path()).open().await.expect("opens");
    let bounded = workspace
        .prepare(RunSpec::new("careful").with_deadline(Duration::from_secs(30)))
        .expect("mints");
    let unbounded = workspace.prepare("whatever it takes").expect("mints");

    assert_eq!(bounded.bounds().deadline, Some(Duration::from_secs(30)));
    assert_eq!(unbounded.bounds().deadline, None);
}

/// Conversations are persisted where the caller said, and nowhere else.
///
/// The discriminating half is the last one: without a store directory both
/// workspaces would fall back to the same machine-wide default and *every*
/// resume would succeed, so a test that only opened the store twice would pass
/// whether or not the knob did anything.
#[tokio::test]
async fn a_conversation_is_found_again_only_through_the_directory_it_was_written_to() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");

    let opened = offline(dir.path())
        .with_store_dir(store.path())
        .open()
        .await
        .expect("opens");
    let agent_id = opened.prepare("go").expect("mints").agent_id().to_string();
    drop(opened);

    assert!(
        std::fs::read_dir(store.path())
            .expect("the store directory was created")
            .next()
            .is_some(),
        "minting a run persists an agent, and it persists it where the caller said"
    );

    let reopened = offline(dir.path())
        .with_store_dir(store.path())
        .open()
        .await
        .expect("opens");
    assert_eq!(
        reopened
            .resume(&agent_id, "again")
            .expect("the conversation is in the store it was written to")
            .agent_id(),
        agent_id
    );

    let reopened = offline(dir.path())
        .with_store_dir(elsewhere.path())
        .open()
        .await
        .expect("opens");
    assert!(
        reopened.resume(&agent_id, "again").is_err(),
        "a different directory is a different history"
    );
}

#[tokio::test]
async fn a_workspace_fingerprints_itself_as_it_is_now() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let workspace = offline(dir.path()).open().await.expect("opens");
    let Snapshot::Known(before) = workspace.fingerprint() else {
        panic!("a workspace with a file in it fingerprints");
    };

    // ADR-0014 kept the fingerprint so a caller's loop can skip an unchanged
    // workspace. That only works if it reads the tree now rather than as it
    // was when the workspace was opened.
    write(&dir.path().join("new.txt"), "arrived later");
    let Snapshot::Known(after) = workspace.fingerprint() else {
        panic!("a workspace with two files in it fingerprints");
    };

    assert_ne!(before, after);
}

#[tokio::test]
async fn two_runs_from_one_workspace_are_driven_concurrently() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let endpoint = ScriptedEndpoint::start();
    let workspace = offline(dir.path())
        .with_base_url(&endpoint.base_url)
        .open()
        .await
        .expect("opens");

    let mut first = workspace.prepare("one").expect("mints");
    let mut second = workspace.prepare("two").expect("mints");

    let (left, right) = tokio::join!(
        first.execute(CollectingSink::default()),
        second.execute(CollectingSink::default()),
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

/// An OpenAI-compatible endpoint on loopback that completes any turn.
///
/// Every connection gets its own numbered answer, which is what lets a test
/// tell two concurrent runs apart. The listener is dropped when the endpoint
/// is, and the accept loop ends with it.
struct ScriptedEndpoint {
    base_url: String,
    served: Arc<AtomicUsize>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let served = Arc::new(AtomicUsize::new(0));

        let counted = Arc::clone(&served);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                // One thread per connection, so a second request that arrives
                // while the first is still being answered is not made to wait
                // — the point of the test is that both are in flight.
                let index = counted.fetch_add(1, Ordering::SeqCst) + 1;
                thread::spawn(move || answer(stream, index));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            served,
        }
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

/// Reads one request and writes one completed response.
fn answer(mut stream: TcpStream, index: usize) {
    read_http_request(&mut stream);

    let body = sse_body(index);
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The smallest stream that is a finished assistant turn: a response opens, one
/// message arrives whole, the response completes. No tool calls, so nothing
/// here depends on the runtime's policy or on an approver.
fn sse_body(index: usize) -> String {
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
fn read_http_request(stream: &mut TcpStream) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let Ok(read) = stream.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            return;
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
            return;
        }
    }
}
