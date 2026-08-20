//! One runtime, many workspaces: the acceptance surface of ADR-0018.
//!
//! Three claims, each checked against a real (loopback-scripted) runtime:
//!
//! 1. **Sharing is real.** Two workspaces opened with one `Arc<Runtime>` run
//!    on the same mentra runtime, write one store file, and mint runs that can
//!    be driven concurrently.
//! 2. **Sharing does not leak.** A tool one workspace put on the shared
//!    registry — a `mcp__*` bridged one, or one its `.basis/tools.json`
//!    declared — never reaches another workspace's roster, asserted on the
//!    wire, in the `tools` array of the request the model actually receives.
//! 3. **The dispatcher's key holds.** The `working_directory` mentra hands a
//!    pre-hook is the agent's `base_dir` — the assumption basis's per-workspace
//!    hook routing dispatches on — and a workspace's own hooks therefore still
//!    guard its runs on a shared runtime.
//!
//! The endpoint is `tests/workspace.rs`'s, grown two abilities: it can script
//! a tool call, and it keeps every request body so a test can read the roster
//! the model was offered.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use basis::{
    CollectingSink, ContextConfig, RunOutcome, Runtime, Workspace, WorkspaceBuilder,
    hooks::HooksConfig, skills::SkillsConfig, store, templates::TemplatesConfig,
    tools::declared::ToolsConfig,
};
use mentra::ModelSelector;
use serde_json::json;

/// A workspace builder that looks nowhere except where the test put something.
/// `tests/workspace.rs` explains the choices; here the runtime always arrives
/// shared, which is the point of the file.
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

fn shared_runtime(endpoint: &ScriptedEndpoint) -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_base_url(&endpoint.base_url)
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("a shared runtime builds without touching the network"),
    )
}

fn workspace_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules").expect("write");
    dir
}

#[tokio::test]
async fn two_workspaces_minted_from_one_runtime_share_it() {
    let endpoint = ScriptedEndpoint::start(Vec::new());
    let store_dir = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(
        Runtime::builder()
            .with_base_url(&endpoint.base_url)
            .with_api_key("test-key")
            .with_store_dir(store_dir.path())
            .build()
            .expect("builds"),
    );

    let (dir_a, dir_b) = (workspace_dir(), workspace_dir());
    let a = pinned(dir_a.path(), Arc::clone(&runtime))
        .open()
        .await
        .expect("opens");
    let b = pinned(dir_b.path(), Arc::clone(&runtime))
        .open()
        .await
        .expect("opens");

    // The identity check: one substrate, not two that happen to agree.
    assert!(
        std::ptr::eq(a.mentra_runtime(), b.mentra_runtime()),
        "both workspaces must run on the very same mentra runtime"
    );

    // One store handle: every conversation from every workspace lands in one
    // file, where N private runtimes would have opened N.
    let mut run_a = a.prepare("one").expect("mints");
    let mut run_b = b.prepare("two").expect("mints");
    let (left, right) = tokio::join!(
        run_a.execute(CollectingSink::default()),
        run_b.execute(CollectingSink::default()),
    );
    assert!(matches!(left.expect("completes").outcome, RunOutcome::Ok));
    assert!(matches!(right.expect("completes").outcome, RunOutcome::Ok));

    let stored: Vec<String> = std::fs::read_dir(store_dir.path())
        .expect("store dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        stored,
        vec!["runtime.sqlite".to_string()],
        "one runtime, one store file, both workspaces inside it"
    );
}

/// Gated on the mentra per-session persist identifier (see `Runtime::mint`):
/// today a shared runtime tags every row `"basis:runtime"`, so per-workspace
/// listing cannot distinguish them. Unignore in the commit that adopts the
/// upstream override.
#[tokio::test]
#[ignore = "requires mentra's per-session persist identifier; see Runtime::mint"]
async fn a_shared_runtimes_conversations_list_under_their_own_workspaces() {
    let endpoint = ScriptedEndpoint::start(Vec::new());
    let store_dir = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(
        Runtime::builder()
            .with_base_url(&endpoint.base_url)
            .with_api_key("test-key")
            .with_store_dir(store_dir.path())
            .build()
            .expect("builds"),
    );

    let (dir_a, dir_b) = (workspace_dir(), workspace_dir());
    let a = pinned(dir_a.path(), Arc::clone(&runtime))
        .open()
        .await
        .expect("opens");
    let b = pinned(dir_b.path(), Arc::clone(&runtime))
        .open()
        .await
        .expect("opens");

    let mut run_a = a.prepare("one").expect("mints");
    let agent_a = run_a.agent_id().to_string();
    run_a
        .execute(CollectingSink::default())
        .await
        .expect("completes");
    b.prepare("two")
        .expect("mints")
        .execute(CollectingSink::default())
        .await
        .expect("completes");

    let listed: Vec<String> = store::list_in(store_dir.path(), dir_a.path())
        .expect("lists")
        .into_iter()
        .map(|session| session.agent_id)
        .collect();
    assert_eq!(
        listed,
        vec![agent_a],
        "workspace A lists its own conversation and not its sibling's"
    );
}

