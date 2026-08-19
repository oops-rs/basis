//! A client's `mcpServers`, meeting the workspace's own.
//!
//! The unit tests beside the mapping cover each transport in isolation. What is
//! worth checking from outside is the join: a server the client sent has to
//! outrank the same name in a `.mcp.json`, and a transport basis cannot serve has
//! to stop `session/new` rather than quietly shrink the roster.
//!
//! Every server here is fictional and nothing connects.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{McpServer as AcpServer, McpServerHttp, McpServerStdio};
use basis_acp::from_acp;
use basis_core::mcp::{DEFAULT_WORKSPACE_MCP_FILE, McpConfig, servers};

/// A config that looks nowhere except where a test puts something.
fn config() -> McpConfig {
    McpConfig {
        workspace_file: PathBuf::from(DEFAULT_WORKSPACE_MCP_FILE),
        global_dir: None,
        supplied: Vec::new(),
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

#[test]
fn a_client_supplied_server_wins_over_the_workspaces_own() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
        r#"{"mcpServers":{"fs":{"command":"from-the-file"}}}"#,
    );

    let supplied = from_acp(&[AcpServer::Stdio(McpServerStdio::new(
        "fs",
        "/bin/from-the-client",
    ))])
    .expect("stdio is always serviceable");

    let found = servers(tmp.path(), &config().with_supplied(supplied)).expect("layering");

    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].as_stdio().expect("stdio").command,
        "/bin/from-the-client",
        "the client answers for this session in particular"
    );
}

#[test]
fn a_client_transport_lan_cannot_serve_is_refused_not_ignored() {
    let error = from_acp(&[AcpServer::Http(McpServerHttp::new(
        "api",
        "https://example.com/mcp",
    ))])
    .expect_err("mentra has no Streamable HTTP client");

    assert!(
        error.to_string().contains("api"),
        "session/new must fail loudly enough to name the server: {error}"
    );
}
