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

use basis::{
    CollectingSink, ContextConfig, RunOutcome, Runtime, RuntimeBuilder, Snapshot, Workspace,
    WorkspaceBuilder, hooks::HooksConfig, skills::SkillsConfig, store, templates::TemplatesConfig,
    tools::declared::ToolsConfig,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelSelector, agent::AgentConfig, runtime::SqliteRuntimeStore,
    test::MockRuntime,
};

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
/// history is ephemeral, so nothing here writes to the database under the
/// user's data directory — and the tests that are *about* persistence say
/// [`basis::RuntimeBuilder::with_store_dir`] afterwards, which is the last
/// word. The process knobs ride on the private runtime's recipe, where
/// ADR-0018 moved them; everything else is still the workspace's.
fn offline(workspace: &Path) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_runtime_builder(offline_runtime())
        .with_model(ModelSelector::Id("test-model".to_string()))
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

/// The process half of [`offline`], for the tests that re-say a runtime knob:
/// `with_runtime_builder` replaces the whole recipe, so a test that wants the
/// offline defaults plus one change starts from here.
fn offline_runtime() -> RuntimeBuilder {
    Runtime::builder()
        .with_base_url(CLOSED_PORT)
        .with_api_key("test-key")
        .with_ephemeral_history()
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

    use basis::RunSpec;

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
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
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
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
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
        .with_runtime_builder(offline_runtime().with_store_dir(elsewhere.path()))
        .open()
        .await
        .expect("opens");
    assert!(
        reopened.resume(&agent_id, "again").is_err(),
        "a different directory is a different history"
    );
}

/// `offline` resolves its model by explicit id, which mentra never asks a
/// listing for (`Runtime::resolve_model`) — so this workspace's context
/// window is unknown before *and* after a resume. What this pins is that
/// `resume`'s own reapplication of the resolved model — the fix for a
/// resumed agent otherwise losing a *known* window mentra does not persist —
/// does not corrupt the model a resumed conversation reports, in the one case
/// that exercises the same code path without a known window to lose.
#[tokio::test]
async fn resuming_on_the_same_model_reports_the_same_model_and_an_honest_unknown_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store = tempfile::tempdir().expect("tempdir");

    let opened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let prepared = opened.prepare("go").expect("mints");
    assert_eq!(
        prepared.context_window(),
        None,
        "an id-selected model was never listed, on any provider"
    );
    assert!(
        prepared.estimated_context_tokens() > 0,
        "the estimate still counts the system prompt AGENTS.md rendered, \
         even with an empty history"
    );
    let agent_id = prepared.agent_id().to_string();
    drop(prepared);
    drop(opened);

    let reopened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let resumed = reopened.resume(&agent_id, "again").expect("resumes");

    assert_eq!(resumed.context_window(), None);
    assert_eq!(
        resumed.context().model,
        "test-model",
        "reapplying the resolved model on resume must not rename it"
    );
}

/// A workspace that keeps its history nowhere is still a workspace.
///
/// The knob's floor. Swapping the backing store is exactly the kind of change
/// that looks fine until a turn is driven through it: minting persists an
/// agent, every round loads and saves it again, and resuming reads it back —
/// all through the store, none of it exercised by opening one.
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
            .execute(CollectingSink::default())
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

/// Ephemeral history is written nowhere — including wherever the same builder
/// had just been told to write it.
///
/// Both halves of the knob at once, and neither is provable without the other:
/// if the last word did not count the file would appear because a directory was
/// named, and if the store were not really in memory it would appear anyway.
#[tokio::test]
async fn an_ephemeral_workspace_leaves_the_directory_it_was_offered_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime_builder(
            offline_runtime()
                .with_store_dir(store_dir.path())
                .with_ephemeral_history(),
        )
        .open()
        .await
        .expect("opens");
    workspace.prepare("go").expect("mints");

    assert_eq!(
        std::fs::read_dir(store_dir.path())
            .expect("the directory the test made")
            .count(),
        0,
        "minting a run persists an agent, and this one persists it nowhere"
    );
    // Ordered after the directory check on purpose: listing opens the store it
    // is pointed at, so asking first would create the very file being denied.
    assert!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .is_empty(),
        "and there is nothing to list either"
    );
}

/// Nothing outlives the workspace: no resume by agent id, nothing to list.
///
/// What `with_ephemeral_history` promises about a later *process*, proved here
/// without starting one — a second `Workspace::open` gets a store of its own
/// exactly as a second process would. The second one keeps real history, which
/// is the sharpest form of the question: it has a database, it is pointed at
/// the same workspace, and the conversation is still not in it.
#[tokio::test]
async fn an_ephemeral_conversation_is_gone_once_its_workspace_is() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let opened = offline(dir.path()).open().await.expect("opens");
    let agent_id = opened.prepare("go").expect("mints").agent_id().to_string();
    drop(opened);

    let reopened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store_dir.path()))
        .open()
        .await
        .expect("opens");

    assert!(
        reopened.resume(&agent_id, "again").is_err(),
        "an ephemeral conversation cannot be resumed from anywhere else"
    );
    assert!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .is_empty(),
        "nor can it be found by looking"
    );
}

