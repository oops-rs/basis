//! What the server settles before any client is connected.
//!
//! Split out of `server.rs` for its size — the file was past the 800-line
//! ceiling with these inline, and past it again with the handlers still in one
//! module. The handlers are driven end to end over a real connection in
//! `tests/acp/`; what is left here is the pieces that can be checked without
//! one, which is why they sit together rather than beside each handler.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{
        ContentBlock, ErrorCode, InitializeRequest, ResourceLink, SessionCapabilities, TextContent,
    },
};

use super::config::{ConfiguredSource, ServeConfig, SessionSource};
use super::initialize;
use super::lifecycle::{session_info, setup_failed};
use super::turn::prompt_text;
use crate::mode::ApprovalMode;
use lan_core::{
    McpServer, PersistedSession, PreparedRun, RunConfig, RunError, provider::ProviderError,
};

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
    // -32601, which at least says lan cannot answer.
    assert!(
        capabilities(&ServeConfig::with_source(Ephemeral))
            .list
            .is_none()
    );
}

#[test]
fn no_authentication_method_is_offered() {
    // lan's credential comes from the environment. Offering a method here
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
            name: "lan acp".to_string(),
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
    assert_eq!(info.title.as_deref(), Some("lan acp"));
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
    let source = ConfiguredSource {
        template: Some(
            RunConfig::new("/placeholder", "").with_shell(lan_core::ShellAccess::Denied),
        ),
    };

    let built = source.config_for(PathBuf::from("/repo"), Vec::new());

    assert_eq!(built.workspace, PathBuf::from("/repo"));
    assert_eq!(
        built.shell,
        lan_core::ShellAccess::Denied,
        "everything the client cannot say must carry through"
    );
}

#[test]
fn the_clients_mcp_servers_reach_the_config() {
    let source = ConfiguredSource { template: None };

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
        "`lan acp --approve never` opens every session read-only"
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
