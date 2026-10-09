//! `.basis/a2a.json` and `WorkspaceBuilder::with_a2a`, end to end through a
//! workspace open: what is discovered, what is bridged onto the runtime and
//! for whom, what a run reports, and what goes when the workspace does.
//!
//! The agent on the other end is a loopback listener that serves an Agent
//! Card and nothing else — everything asserted here happens at the open or at
//! a mint, before any task is sent. `basis-a2a/tests/client.rs` drives a real
//! call against a live basis A2A server.

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

/// The tool names this directory's audience resolves — the honest observable
/// for an audience-scoped registration, which mentra's global readers do not
/// walk.
fn offered(runtime: &Runtime, root: &Path) -> Vec<String> {
    runtime
        .mentra_runtime()
        .tools_for_audience(Some(&ToolAudience::new(store::runtime_identifier(root))))
        .into_iter()
        .map(|descriptor| descriptor.provider.name)
        .filter(|name| name.starts_with("a2a__"))
        .collect()
}

#[tokio::test]
async fn a_supplied_agent_is_bridged_for_its_own_workspace_only() {
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
    assert_eq!(offered(&runtime, mine.path()), ["a2a__nous"]);
    assert!(
        offered(&runtime, theirs.path()).is_empty(),
        "another directory's model can neither list nor call this repository's agent"
    );
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
    // is the one that gets bridged.
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
    assert_eq!(offered(&runtime, dir.path()), ["a2a__nous"]);
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
    assert!(offered(&runtime, dir.path()).is_empty(), "nothing bridged");
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
async fn two_repositories_naming_one_agent_are_told_apart_and_released_on_drop() {
    let server = CardServer::start();
    let runtime = offline_runtime();
    let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());

    let first = pinned(one.path(), Arc::clone(&runtime))
        .with_a2a(no_a2a().with_supplied(vec![server.agent("nous").into()]))
        .open()
        .await
        .expect("opens");
    let second = pinned(two.path(), Arc::clone(&runtime))
        .with_a2a(no_a2a().with_supplied(vec![server.agent("nous").into()]))
        .open()
        .await
        .expect("opens");

    let suffixed = second.a2a_agents()[0].clone();
    assert_eq!(first.a2a_agents(), ["nous"]);
    assert!(suffixed.starts_with("nous-"), "{suffixed}");
    assert_eq!(offered(&runtime, one.path()), ["a2a__nous"]);
    assert_eq!(offered(&runtime, two.path()), [format!("a2a__{suffixed}")]);

    drop(first);
    assert!(
        offered(&runtime, one.path()).is_empty(),
        "a registry a host keeps for its whole process must not grow per open"
    );

    let third = pinned(one.path(), Arc::clone(&runtime))
        .with_a2a(no_a2a().with_supplied(vec![server.agent("nous").into()]))
        .open()
        .await
        .expect("opens");
    assert_eq!(
        third.a2a_agents(),
        ["nous"],
        "a released name is free again"
    );
}

#[tokio::test]
async fn a_same_directory_sibling_that_named_no_agent_is_not_offered_it() {
    // One directory is one audience, so mentra resolves the owner's bridged
    // agent for the sibling too. The tool carries the owner's credential;
    // the mint is what hides it (and `ForeignToolGuard` what refuses a call
    // the hide could not cover — `runtime::agents`' own tests).
    let server = CardServer::start();
    let runtime = offline_runtime();
    let dir = tempfile::tempdir().unwrap();

    let owner = pinned(dir.path(), Arc::clone(&runtime))
        .with_a2a(
            no_a2a().with_supplied(vec![server.agent("nous").with_bearer_token("owner").into()]),
        )
        .open()
        .await
        .expect("opens");
    let sibling = pinned(dir.path(), Arc::clone(&runtime))
        .open()
        .await
        .expect("the same directory opens again");

    let hidden = |workspace: &Workspace| {
        workspace
            .prepare("go")
            .expect("mints")
            .session()
            .config()
            .tool_profile
            .hidden_tools
            .contains("a2a__nous")
    };
    assert!(
        hidden(&sibling),
        "the open that named no agent is not offered it"
    );
    assert!(!hidden(&owner), "and the open that named it keeps it");
}

#[tokio::test]
async fn discovery_off_still_bridges_a_supplied_agent_and_reads_no_file() {
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
    assert!(
        workspace
            .mentra_runtime()
            .tools_for_audience(Some(&ToolAudience::new(store::runtime_identifier(
                dir.path()
            ))))
            .iter()
            .any(|descriptor| descriptor.provider.name == "a2a__from-host")
    );
}