/// Every conversation a workspace mints is tagged with that workspace, which
/// is the whole of what makes listing possible.
///
/// The tag is mentra's runtime identifier and basis derives it from the
/// workspace path ([`store::runtime_identifier`]). Until `WorkspaceBuilder::open`
/// set one, everything basis persisted carried mentra's `"default"` while
/// `store::list_in` filtered on the workspace's — so listing had never returned
/// a conversation basis itself had written, and no test noticed because none of
/// them wrote one and then looked.
#[tokio::test]
async fn a_conversation_is_listed_for_the_workspace_that_minted_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store_dir.path()))
        .open()
        .await
        .expect("opens");
    let agent_id = workspace
        .prepare("go")
        .expect("mints")
        .agent_id()
        .to_string();

    let listed = store::list_in(store_dir.path(), dir.path()).expect("lists");

    assert_eq!(
        listed
            .iter()
            .map(|session| session.agent_id.as_str())
            .collect::<Vec<_>>(),
        vec![agent_id.as_str()],
        "a conversation this workspace minted must be one this workspace lists"
    );
}

#[tokio::test]
async fn one_workspace_does_not_list_anothers_conversations() {
    // The discriminating half: two workspaces sharing one store file, which is
    // the arrangement every basis on one machine is in by default.
    let mine = tempfile::tempdir().expect("tempdir");
    let theirs = tempfile::tempdir().expect("tempdir");
    write(&mine.path().join("AGENTS.md"), "house rules");
    write(&theirs.path().join("AGENTS.md"), "other rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(mine.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store_dir.path()))
        .open()
        .await
        .expect("opens");
    workspace.prepare("go").expect("mints");

    assert!(
        store::list_in(store_dir.path(), theirs.path())
            .expect("lists")
            .is_empty(),
        "offering a person another repository's conversations is worse than offering none"
    );
}

/// A conversation written before workspaces were tagged is still resumable, and
/// joins its workspace's list the first time it is used.
///
/// The back-compat question the tag raised, answered forward-only: nothing
/// migrates old rows, because nothing has to. mentra loads an agent by id alone
/// (`RuntimeStore::load_agent` is `WHERE id = ?1`), so the identifier never
/// gated resuming; and it re-derives the tag from the live runtime every time
/// it persists (`Agent::persisted_record`, and the upsert's
/// `runtime_identifier = excluded.runtime_identifier`), so using an old
/// conversation is what files it. Since listing never worked, no client has
/// ever seen these rows to miss them in the meantime.
#[tokio::test]
async fn a_conversation_tagged_before_workspaces_were_is_resumable_and_files_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    // What every basis before this fix wrote: mentra's own default tag.
    let agent_id = {
        let mock = MockRuntime::builder()
            .model("test-model", BuiltinProvider::OpenAI)
            .runtime_identifier("default")
            .with_store(SqliteRuntimeStore::new(
                store_dir.path().join(store_filename()),
            ))
            .text("from before")
            .build()
            .expect("the mock runtime builds");
        let mut session = mock
            .runtime()
            .create_session_with_config("old", mock.model(), AgentConfig::default())
            .expect("session");
        session
            .append_turn(vec![ContentBlock::text("hello")])
            .await
            .expect("a scripted turn completes");

        session.agent_id().to_string()
    };

    assert!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .is_empty(),
        "an untagged conversation is not claimed by a workspace it never recorded"
    );

    let endpoint = ScriptedEndpoint::start();
    let workspace = offline(dir.path())
        .with_runtime_builder(
            offline_runtime()
                .with_base_url(&endpoint.base_url)
                .with_store_dir(store_dir.path()),
        )
        .open()
        .await
        .expect("opens");
    let report = workspace
        .resume(&agent_id, "again")
        .expect("an old conversation is still resumable")
        .execute(CollectingSink::default())
        .await
        .expect("the resumed run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .into_iter()
            .map(|session| session.agent_id)
            .collect::<Vec<_>>(),
        vec![agent_id],
        "using an old conversation is what files it under its workspace"
    );
}

/// The filename basis puts inside a store directory.
///
/// Taken from mentra's default rather than spelled out, because basis chooses
/// mentra's own name (`store::store_in`) precisely so that pointing a workspace
/// at the default directory is a no-op — a literal here would be a second place
/// to keep that true.
fn store_filename() -> PathBuf {
    SqliteRuntimeStore::default()
        .path()
        .file_name()
        .expect("mentra's default store is a file")
        .into()
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
        .with_runtime_builder(offline_runtime().with_base_url(&endpoint.base_url))
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
        Self::start_with(sse_body)
    }

    /// The same endpoint answering each connection from a caller-chosen
    /// script, for the tests whose turns need a tool call in them.
    fn start_with(script: fn(usize) -> String) -> Self {
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
                thread::spawn(move || answer(stream, script(index)));
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
fn answer(mut stream: TcpStream, body: String) {
    read_http_request(&mut stream);

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
