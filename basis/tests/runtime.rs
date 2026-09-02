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
//! 3. **A workspace's hooks are its own.** Each open registers its folded
//!    chain live, for its own tool audience, so a workspace's hooks guard its
//!    runs on a shared runtime, a sibling is untouched by them, and a second
//!    open of one root joins the first rather than replacing it.
//!
//! The endpoint is `tests/workspace.rs`'s, grown two abilities: it can script
//! a tool call, and it keeps every request body so a test can read the roster
//! the model was offered.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use basis::{
    AllowAll, CollectingSink, ContextConfig, HookSpec, MemoryConfig, RunOutcome, Runtime,
    Workspace, WorkspaceBuilder, hooks::HooksConfig, skills::SkillsConfig, store,
    templates::TemplatesConfig, tools::declared::ToolsConfig,
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
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        // A malformed file in the developer's own ~/.config/basis/memory must
        // never be able to fail this suite (G1); this test is not about
        // memory at all.
        .with_memory(MemoryConfig::disabled())
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

    // One store handle: every conversation from every workspace lands under
    // one root, where N private runtimes would have opened N.
    let mut run_a = a.prepare("one").expect("mints");
    let mut run_b = b.prepare("two").expect("mints");
    let (left, right) = tokio::join!(
        run_a.execute_with_approver(CollectingSink::default(), AllowAll),
        run_b.execute_with_approver(CollectingSink::default(), AllowAll),
    );
    assert!(matches!(left.expect("completes").outcome, RunOutcome::Ok));
    assert!(matches!(right.expect("completes").outcome, RunOutcome::Ok));

    let stored = std::fs::read_dir(store_dir.path().join("agents"))
        .expect("one runtime lays its agents under the one store root it was pointed at")
        .count();
    assert_eq!(
        stored, 2,
        "one runtime, one store root, both workspaces' conversations inside it"
    );
}

/// Each session's rows are tagged with its own workspace (`Runtime::mint`
/// passes the per-session identifier), so one store file serving two
/// workspaces lists each one's conversations apart.
#[tokio::test]
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
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("completes");
    b.prepare("two")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
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

#[tokio::test]
async fn a_conversation_cannot_be_resumed_under_a_workspace_it_did_not_run_in() {
    // The pairing a shared runtime makes possible and an agent id cannot
    // refuse: a host that picks the workspace from a client's `cwd` and the
    // conversation from an id it was handed can bring the two together wrongly
    // — ACP's `session/load` is exactly that shape. A resume states this
    // workspace's policy and tool audience onto whatever it picks up, so under
    // the wrong one a conversation would run with another repository's `.git`
    // carve-out and shell posture while its agent stayed based in its own
    // directory, which mentra's file tools always allow writes under.
    let endpoint = ScriptedEndpoint::start(Vec::new());
    let runtime = shared_runtime(&endpoint);
    let (dir_a, dir_b) = (workspace_dir(), workspace_dir());
    let a = pinned(dir_a.path(), Arc::clone(&runtime))
        .open()
        .await
        .expect("opens");
    let b = pinned(dir_b.path(), runtime).open().await.expect("opens");

    let run = a.prepare("go").expect("mints");
    let agent_id = run.agent_id().to_string();
    // The live run holds the agent's lease; a resume is what a later attach
    // does, and here it needs the first to have let go.
    drop(run);

    let refused = b
        .resume(&agent_id, "again")
        .expect_err("a sibling workspace must not adopt this conversation");
    assert!(
        matches!(refused, basis::RunError::WorkspaceMismatch { .. }),
        "the refusal has to be the typed one a host can react to: {refused}"
    );
    let message = refused.to_string();
    assert!(
        message.contains(&dir_a.path().to_string_lossy().to_string())
            && message.contains(&dir_b.path().to_string_lossy().to_string()),
        "and it has to name both directories: {message}"
    );

    assert_eq!(
        a.resume(&agent_id, "again")
            .expect("its own workspace still resumes it")
            .agent_id(),
        agent_id,
        "the check must refuse the mismatch and nothing else"
    );
}

#[cfg(feature = "mcp")]
mod roster {
    use super::*;

    use mentra::tool::{RuntimeToolDescriptor, ToolAudience, ToolExecutor, ToolResult};

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
        // tools onto the runtime's single registry, for that workspace's own
        // audience. Global registration would be a different claim — a runtime
        // tool the host meant every workspace to have — and is deliberately
        // still visible to everyone.
        let _foreign = runtime
            .mentra_runtime()
            .try_register_tool_for_audience(
                ToolAudience::new(basis::store::runtime_identifier(std::path::Path::new(
                    "/repo/sibling",
                ))),
                ForeignBridged,
            )
            .expect("the sibling's audience is free");

        let dir = workspace_dir();
        let workspace = pinned(dir.path(), runtime).open().await.expect("opens");
        let report = workspace
            .prepare("go")
            .expect("mints")
            .execute_with_approver(CollectingSink::default(), AllowAll)
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
            .filter_map(|tool| tool["function"]["name"].as_str())
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

