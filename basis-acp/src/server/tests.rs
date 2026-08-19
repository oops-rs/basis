//! What the server settles before any client is connected.
//!
//! Split out of `server.rs` for its size — the file was past the 800-line
//! ceiling with these inline, and past it again with the handlers still in one
//! module. The handlers are driven end to end over a real connection in
//! `tests/acp/`; what is left here is the pieces that can be checked without
//! one, which is why they sit together rather than beside each handler.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{
        ContentBlock, ErrorCode, InitializeRequest, ListSessionsRequest, ResourceLink,
        SessionCapabilities, TextContent,
    },
};

use super::config::{ServeConfig, SessionSource};
use super::initialize;
use super::lifecycle::{list_sessions, session_info, setup_failed};
use super::turn::prompt_text;
use super::workspaces::{ConfiguredSource, WorkspaceKey};
use crate::mode::ApprovalMode;
use basis_core::{
    ContextConfig, McpServer, PersistedSession, PreparedRun, RunConfig, RunError, Runtime,
    hooks::HooksConfig, mcp::McpConfig, provider::ProviderError, skills::SkillsConfig,
    templates::TemplatesConfig,
};
use mentra::ModelSelector;

/// A source that cannot enumerate, which is what most hosts supplying
/// their own sessions are.
struct Ephemeral;

#[async_trait::async_trait]
impl SessionSource for Ephemeral {
    async fn create(&self, _cwd: PathBuf, _mcp: Vec<McpServer>) -> Result<PreparedRun, RunError> {
        Err(RunError::NoSuchSession)
    }
}

fn capabilities(config: &ServeConfig) -> SessionCapabilities {
    initialize(&InitializeRequest::new(ProtocolVersion::V1), config)
        .agent_capabilities
        .session_capabilities
}

#[test]
fn initialize_advertises_resumable_sessions() {
    let response = initialize(
        &InitializeRequest::new(ProtocolVersion::V1),
        &ServeConfig::default(),
    );

    assert!(
        response.agent_capabilities.load_session,
        "sessions are persisted mentra agents, so a client can reconnect"
    );
    assert_eq!(
        response.protocol_version,
        ProtocolVersion::V1,
        "the client's version is echoed, not overridden"
    );
}

#[test]
fn initialize_advertises_only_the_session_methods_lan_answers() {
    let capabilities = capabilities(&ServeConfig::default());

    assert!(capabilities.resume.is_some());
    assert!(capabilities.close.is_some());
    assert!(
        capabilities.list.is_some(),
        "the default source reads mentra's store, so it can enumerate"
    );
    assert!(
        capabilities.delete.is_none(),
        "mentra's store has no delete; claiming one would promise a deletion that undoes itself"
    );
}

#[test]
fn a_source_that_cannot_enumerate_does_not_claim_a_list() {
    // Reporting "no sessions" for a workspace that has some is worse than
    // -32601, which at least says basis cannot answer.
    assert!(
        capabilities(&ServeConfig::with_source(Ephemeral))
            .list
            .is_none()
    );
}

#[tokio::test]
async fn a_source_that_cannot_enumerate_refuses_the_call_it_never_claimed() {
    // The other half of the same promise, and the half that was missing: the
    // handler is registered whatever the source is, so a client that asks
    // without reading the capability has to hear the same -32601 an
    // unadvertised method gives it — not an empty list for a workspace whose
    // conversations this source simply cannot see.
    let error = list_sessions(
        &ServeConfig::with_source(Ephemeral),
        ListSessionsRequest::new().cwd(PathBuf::from("/repo")),
    )
    .await
    .expect_err("a source with no registry must not answer for one");

    assert_eq!(error.code, ErrorCode::MethodNotFound);
}

#[test]
fn no_authentication_method_is_offered() {
    // basis's credential comes from the environment. Offering a method here
    // would invite a call to `authenticate`, which answers -32601.
    assert!(
        initialize(
            &InitializeRequest::new(ProtocolVersion::V1),
            &ServeConfig::default()
        )
        .auth_methods
        .is_empty()
    );
}

