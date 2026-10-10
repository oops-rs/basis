//! What every A2A suite here opens a workspace with: a loopback agent that
//! serves its card, an offline runtime, and discovery pinned so the
//! developer's own configuration never reaches a test.

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
    A2aAgentConfig, A2aConfig, ContextConfig, MemoryConfig, Runtime, Workspace, WorkspaceBuilder,
    skills::SkillsConfig, store, templates::TemplatesConfig,
};
use mentra::{ModelSelector, tool::ToolAudience};
use serde_json::json;

/// A loopback agent that answers every request with its card.
pub struct CardServer {
    pub base: String,
    pub fetches: Arc<AtomicUsize>,
}

impl CardServer {
    pub fn start() -> Self {
        Self::serve(TcpListener::bind("127.0.0.1:0").expect("bind"))
    }

    fn serve(listener: TcpListener) -> Self {
        let base = format!("http://{}", listener.local_addr().expect("address"));
        let fetches = Arc::new(AtomicUsize::new(0));

        let counted = Arc::clone(&fetches);
        let endpoint = format!("{base}/rpc");
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
                let card = card(&endpoint, "Investigates questions about the codebase.");
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

    pub fn agent(&self, name: &str) -> A2aAgentConfig {
        A2aAgentConfig::new(name, &self.base)
    }

    pub fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

fn card(endpoint: &str, description: &str) -> String {
    json!({
        "name": "Nous",
        "description": description,
        "version": "1.0.0",
        "supportedInterfaces": [
            {"url": endpoint, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"},
        ],
        "capabilities": {},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [],
    })
    .to_string()
}

/// A port nothing listens on.
pub fn unreachable_agent(name: &str) -> A2aAgentConfig {
    A2aAgentConfig::new(name, "http://127.0.0.1:1")
}

pub fn offline_runtime() -> Arc<Runtime> {
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
pub fn no_a2a() -> A2aConfig {
    A2aConfig {
        global_dir: None,
        ..A2aConfig::default()
    }
}

pub fn pinned(workspace: &Path, runtime: Arc<Runtime>) -> WorkspaceBuilder {
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

pub fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

/// A workspace `a2a.json` naming each `(name, url)`.
pub fn a2a_file(agents: &[(&str, &str)]) -> String {
    let agents: serde_json::Map<String, serde_json::Value> = agents
        .iter()
        .map(|(name, url)| ((*name).to_string(), json!({"url": url})))
        .collect();
    json!({ "agents": agents }).to_string()
}

/// Every tool this directory's audience resolves, global or its own — where a
/// bridged agent used to appear, and where ADR-0029 says none does.
pub fn registered(runtime: &Runtime, root: &Path) -> Vec<String> {
    runtime
        .mentra_runtime()
        .tools_for_audience(Some(&ToolAudience::new(store::runtime_identifier(root))))
        .into_iter()
        .map(|descriptor| descriptor.provider.name)
        .collect()
}

/// A remote agent is a `spawn` target, never a tool of its own.
pub fn assert_no_agent_tools(runtime: &Runtime, root: &Path) {
    let names = registered(runtime, root);
    assert!(
        names.iter().all(|name| !name.starts_with("a2a__")),
        "a remote agent is not a tool: {names:?}"
    );
    assert!(names.iter().any(|name| name == "spawn"), "{names:?}");
}