    /// A host tool registered under an `mcp__`-shaped name.
    struct HostShapedLikeBridged;

    impl mentra::tool::ToolDefinition for HostShapedLikeBridged {
        fn descriptor(&self) -> RuntimeToolDescriptor {
            RuntimeToolDescriptor::builder("mcp__internal__admin")
                .description("the host's own tool, under a bridged name")
                .input_schema(json!({"type": "object"}))
                .build()
        }
    }

    #[async_trait::async_trait]
    impl ToolExecutor for HostShapedLikeBridged {
        async fn execute(
            &self,
            _ctx: mentra::tool::ParallelToolContext,
            _input: serde_json::Value,
        ) -> ToolResult {
            Ok("administered".to_string())
        }
    }

    #[tokio::test]
    async fn a_global_mcp_shaped_host_tool_is_not_in_the_default_roster() {
        // Globals are visible to every audience on purpose — that is what
        // `RuntimeBuilder::with_tool` means. An `mcp__server__tool` name is the
        // one shape where that rule reads wrong: it says a server this
        // workspace never configured, and no `.mcp.json` here ever named
        // `internal`. Asserted against the *default* roster, because an exact
        // roster that names the tool by hand is a host saying otherwise.
        let endpoint = ScriptedEndpoint::start(Vec::new());
        let runtime = Arc::new(
            Runtime::builder()
                .with_base_url(&endpoint.base_url)
                .with_api_key("test-key")
                .with_ephemeral_history()
                .with_tool(HostShapedLikeBridged)
                .build()
                .expect("a shared runtime builds without touching the network"),
        );

        let dir = workspace_dir();
        let workspace = pinned(dir.path(), runtime).open().await.expect("opens");
        let report = workspace
            .prepare("go")
            .expect("mints")
            .execute_with_approver(CollectingSink::default(), AllowAll)
            .await
            .expect("completes");
        assert!(matches!(report.outcome, RunOutcome::Ok));

        let requests = endpoint.requests();
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).expect("a body"))
                .expect("a JSON request");
        let offered: Vec<&str> = body["tools"]
            .as_array()
            .expect("a tools array")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();

        assert!(
            offered.contains(&"spawn"),
            "the roster parsed: basis's own tool must be in it: {offered:?}"
        );
        assert!(
            !offered.contains(&"mcp__internal__admin"),
            "a host global shaped like a bridged tool must not ride into a \
             workspace's default roster: {offered:?}"
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

        // `chat/completions` nests the name under `function`, unlike the
        // Responses wire, which puts it flat on the tool.
        body["tools"]
            .as_array()
            .expect("a tools array")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
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
                .execute_with_approver(CollectingSink::default(), AllowAll)
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

    #[tokio::test]
    async fn a_resumed_run_is_still_offered_its_own_workspaces_declared_tool() {
        // A workspace's own tools are registered for its tool audience, and
        // mentra deliberately keeps an audience out of the persisted agent — so
        // a resume that failed to restate it would hand the model a roster
        // missing exactly the tools the repository declared, with nothing
        // anywhere failing to say so.
        let endpoint = ScriptedEndpoint::start(Vec::new());
        let runtime = shared_runtime(&endpoint);
        let dir = workspace_dir();
        let declared = declaring(dir.path());

        let workspace = pinned(dir.path(), runtime).open().await.expect("opens");
        let mut first = workspace.prepare("go").expect("mints");
        let agent_id = first.agent_id().to_string();
        first
            .execute_with_approver(CollectingSink::default(), AllowAll)
            .await
            .expect("completes");
        // The live run holds the agent's lease; a resume is what a later
        // process does, and here it needs the first run to have let go.
        drop(first);

        workspace
            .resume(&agent_id, "again")
            .expect("resumes")
            .execute_with_approver(CollectingSink::default(), AllowAll)
            .await
            .expect("completes");

        let resumed = roster(&endpoint, 1);
        assert!(
            resumed.iter().any(|tool| tool == declared),
            "the resumed conversation must keep the tools its workspace declared: {resumed:?}"
        );
    }
}

