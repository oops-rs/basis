//! `.basis/a2a.json` and `WorkspaceBuilder::with_a2a`, end to end through a
//! workspace open: what is discovered, what connects, what a run reports, and
//! — ADR-0029 — that none of it lands on the runtime's tool registry, so no
//! name is claimed or suffixed.
//!
//! The agent on the other end is a loopback listener that serves an Agent
//! Card and nothing else — everything asserted here happens at the open or at
//! a mint, before any task is sent. `basis-a2a/tests/client.rs` drives
//! `spawn "@agent …"` against a live basis A2A server, including what each
//! agent's model is told and which agents a sibling open can reach.

#![cfg(feature = "a2a")]

use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use basis::{
    A2aAgentConfig, A2aConfig, ContextConfig, Event, MemoryConfig, RunError, Runtime, Workspace,
    WorkspaceBuilder, skills::SkillsConfig, store, templates::TemplatesConfig,
};
use mentra::{ModelSelector, tool::ToolAudience};
use serde_json::json;

/// A loopback agent that answers every request with its card.
struct CardServer {
    base: String,
    fetches: Arc<AtomicUsize>,
}

impl CardServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("address"));
        let card = json!({
            "name": "Nous",
            "description": "Investigates questions about the codebase.",
            "version": "1.0.0",
            "supportedInterfaces": [
                {"url": format!("{base}/rpc"), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"},
            ],
            "capabilities": {},
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": [],
        })
        .to_string();
        let fetches = Arc::new(AtomicUsize::new(0));

        let counted = Arc::clone(&fetches);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                // One request per connection: read to the end of the head,
                // which is all a card fetch has.
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{card}",
                    card.len()
                );
            }
        });

        Self { base, fetches }
    }

    fn agent(&self, name: &str) -> A2aAgentConfig {
        A2aAgentConfig::new(name, &self.base)
    }
}

/// A port nothing listens on.
fn unreachable_agent(name: &str) -> A2aAgentConfig {
    A2aAgentConfig::new(name, "http://127.0.0.1:1")
}

fn offline_runtime() -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds without touching the network"),
    )
}

/// No global directory, so the developer's own `~/.config/basis/a2a.json`
/// can never reach this suite.
fn no_a2a() -> A2aConfig {
    A2aConfig {
        global_dir: None,
        ..A2aConfig::default()
    }
}

fn pinned(workspace: &Path, runtime: Arc<Runtime>) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_runtime(runtime)
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: None,
            shared_workspace_dir: false,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_memory(MemoryConfig::disabled())
        .with_a2a(no_a2a())
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

/// Every tool this directory's audience resolves, global or its own — where a
/// bridged agent used to appear, and where ADR-0029 says none does.
fn registered(runtime: &Runtime, root: &Path) -> Vec<String> {
    runtime
        .mentra_runtime()
        .tools_for_audience(Some(&ToolAudience::new(store::runtime_identifier(root))))
        .into_iter()
        .map(|descriptor| descriptor.provider.name)
        .collect()
}

/// A remote agent is a `spawn` target, never a tool of its own.
fn assert_no_agent_tools(runtime: &Runtime, root: &Path) {
    let names = registered(runtime, root);
    assert!(
        names.iter().all(|name| !name.starts_with("a2a__")),
        "a remote agent is not a tool: {names:?}"
    );
    assert!(names.iter().any(|name| name == "spawn"), "{names:?}");
}

#[tokio::test]
async fn a_supplied_agent_is_named_and_connected_without_registering_a_tool() {
    let server = CardServer::start();
    let runtime = offline_runtime();
    let (mine, theirs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());

    let workspace = pinned(mine.path(), Arc::clone(&runtime))
        .with_a2a(no_a2a().with_supplied(vec![server.agent("nous").into()]))
        .open()
        .await
        .expect("opens");
    let _other = pinned(theirs.path(), Arc::clone(&runtime))
        .open()
        .await
        .expect("opens");

    assert_eq!(workspace.a2a_agents(), ["nous"]);
    assert_no_agent_tools(&runtime, mine.path());
    assert_no_agent_tools(&runtime, theirs.path());
    assert_eq!(
        server.fetches.load(Ordering::SeqCst),
        1,
        "the card is fetched once, at open"
    );
}