#[cfg(feature = "mcp")]
mod roster {
    use super::*;

    use mentra::tool::{RuntimeToolDescriptor, ToolExecutor, ToolResult};

    /// Stands in for a bridged tool another workspace's MCP server left on the
    /// shared registry — same namespaced name, none of the process baggage.
    struct ForeignBridged;

    impl mentra::tool::ToolDefinition for ForeignBridged {
        fn descriptor(&self) -> RuntimeToolDescriptor {
            RuntimeToolDescriptor::builder("mcp__foreign__peek")
                .description("a sibling workspace's bridged tool")
                .input_schema(json!({"type": "object"}))
                .build()
        }
    }

    #[async_trait::async_trait]
    impl ToolExecutor for ForeignBridged {
        async fn execute(
            &self,
            _ctx: mentra::tool::ParallelToolContext,
            _input: serde_json::Value,
        ) -> ToolResult {
            Ok("peeked".to_string())
        }
    }

    #[tokio::test]
    async fn a_foreign_mcp_tool_never_reaches_this_workspaces_roster() {
        let endpoint = ScriptedEndpoint::start(Vec::new());
        let runtime = shared_runtime(&endpoint);
        // What a sibling workspace's open would have done: bridge its server's
        // tools onto the runtime's single registry.
        runtime.mentra_runtime().register_tool(ForeignBridged);

        let dir = workspace_dir();
        let workspace = pinned(dir.path(), runtime).open().await.expect("opens");
        let report = workspace
            .prepare("go")
            .expect("mints")
            .execute(CollectingSink::default())
            .await
            .expect("completes");
        assert!(matches!(report.outcome, RunOutcome::Ok));

        // The wire is the honest observable: the `tools` array in the request
        // is the roster the model was actually offered.
        let requests = endpoint.requests();
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).expect("a body"))
                .expect("a JSON request");
        let offered: Vec<&str> = body["tools"]
            .as_array()
            .expect("a tools array")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();

        assert!(
            offered.contains(&"spawn"),
            "the roster parsed: basis's own tool must be in it: {offered:?}"
        );
        assert!(
            !offered.contains(&"mcp__foreign__peek"),
            "a tool this workspace never configured must not be offered to its model: {offered:?}"
        );
    }
}

/// The same claim for ADR-0012's other subprocess binding — and it needs its own
/// module rather than a case in [`roster`], because declared tools are core
/// rather than part of the `mcp` feature, and so is the claim.
mod declared_roster {
    use super::*;

