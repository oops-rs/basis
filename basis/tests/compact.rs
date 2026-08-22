//! Compacting a conversation because someone asked, not because it grew.
//!
//! Auto-compaction is mentra's and fires on a threshold; this is the other
//! entry — a person deciding *now*, with an instruction about what to keep.
//! Both are the same summarizing pass, which is a model call, so everything
//! here runs against a loopback endpoint that answers one and records what it
//! was asked.
//!
//! The interesting claim is about the *stream*. mentra installs the tap that
//! carries an agent event onto a session's event stream only for the duration
//! of a turn (`Session::begin_turn`/`finish_turn`), so a compaction invoked
//! outside one reaches no subscriber at all. basis emits both events itself,
//! from what mentra returned — see `PreparedRun::compact`.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use basis::{
    CollectingSink, ContextConfig, Event, Runtime, Workspace, WorkspaceBuilder, hooks::HooksConfig,
    skills::SkillsConfig, store, templates::TemplatesConfig, tools::declared::ToolsConfig,
};
use mentra::{BuiltinProvider, ModelSelector};

#[tokio::test]
async fn compacting_a_conversation_reports_what_it_replaced_on_the_stream() {
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute(CollectingSink::default())
        .await
        .expect("the scripted turn runs");

    let mut sink = CollectingSink::default();
    let compacted = run
        .compact(Some("hold on to the migration plan"), &mut sink)
        .await
        .expect("the compacting pass runs")
        .expect("a conversation with a turn in it has something to compact");

    assert!(
        compacted.replaced_items > 0,
        "a pass that replaced nothing is not a pass: {compacted:?}"
    );
    assert_eq!(
        compacted.transcript_len,
        run.history().len(),
        "the reported length has to be the transcript the next turn will send"
    );

    // Both events, in mentra's own order — the same pair its in-turn mapping
    // produces from one `ContextCompacted`, so a client cannot tell an
    // on-demand pass from an automatic one.
    let agent_id = run.agent_id().to_string();
    let events = sink.into_events();
    assert!(
        matches!(&events[0], Event::CompactionStarted { agent_id: id } if *id == agent_id),
        "{events:?}"
    );
    match &events[1] {
        Event::CompactionCompleted {
            agent_id: id,
            replaced_items,
            transcript_len,
            ..
        } => {
            assert_eq!(*id, agent_id);
            assert_eq!(*replaced_items, compacted.replaced_items);
            assert_eq!(*transcript_len, compacted.transcript_len);
        }
        other => panic!("expected a completed compaction, got {other:?}"),
    }
    assert_eq!(events.len(), 2, "and nothing else: {events:?}");
}