#[tokio::test]
async fn the_workspace_file_is_discovered_and_named_in_the_run_header() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".basis/a2a.json"),
        &format!(r#"{{"agents":{{"nous":{{"url":"{}"}}}}}}"#, server.base),
    );

    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens");
    let header = workspace.prepare("go").expect("mints").header();

    let Event::RunStarted {
        a2a_files,
        a2a_agents,
        ..
    } = header
    else {
        panic!("the header is run_started");
    };
    assert_eq!(a2a_agents, ["nous"]);
    assert_eq!(a2a_files.len(), 1);
    assert!(a2a_files[0].path.ends_with(".basis/a2a.json"));
    assert_eq!(a2a_files[0].scope, "workspace");
}

#[tokio::test]
async fn a_supplied_agent_outranks_the_file_by_name() {
    let server = CardServer::start();
    let runtime = offline_runtime();
    let dir = tempfile::tempdir().unwrap();
    // The file's `nous` is unreachable; the host's is live. Whichever wins
    // is the one whose card is fetched.
    write(
        &dir.path().join(".basis/a2a.json"),
        r#"{"agents":{"nous":{"url":"http://127.0.0.1:1"}}}"#,
    );

    let workspace = pinned(dir.path(), Arc::clone(&runtime))
        .with_a2a(no_a2a().with_supplied(vec![server.agent("nous").into()]))
        .open()
        .await
        .expect("opens");

    assert_eq!(workspace.a2a_agents(), ["nous"], "one name is one agent");
    assert_eq!(
        server.fetches.load(Ordering::SeqCst),
        1,
        "the host's agent won, so its card is the one fetched"
    );
}

#[tokio::test]
async fn an_unreachable_agent_degrades_and_is_still_reported() {
    let runtime = offline_runtime();
    let dir = tempfile::tempdir().unwrap();

    let workspace = pinned(dir.path(), Arc::clone(&runtime))
        .with_a2a(no_a2a().with_supplied(vec![unreachable_agent("nous").into()]))
        .open()
        .await
        .expect("one unreachable agent must not sink the open");

    assert_eq!(
        workspace.a2a_agents(),
        ["nous"],
        "a configured agent is the workspace's whether or not it came up"
    );
    assert_no_agent_tools(&runtime, dir.path());
}

#[tokio::test]
async fn a_broken_file_refuses_the_open_naming_it_and_nothing_it_read() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".basis/a2a.json"),
        r#"{"agents":{"nous":{"url":"http://remote.example/?k=sk-live","headers":{"Authorization":"Bearer sk-live"}}}}"#,
    );

    let error = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect_err("a committed file cannot grant plaintext passage");

    let rendered = error.to_string();
    assert!(matches!(error, RunError::A2a(_)), "{rendered}");
    assert!(
        rendered.contains("a2a.json") && rendered.contains("nous"),
        "{rendered}"
    );
    assert!(!rendered.contains("sk-live"), "{rendered}");
}

#[tokio::test]
async fn two_workspaces_naming_one_agent_both_keep_the_name() {
    // Nothing is registered per agent, so there is no shared name to claim:
    // both repositories — and both opens of one repository — call it `nous`.
    let server = CardServer::start();
    let runtime = offline_runtime();
    let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());

    let open = |root: PathBuf| {
        let runtime = Arc::clone(&runtime);
        let agent = server.agent("nous");
        async move {
            pinned(&root, runtime)
                .with_a2a(no_a2a().with_supplied(vec![agent.into()]))
                .open()
                .await
                .expect("opens")
        }
    };
    let first = open(one.path().to_path_buf()).await;
    let second = open(two.path().to_path_buf()).await;
    let sibling = open(one.path().to_path_buf()).await;

    assert_eq!(first.a2a_agents(), ["nous"]);
    assert_eq!(second.a2a_agents(), ["nous"]);
    assert_eq!(sibling.a2a_agents(), ["nous"]);
}

#[tokio::test]
async fn discovery_off_still_connects_a_supplied_agent_and_reads_no_file() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    // Malformed, so reading it at all would refuse the open.
    write(&dir.path().join(".basis/a2a.json"), "{not json");

    // `without_discovery` refuses a shared runtime, so this one is private.
    let workspace = Workspace::builder(dir.path())
        .with_runtime_builder(
            Runtime::builder()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_api_key("test-key")
                .with_ephemeral_history(),
        )
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_a2a(A2aConfig::default().with_supplied(vec![server.agent("from-host").into()]))
        .without_discovery()
        .open()
        .await
        .expect("the malformed discovered file stays unread");

    assert_eq!(
        workspace.a2a_agents(),
        ["from-host"],
        "being handed an agent is not finding one"
    );
    assert_eq!(server.fetches.load(Ordering::SeqCst), 1, "and it connected");
}