    /// A workspace whose manifest declares one tool, and the tool's name.
    fn declaring(dir: &Path) -> &'static str {
        std::fs::create_dir_all(dir.join(".basis")).expect("create .basis");
        std::fs::write(
            dir.join(".basis/tools.json"),
            r#"{"schema": 1, "tools": {"jenkins_job": {
                "description": "trigger a job",
                "input_schema": {"type": "object", "properties": {}},
                "command": ["./ci/jenkins"]
            }}}"#,
        )
        .expect("write manifest");

        "jenkins_job"
    }

    /// The tool names in the nth request's `tools` array — the roster the model
    /// was actually offered, which is the only honest observable.
    fn roster(endpoint: &ScriptedEndpoint, index: usize) -> Vec<String> {
        let requests = endpoint.requests();
        let body: serde_json::Value = serde_json::from_str(
            requests[index]
                .split("\r\n\r\n")
                .nth(1)
                .expect("a request body"),
        )
        .expect("a JSON request");

        body["tools"]
            .as_array()
            .expect("a tools array")
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    }

    #[tokio::test]
    async fn a_declared_tool_is_offered_to_its_own_workspace_and_to_no_other() {
        // A declared tool carries no `mcp__` prefix, so nothing about its *name*
        // keeps it out of a sibling's roster on a shared registry — only the
        // claim basis holds on it does. Both halves are asserted, because a rule
        // that hid the tool from everybody would pass the second one.
        let endpoint = ScriptedEndpoint::start(Vec::new());
        let runtime = shared_runtime(&endpoint);

        let declaring_dir = workspace_dir();
        let declared = declaring(declaring_dir.path());
        let owner = pinned(declaring_dir.path(), Arc::clone(&runtime))
            .open()
            .await
            .expect("opens");
        assert_eq!(owner.declared_tools(), [declared]);

        let bystander_dir = workspace_dir();
        let bystander = pinned(bystander_dir.path(), runtime)
            .open()
            .await
            .expect("opens");

        for workspace in [&owner, &bystander] {
            let report = workspace
                .prepare("go")
                .expect("mints")
                .execute(CollectingSink::default())
                .await
                .expect("completes");
            assert!(matches!(report.outcome, RunOutcome::Ok));
        }

        let (owners, bystanders) = (roster(&endpoint, 0), roster(&endpoint, 1));

        assert!(
            owners.iter().any(|tool| tool == "spawn"),
            "the roster parsed: basis's own tool must be in it: {owners:?}"
        );
        assert!(
            owners.iter().any(|tool| tool == declared),
            "the workspace that declared it must be offered it: {owners:?}"
        );
        assert!(
            !bystanders.iter().any(|tool| tool == declared),
            "a program another repository declared must not be offered here: {bystanders:?}"
        );
    }
}

mod dispatch_key {
    use super::*;

    use mentra::{
        ContentBlock,
        error::RuntimeError,
        runtime::{HookDecision, PreExecutionContext, PreExecutionHook},
        test::{MockRuntime, MockToolCall},
    };

    struct Recording(Arc<Mutex<Vec<PathBuf>>>);

    #[async_trait::async_trait]
    impl PreExecutionHook for Recording {
        async fn pre_tool_execution(
            &self,
            context: &PreExecutionContext,
        ) -> Result<HookDecision, RuntimeError> {
            self.0
                .lock()
                .expect("recorder")
                .push(context.working_directory.clone());
            Ok(HookDecision::Allow)
        }
    }

    /// Pins the assumption basis's hook dispatcher is built on: what mentra
    /// hands a pre-hook as `working_directory` is the agent's `base_dir` —
    /// the same path basis keys its workspace registry with. If this ever moves
    /// upstream, dispatch would miss and workspace hooks would silently stop
    /// running; this test is the tripwire.
    #[tokio::test]
    async fn the_working_directory_a_hook_sees_is_the_agents_base_dir() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let workspace = tempfile::tempdir().expect("tempdir");