#[test]
fn a_listed_session_is_reported_in_the_workspace_it_was_listed_for() {
    let info = session_info(
        PersistedSession {
            agent_id: "agent-1".to_string(),
            name: "basis acp".to_string(),
            messages: 4,
        },
        Path::new("/repo"),
    );

    assert_eq!(
        &*info.session_id.0, "agent-1",
        "the agent id is the session id"
    );
    assert_eq!(
        info.cwd,
        PathBuf::from("/repo"),
        "a conversation is in this list because it belongs to this workspace"
    );
    assert_eq!(info.title.as_deref(), Some("basis acp"));
    assert_eq!(
        info.updated_at, None,
        "mentra exposes no timestamp, and a made-up one would sort a picker by nothing"
    );
}

#[test]
fn prompt_text_joins_text_blocks() {
    let text = prompt_text(&[
        ContentBlock::Text(TextContent::new("first".to_string())),
        ContentBlock::Text(TextContent::new("second".to_string())),
    ]);

    assert_eq!(text, "first\nsecond");
}

#[test]
fn a_resource_link_is_named_rather_than_dropped() {
    let text = prompt_text(&[ContentBlock::ResourceLink(ResourceLink::new(
        "notes.md".to_string(),
        "file:///repo/notes.md".to_string(),
    ))]);

    assert!(
        text.contains("notes.md") && text.contains("file:///repo/notes.md"),
        "what the user attached must survive into the prompt: {text}"
    );
}

#[test]
fn an_empty_prompt_produces_no_text() {
    assert!(prompt_text(&[]).is_empty());
}

#[test]
fn the_config_template_takes_the_clients_working_directory() {
    // Denied rather than granted, so the assertion below has something to
    // catch: granted is the default, and a template that was dropped
    // entirely would still look right.
    let source = ConfiguredSource::new(Some(
        RunConfig::new("/placeholder", "").with_shell(basis_core::ShellAccess::Denied),
    ));

    let built = source.config_for(PathBuf::from("/repo"), Vec::new());

    assert_eq!(built.workspace, PathBuf::from("/repo"));
    assert_eq!(
        built.shell,
        basis_core::ShellAccess::Denied,
        "everything the client cannot say must carry through"
    );
}

#[test]
fn the_clients_mcp_servers_reach_the_config() {
    let source = ConfiguredSource::new(None);

    let built = source.config_for(
        PathBuf::from("/repo"),
        vec![McpServer::Stdio(mentra::McpServerConfig {
            name: "fs".to_string(),
            command: "/bin/mcp-fs".to_string(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
        })],
    );

    let supplied: Vec<&str> = built.mcp.supplied.iter().map(McpServer::name).collect();
    assert_eq!(
        supplied,
        vec!["fs"],
        "session/new is where a client says which servers it wants"
    );
}

#[test]
fn a_session_opens_asking_unless_the_operator_says_otherwise() {
    // The library default is to allow everything, which is right for a
    // headless run and wrong here: a client that can be asked should be.
    assert_eq!(ServeConfig::default().initial_mode, ApprovalMode::Prompt);
    assert_eq!(
        ServeConfig::new(RunConfig::new("/repo", "")).initial_mode,
        ApprovalMode::Prompt,
        "a template says what a run is, not how much it may do without asking"
    );
    assert_eq!(
        ServeConfig::default()
            .with_initial_mode(ApprovalMode::Never)
            .initial_mode,
        ApprovalMode::Never,
        "`basis acp --approve never` opens every session read-only"
    );
}

#[test]
fn a_missing_credential_is_an_authentication_failure_not_an_internal_one() {
    let error = setup_failed(RunError::Provider(ProviderError::NoCredential));

    assert_eq!(error.code, ErrorCode::AuthRequired);
    assert!(
        error
            .data
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .is_some_and(|data| data.contains("ANTHROPIC_API_KEY")),
        "the actionable part is which variable to set: {:?}",
        error.data
    );
}

#[test]
fn other_setup_failures_stay_internal_errors() {
    // Reporting these as `auth_required` would send a client looking for a
    // login that would not have helped.
    let error = setup_failed(RunError::NoSuchSession);

    assert_eq!(error.code, ErrorCode::InternalError);
}

/// A runtime that resolves offline: a loopback endpoint nothing here dials and
/// a placeholder credential, so building it does no more than pick a provider.
/// Its history is ephemeral, so a test suite writes no database.
fn offline_runtime() -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("a runtime builds without touching the network"),
    )
}