#[tokio::test]
async fn the_instruction_is_added_to_what_the_summarizer_is_already_told() {
    // The knob's whole point, and the half a caller cannot see from the return
    // value: "keep the migration plan" has to reach the summarizing request
    // *alongside* mentra's standing continuity requirements rather than in
    // place of them.
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(endpoint.runtime(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute(CollectingSink::default())
        .await
        .expect("the scripted turn runs");

    run.compact(
        Some("hold on to the migration plan"),
        &mut CollectingSink::default(),
    )
    .await
    .expect("the compacting pass runs");

    let asked = endpoint.requests().join("\n");
    assert!(
        asked.contains("hold on to the migration plan"),
        "the caller's instruction never reached the summarizer: {asked}"
    );
    assert!(
        asked.contains("compaction"),
        "and it must arrive inside mentra's own compaction instructions: {asked}"
    );
}

#[tokio::test]
async fn a_conversation_with_nothing_to_compact_says_so_and_emits_nothing() {
    // The answer a caller gets either way. A `/compact` on a session that has
    // not spoken yet must not report a compaction that did not happen, and
    // must not leave a lone "compacting…" on a client's stream.
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(closed_port(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut sink = CollectingSink::default();
    let compacted = workspace
        .prepare("go")
        .expect("mints")
        .compact(None, &mut sink)
        .await
        .expect("an empty conversation is not an error");

    assert_eq!(compacted, None);
    assert!(
        sink.into_events().is_empty(),
        "nothing happened, so nothing is announced"
    );
}

#[tokio::test]
async fn renaming_a_session_is_what_a_later_listing_reports() {
    // mentra fixes a session's name at creation otherwise, so every ACP
    // session basis opened listed under the same placeholder.
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(closed_port(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    let agent_id = run.agent_id().to_string();
    run.set_name("the parser fix").expect("renames");

    let listed = store::list_in(store_dir.path(), dir.path()).expect("lists");
    let named = listed
        .iter()
        .find(|session| session.agent_id == agent_id)
        .expect("the conversation this workspace minted");

    assert_eq!(named.name, "the parser fix");
}

#[tokio::test]
async fn a_forgotten_conversation_is_neither_listed_nor_resumable() {
    // Both halves, because either alone would be a deletion that did not
    // delete: a row `list` still offers is one a person can pick, and a row
    // `resume` still opens is one that was never gone.
    let dir = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime(closed_port(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let kept = workspace
        .prepare("keep me")
        .expect("mints")
        .agent_id()
        .to_string();
    // Scoped: a live run holds the agent, and mentra's delete removes rows
    // rather than stopping anything in memory — a run still held would write
    // its row back on its next persist.
    let deleted = {
        let run = workspace.prepare("forget me").expect("mints");
        run.agent_id().to_string()
    };

    store::forget_in(store_dir.path(), &deleted).expect("deletes");

    assert_eq!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .into_iter()
            .map(|session| session.agent_id)
            .collect::<Vec<_>>(),
        vec![kept],
        "the one that was forgotten must be gone and the other must not"
    );
    assert!(
        workspace.resume(&deleted, "again").is_err(),
        "and there is nothing left to pick back up"
    );
}

#[tokio::test]
async fn forgetting_a_conversation_that_was_never_there_is_not_an_error() {
    // A caller deleting by an id it read from a list is racing anyone else
    // holding the same store, and "it is gone" is the outcome both wanted.
    let store_dir = tempfile::tempdir().expect("tempdir");

    store::forget_in(store_dir.path(), "agent-nobody-ever-minted")
        .expect("deleting nothing deletes nothing");
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A builder that discovers nothing it was not shown.
fn offline(workspace: &Path) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: PathBuf::from(".basis/skills"),
            global_dir: None,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
        })
}

/// A runtime whose history — and whose compaction snapshots — go where the
/// test put them, rather than under the developer's own data directory.
fn runtime_at(store_dir: &Path, base_url: &str) -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_provider(BuiltinProvider::OpenAI)
            .with_api_key("test-key")
            .with_base_url(base_url)
            .with_model(ModelSelector::Id("test-model".to_string()))
            .with_store_dir(store_dir)
            .build()
            .expect("the runtime builds without contacting anything"),
    )
}

/// For the tests that must not reach a provider at all.
fn closed_port(store_dir: &Path) -> Arc<Runtime> {
    runtime_at(store_dir, "http://127.0.0.1:1/v1")
}

/// The smallest endpoint that is a finished turn, with every request kept.
struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || answer(stream, &recorded));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            requests,
        }
    }

    fn runtime(&self, store_dir: &Path) -> Arc<Runtime> {
        runtime_at(store_dir, &self.base_url)
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn answer(mut stream: TcpStream, recorded: &Mutex<Vec<String>>) {
    let request = read_http_request(&mut stream);
    recorded
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(request);

    let body = sse_body();
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The smallest chat/completions stream that is a finished assistant turn.
///
/// A custom `base_url` speaks chat/completions, which is also why compaction
/// here takes mentra's *local* summarizing path: the wire declares
/// `supports_history_compaction: false`, so there is no remote `compact` call
/// to answer and the summary is asked for as an ordinary completion.
fn sse_body() -> String {
    [
        r#"{"id":"chatcmpl_1","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","content":"done"}}]}"#,
        r#"{"id":"chatcmpl_1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// Reads a request up to the end of its declared body. Reading to
/// end-of-stream would deadlock: the client is waiting for the response.
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