        let mock = MockRuntime::builder()
            .model("test-model", "openai")
            .with_pre_hook(Recording(Arc::clone(&seen)))
            .tool_calls(vec![MockToolCall::new(
                "files",
                json!({"operations": [{"op": "list", "path": "."}]}),
            )])
            .text("done")
            .build()
            .expect("the mock runtime builds");
        let mut session = mock
            .runtime()
            .create_session_with_config(
                "test",
                mock.model(),
                mentra::agent::AgentConfig {
                    workspace: mentra::agent::WorkspaceConfig {
                        base_dir: workspace.path().to_path_buf(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .expect("session");
        session
            .append_turn(vec![ContentBlock::text("go")])
            .await
            .expect("a scripted turn completes");

        assert_eq!(
            seen.lock().expect("recorder").as_slice(),
            &[workspace.path().to_path_buf()],
            "dispatching on working_directory only works if it is base_dir"
        );
    }
}

/// A workspace's own hooks still guard its runs when the runtime is shared —
/// the registration moved onto the dispatcher, the effect did not — and a
/// sibling workspace without hooks is untouched by them.
#[cfg(unix)]
#[tokio::test]
async fn a_workspaces_hooks_guard_its_runs_on_a_shared_runtime() {
    use std::os::unix::fs::PermissionsExt;

    // Connections alternate per run: a tool call, then the wrap-up text.
    let endpoint = ScriptedEndpoint::start(vec![
        Reply::files_create("made.txt"),
        Reply::Text,
        Reply::files_create("made.txt"),
        Reply::Text,
    ]);
    let runtime = shared_runtime(&endpoint);

    let guarded = workspace_dir();
    let script = guarded.path().join("deny.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho '{\"decision\":\"deny\",\"reason\":\"workspace guard\"}'\n",
    )
    .expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::create_dir_all(guarded.path().join(".basis")).expect("dir");
    std::fs::write(
        guarded.path().join(".basis/hooks.json"),
        format!(
            r#"{{"schema": 1, "hooks": [{{"name": "guard", "command": ["{}"]}}]}}"#,
            script.display()
        ),
    )
    .expect("hooks file");
    let free = workspace_dir();

    let first = pinned(guarded.path(), Arc::clone(&runtime))
        .open()
        .await
        .expect("opens");
    let second = pinned(free.path(), runtime).open().await.expect("opens");

    first
        .prepare("write a file")
        .expect("mints")
        .execute(CollectingSink::default())
        .await
        .expect("the guarded run completes — a denial is an answer, not an error");
    assert!(
        !guarded.path().join("made.txt").exists(),
        "the guarded workspace's hook must stop the write"
    );

    second
        .prepare("write a file")
        .expect("mints")
        .execute(CollectingSink::default())
        .await
        .expect("the free run completes");
    assert!(
        second.path().join("made.txt").exists(),
        "a sibling with no hooks must be untouched by the guarded one's"
    );
}

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

/// What one connection answers with.
#[derive(Clone)]
enum Reply {
    /// A finished assistant message, numbered by connection.
    Text,
    /// A single tool call; the next connection is expected to wrap up.
    ToolCall { name: String, arguments: String },
}

impl Reply {
    fn files_create(path: &str) -> Self {
        Self::ToolCall {
            name: "files".to_string(),
            arguments: json!({"operations": [{"op": "create", "path": path, "content": "hi"}]})
                .to_string(),
        }
    }
}

/// An OpenAI-compatible endpoint on loopback that follows a per-connection
/// script (falling back to a numbered text reply) and keeps every request it
/// was sent. `tests/workspace.rs` explains why loopback is not "the network".
struct ScriptedEndpoint {
    base_url: String,
    #[cfg_attr(
        not(feature = "mcp"),
        allow(
            dead_code,
            reason = "read back only by the roster test, which is mcp-gated"
        )
    )]
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            let mut index = 0_usize;
            while let Ok((stream, _)) = listener.accept() {
                index += 1;
                let reply = script.get(index - 1).cloned().unwrap_or(Reply::Text);
                let recorded = Arc::clone(&recorded);
                // One thread per connection, so concurrent runs are answered
                // concurrently rather than queued.
                thread::spawn(move || answer(stream, index, &reply, &recorded));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            requests,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

fn answer(mut stream: TcpStream, index: usize, reply: &Reply, recorded: &Mutex<Vec<String>>) {
    let request = read_http_request(&mut stream);
    recorded.lock().expect("requests").push(request);

    let body = sse_body(index, reply);
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The smallest stream that is a finished turn of the requested shape.
fn sse_body(index: usize, reply: &Reply) -> String {
    let mut events = vec![json!({
        "type": "response.created",
        "response": {"id": format!("resp_{index}"), "model": "test-model", "status": "in_progress"}
    })];

    match reply {
        Reply::Text => {
            events.push(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "content": []}
            }));
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message", "content": [{"type": "output_text", "text": format!("reply-{index}")}]}
            }));
        }
        Reply::ToolCall { name, arguments } => {
            events.push(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "id": format!("fc_{index}"),
                         "call_id": format!("call_{index}"), "name": name, "arguments": ""}
            }));
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": format!("call_{index}"),
                         "name": name, "arguments": arguments}
            }));
        }
    }

    events.push(json!({
        "type": "response.completed",
        "response": {"id": format!("resp_{index}"), "model": "test-model", "status": "completed"}
    }));

    events
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

/// A host that builds software needs more than two minutes.
///
/// The default suits the commands a harness usually runs and does not suit
/// `docker compose build`. Past the limit the process is killed mid-stream and
/// the caller gets truncated output with no error in it, which reads as a
/// silent failure rather than a stopped one.
#[test]
fn a_host_can_ask_for_more_command_patience() {
    let runtime = Runtime::builder()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
        .with_command_timeout(std::time::Duration::from_secs(600))
        .build();
    assert!(
        runtime.is_ok(),
        "a longer timeout is not a reason to fail: {:?}",
        runtime.err()
    );
}

#[test]
fn asking_for_nothing_keeps_the_default() {
    let runtime = Runtime::builder()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
        .build();
    assert!(runtime.is_ok(), "{:?}", runtime.err());
}