/// A template that looks nowhere except where a test put something.
///
/// Every discovery root is pinned, global ones to `None`: an unpinned one would
/// read the developer's own configuration — and, for MCP, spawn the servers
/// their `mcp.json` names. The model is an id rather than "newest available",
/// which is what keeps resolution off the network.
fn offline_template() -> RunConfig {
    RunConfig::new("/placeholder", "")
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
        .with_mcp(McpConfig {
            workspace_file: PathBuf::from(".mcp.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
}

/// ADR-0018's acceptance for basis-acp: a server holding two sessions on one
/// `cwd` holds one runtime and one workspace, not two of each.
///
/// Identity rather than a count of store files, which cannot tell the two
/// shapes apart — N private runtimes still share mentra's one default
/// directory. What a runtime per session actually cost was a second provider
/// resolution, a second store handle, and a second copy of every MCP server
/// and hook the repository configures; pointer identity is what says none of
/// that happened twice.
#[tokio::test]
async fn two_sessions_on_one_workspace_share_one_runtime() {
    let repository = tempfile::tempdir().expect("tempdir");
    let runtime = offline_runtime();
    let source = ConfiguredSource::on_runtime(Arc::clone(&runtime), Some(offline_template()));

    let first = source
        .create(repository.path().to_path_buf(), Vec::new())
        .await
        .expect("the first session opens");
    let second = source
        .create(repository.path().to_path_buf(), Vec::new())
        .await
        .expect("the second session opens");

    assert_ne!(
        first.agent_id(),
        second.agent_id(),
        "two sessions, not one conversation handed out twice"
    );

    let opened = source.opened();
    assert_eq!(
        opened.len(),
        1,
        "one directory is one workspace, however many sessions were minted on it"
    );
    assert!(
        std::ptr::eq(opened[0].mentra_runtime(), runtime.mentra_runtime()),
        "and both were minted on the one runtime this process built"
    );
}

/// The other half of the same key: two sessions share a workspace only when
/// they asked for the same servers.
///
/// Sharing on the directory alone would hand the second session the first
/// one's roster and drop what it asked for, which reads exactly like a server
/// with nothing to offer. Asserted on the key rather than on two opened
/// workspaces, because opening one spawns the programs it names.
#[test]
fn a_session_that_asked_for_different_servers_is_a_different_workspace() {
    let source = ConfiguredSource::new(Some(offline_template()));
    let server = |command: &str| {
        vec![McpServer::Stdio(mentra::McpServerConfig {
            name: "fs".to_string(),
            command: command.to_string(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
        })]
    };
    let key = |mcp| WorkspaceKey::of(&source.config_for(PathBuf::from("/repo"), mcp));

    assert_eq!(
        key(Vec::new()),
        key(Vec::new()),
        "the same directory asked about the same way is one workspace"
    );
    assert_ne!(
        key(Vec::new()),
        key(server("/bin/mcp-fs")),
        "a supplied server is not none"
    );
    assert_ne!(
        key(server("/bin/mcp-fs")),
        key(server("/bin/other-fs")),
        "one name, two commands: a client that named a different program must get it"
    );
    assert_ne!(
        key(Vec::new()),
        WorkspaceKey::of(&source.config_for(PathBuf::from("/other-repo"), Vec::new())),
        "and a directory is a key"
    );
}