/// A workspace's own hooks still guard its runs when the runtime is shared —
/// the registration is a live, audience-scoped one now, the effect is unchanged
/// — and a sibling workspace without hooks is untouched by them.
#[cfg(unix)]
#[tokio::test]
async fn a_workspaces_hooks_guard_its_runs_on_a_shared_runtime() {
    use std::os::unix::fs::PermissionsExt;

    // Connections alternate per run: a tool call, then the wrap-up text.
    let endpoint = ScriptedEndpoint::start(vec![
        Reply::write_file("made.txt"),
        Reply::Text,
        Reply::write_file("made.txt"),
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
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the guarded run completes — a denial is an answer, not an error");
    assert!(
        !guarded.path().join("made.txt").exists(),
        "the guarded workspace's hook must stop the write"
    );

    second
        .prepare("write a file")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the free run completes");
    assert!(
        second.path().join("made.txt").exists(),
        "a sibling with no hooks must be untouched by the guarded one's"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_second_open_of_one_root_cannot_weaken_the_first_ones_hooks() {
    // Two live opens of one repository share a tool audience, so both their
    // chains are consulted for either one's runs. That is the shape that
    // matters: a later open with a permissive guard joins the first rather
    // than replacing it, and the first refusal still wins — the property the
    // old same-root registry enforced by refusing the second open outright.
    // When the first goes, its registration goes with it, and what is left is
    // the second's own answer.
    use std::os::unix::fs::PermissionsExt;

    let endpoint = ScriptedEndpoint::start(vec![
        Reply::write_file("same-root.txt"),
        Reply::Text,
        Reply::write_file("same-root.txt"),
        Reply::Text,
    ]);
    let runtime = shared_runtime(&endpoint);
    let root = workspace_dir();

    let script = |name: &str, body: &str| {
        let path = root.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        HookSpec::new(name, vec![path.to_string_lossy().into_owned()])
    };
    let deny = script(
        "deny-same-root.sh",
        r#"echo '{"decision":"deny","reason":"first guard stays"}'"#,
    );
    let allow = script("allow-same-root.sh", r#"echo '{"decision":"allow"}'"#);
    let config = |supplied| HooksConfig {
        workspace_file: PathBuf::from(".basis/hooks.json"),
        global_dir: None,
        supplied,
    };

    let first = pinned(root.path(), Arc::clone(&runtime))
        .with_hooks(config(vec![deny]))
        .open()
        .await
        .expect("the first guard registers");
    let permissive = pinned(root.path(), runtime)
        .with_hooks(config(vec![allow]))
        .open()
        .await
        .expect("a second open of one root is not a conflict");

    permissive
        .prepare("write a file")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("a hook denial is an answer, not a run failure");
    assert!(
        !root.path().join("same-root.txt").exists(),
        "the second open must not be able to lift the first open's refusal"
    );

    drop(first);

    permissive
        .prepare("write a file")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("completes");
    assert!(
        root.path().join("same-root.txt").exists(),
        "and the refusal leaves with the open that declared it"
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
    #[cfg(unix)]
    ToolCall { name: String, arguments: String },
}

impl Reply {
    /// The write a basis runtime's roster actually offers: `write`, not the
    /// batched `files` this used to script. A runtime built through
    /// [`basis::RuntimeBuilder`] registers mentra's split file tools by
    /// default, so a scripted `files` call would name a tool that is not
    /// there and the run would prove nothing about hooks.
    #[cfg(unix)]
    fn write_file(path: &str) -> Self {
        Self::ToolCall {
            name: "write".to_string(),
            arguments: json!({"path": path, "content": "hi"}).to_string(),
        }
    }
}

/// An OpenAI-compatible endpoint on loopback speaking `chat/completions` — the
/// wire a base URL gets — that follows a per-connection script (falling back to
/// a numbered text reply) and keeps every request it was sent.
/// `tests/workspace.rs` explains why loopback is not "the network".
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
        let script = Arc::new(script);
        let turns = Arc::new(AtomicUsize::new(0));
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let script = Arc::clone(&script);
                let turns = Arc::clone(&turns);
                let recorded = Arc::clone(&recorded);
                // One thread per connection, so concurrent runs are answered
                // concurrently rather than queued.
                thread::spawn(move || answer(stream, &script, &turns, &recorded));
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
    script: &[Reply],
    turns: &AtomicUsize,
    recorded: &Mutex<Vec<String>>,
) {
    let request = read_http_request(&mut stream);
    if let Some(listing) = model_listing(&request) {
        let _ = stream.write_all(listing.as_bytes());
        return;
    }
    let index = turns.fetch_add(1, Ordering::SeqCst) + 1;
    let reply = &script.get(index - 1).cloned().unwrap_or(Reply::Text);
    recorded.lock().expect("requests").push(request);

    let body = sse_body(index, reply);
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The smallest `chat/completions` stream that is a finished turn of the
/// requested shape. This wire streams flat deltas and ends at `[DONE]`; there
/// is no item to open or close, so a whole reply is one chunk.
fn sse_body(index: usize, reply: &Reply) -> String {
    let id = format!("chatcmpl_{index}");
    let mut events = Vec::new();

    match reply {
        Reply::Text => {
            events.push(json!({
                "id": id, "model": "test-model",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": format!("reply-{index}")}}]
            }));
            events.push(json!({
                "id": id,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            }));
        }
        #[cfg(unix)]
        Reply::ToolCall { name, arguments } => {
            events.push(json!({
                "id": id, "model": "test-model",
                "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
                    "index": 0, "id": format!("call_{index}"), "type": "function",
                    // Arguments are a JSON *string* on this wire, and arrive in
                    // slices; one slice is enough to be a call.
                    "function": {"name": name, "arguments": arguments}
                }]}}]
            }));
            events.push(json!({
                "id": id,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
            }));
        }
    }

    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .chain(std::iter::once("data: [DONE]\n\n".to_string()))
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
