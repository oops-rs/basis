//! A client's `mcpServers`, meeting the workspace's own.
//!
//! The unit tests beside the mapping cover each transport in isolation. What is
//! worth checking from outside is the join: a server the client sent has to
//! outrank the same name in a `.mcp.json`, and every transport a client can
//! name — stdio, SSE, Streamable HTTP — has to survive it untouched.
//!
//! Every server here is fictional and nothing connects.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{McpServer as AcpServer, McpServerHttp, McpServerStdio};
use basis::mcp::{DEFAULT_WORKSPACE_MCP_FILE, McpConfig, servers};
use basis_acp::from_acp;

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
fn a_client_supplied_http_server_survives_the_join() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let supplied = from_acp(&[AcpServer::Http(McpServerHttp::new(
        "api",
        "https://example.com/mcp",
    ))])
    .expect("mentra speaks Streamable HTTP");

    let found = servers(tmp.path(), &config().with_supplied(supplied)).expect("layering");

    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].as_http().expect("http").url,
        "https://example.com/mcp",
        "the transport rides the join untouched"
    );
}
