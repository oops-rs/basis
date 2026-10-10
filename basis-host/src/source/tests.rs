//! One runtime per process, one workspace per directory, and what a cached
//! workspace keeps running.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use basis::{
    ContextConfig, McpConfig, McpServer, Runtime, ToolsConfig, hooks::HooksConfig,
    skills::SkillsConfig, templates::TemplatesConfig,
};
use mentra::{McpServerConfig, McpSseServerConfig, McpStreamableHttpServerConfig, ModelSelector};

use super::{ConfiguredSource, WorkspaceKey, digest};
use crate::{Discovery, SessionSource, SessionTemplate};

#[test]
fn two_servers_differing_only_in_transport_hash_apart() {
    // Without the discriminant in the hash, an SSE server and a Streamable
    // HTTP server sharing a name and URL would key one workspace, and the
    // second client's transport would silently become the first's.
    let sse = McpServer::Sse(McpSseServerConfig::new("api", "https://example.com/mcp"));
    let http = McpServer::Http(McpStreamableHttpServerConfig::new(
        "api",
        "https://example.com/mcp",
    ));

    assert_ne!(digest(&[sse]), digest(&[http]));
}

#[test]
fn the_plaintext_override_reaches_the_digest() {
    // Latent until basis exposes the knob, armed now: two configs that
    // differ in where a credential may travel are two authorities.
    let base = || McpStreamableHttpServerConfig::new("api", "http://127.0.0.1/mcp");

    assert_ne!(
        digest(&[McpServer::Http(base())]),
        digest(&[McpServer::Http(base().allowing_plaintext_credentials())]),
    );
}

#[test]
fn the_clients_mcp_servers_reach_the_config() {
    let source = ConfiguredSource::new(None);

    let built = source.session_mcp(vec![McpServer::Stdio(McpServerConfig {
        name: "fs".to_string(),
        command: "/bin/mcp-fs".to_string(),
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
    })]);

    let supplied: Vec<&str> = built.supplied.iter().map(McpServer::name).collect();
    assert_eq!(
        supplied,
        vec!["fs"],
        "the session request is where a client says which servers it wants"
    );
}

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

fn offline_template() -> SessionTemplate {
    SessionTemplate::new().with_model(ModelSelector::Id("test-model".to_string()))
}

fn offline_discovery() -> Discovery {
    Discovery {
        context: ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        },
        skills: SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: None,
            shared_home_dir: false,
        },
        templates: TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        },
        hooks: HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
            supplied: Vec::new(),
        },
        tools: ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
            supplied: Vec::new(),
        },
        mcp: McpConfig {
            workspace_file: PathBuf::from(".mcp.json"),
            global_dir: None,
            supplied: Vec::new(),
        },
    }
}

#[tokio::test]
async fn two_sessions_on_one_workspace_share_one_runtime() {
    let repository = tempfile::tempdir().expect("tempdir");
    let runtime = offline_runtime();
    let source = ConfiguredSource::on_runtime(
        Arc::clone(&runtime),
        Some(offline_template().with_discovery(offline_discovery())),
    );

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

/// The claim `list_sessions` is built on, and the one nothing checked: a
/// session this source opens is filed under the directory it was opened
/// for, so a client asking about that directory gets it back.
///
/// Worth a test of its own because the doc beside `list_sessions` asserted
/// the opposite for two releases after it stopped being true — the tag is a
/// per-session fact now (`Runtime::mint`), and a shared runtime files each
/// workspace's conversations apart.
///
/// Read through `store::list_in` rather than through `list_sessions`
/// itself, which resolves the store root the process would use. Opening
/// that directory in a test is a legitimate refusal on any machine that
/// ran basis 0.6 (`RunError::LegacyStore`), and that the two roots are one
/// path is pinned in `basis`'s own `store` tests.
#[tokio::test]
async fn a_session_this_source_opens_is_listed_for_its_own_directory() {
    let store = tempfile::tempdir().expect("tempdir");
    let repository = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(
        Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_store_dir(store.path())
            .build()
            .expect("a runtime builds without touching the network"),
    );
    let source = ConfiguredSource::on_runtime(
        runtime,
        Some(offline_template().with_discovery(offline_discovery())),
    );

    let opened = source
        .create(repository.path().to_path_buf(), Vec::new())
        .await
        .expect("the session opens");

    let listed: Vec<String> = basis::store::list_in(store.path(), repository.path())
        .expect("lists")
        .into_iter()
        .map(|session| session.agent_id)
        .collect();
    assert_eq!(
        listed,
        vec![opened.agent_id().to_string()],
        "a client asking about this directory must be handed the session it opened here"
    );
    assert!(
        basis::store::list_in(store.path(), elsewhere.path())
            .expect("lists")
            .is_empty(),
        "and no other directory may claim it"
    );
}

#[test]
fn a_session_that_asked_for_different_servers_is_a_different_workspace() {
    let server = |command: &str| {
        vec![McpServer::Stdio(McpServerConfig {
            name: "fs".to_string(),
            command: command.to_string(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
        })]
    };
    let key = |mcp: Vec<McpServer>| WorkspaceKey::new(Path::new("/repo"), &mcp);

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
        WorkspaceKey::new(Path::new("/other-repo"), &[]),
        "and a directory is a key"
    );
}

#[test]
fn a_template_watches_a2a_files_by_default() {
    assert_eq!(
        SessionTemplate::new().a2a_watch_interval(),
        Some(basis::DEFAULT_A2A_WATCH_INTERVAL)
    );
    assert_eq!(
        SessionTemplate::new()
            .with_a2a_watch(None)
            .a2a_watch_interval(),
        None
    );
}

/// ADR-0031 §4: a server holds a workspace for as long as an editor has the
/// repository open, so an agent the operator adds meanwhile must reach it
/// without a restart.
#[tokio::test]
async fn a_cached_workspace_picks_up_an_agent_added_while_it_is_open() {
    let repository = tempfile::tempdir().expect("tempdir");
    let source = ConfiguredSource::on_runtime(
        offline_runtime(),
        Some(
            offline_template()
                .with_discovery(offline_discovery())
                .with_a2a_watch(Some(Duration::from_millis(20))),
        ),
    );
    let _session = source
        .create(repository.path().to_path_buf(), Vec::new())
        .await
        .expect("the session opens");
    let workspace = Arc::clone(&source.opened()[0]);
    assert!(workspace.a2a_agents().is_empty());

    // Unreachable on purpose: a configured agent is reported whether or not
    // it answers, and nothing here should depend on the network.
    let file = repository.path().join(".basis/a2a.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, r#"{"agents":{"nous":{"url":"http://127.0.0.1:1"}}}"#).unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while workspace.a2a_agents() != ["nous"] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the watch never picked the file up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
