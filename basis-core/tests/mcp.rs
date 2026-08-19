//! `.mcp.json` discovery, end to end over real files.
//!
//! Every server here is fictional and nothing connects: the unit under test is
//! the path from a file on disk to the list of servers a runtime would be built
//! from. Spawning a real MCP server would test mentra's client, which mentra
//! already tests. What a client sends over `session/new` is `basis-acp`'s edge,
//! and is tested there.
//!
//! The whole file compiles away without the `mcp` feature, because so does
//! everything it names (ADR-0012).
#![cfg(feature = "mcp")]

use std::path::{Path, PathBuf};

use basis_core::{
    ContextScope, McpConfig, McpError, McpServer, RunConfig,
    mcp::{DEFAULT_GLOBAL_MCP_FILE, DEFAULT_WORKSPACE_MCP_FILE, discover, servers},
};

/// A config that looks nowhere except where a test puts something.
fn config(global: Option<PathBuf>) -> McpConfig {
    McpConfig {
        workspace_file: PathBuf::from(DEFAULT_WORKSPACE_MCP_FILE),
        global_dir: global,
        supplied: Vec::new(),
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

#[test]
fn a_realistic_file_becomes_the_servers_it_describes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
        r#"{
            "mcpServers": {
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]
                },
                "sqlite": {
                    "command": "uvx",
                    "args": ["mcp-server-sqlite", "--db-path", "./app.db"],
                    "env": {"LOG_LEVEL": "debug"},
                    "cwd": "/srv"
                }
            }
        }"#,
    );

    let found = servers(tmp.path(), &config(None)).expect("a well-formed file");

    let names: Vec<&str> = found.iter().map(McpServer::name).collect();
    assert_eq!(names, vec!["filesystem", "sqlite"]);

    let sqlite = found[1].as_stdio().expect("stdio");
    assert_eq!(sqlite.command, "uvx");
    assert_eq!(sqlite.args[0], "mcp-server-sqlite");
    assert_eq!(
        sqlite.env.get("LOG_LEVEL").map(String::as_str),
        Some("debug")
    );
    assert_eq!(sqlite.cwd.as_deref(), Some("/srv"));
}

#[test]
fn a_workspace_without_the_file_configures_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let found = servers(tmp.path(), &config(None)).expect("no file is not an error");

    assert!(
        found.is_empty(),
        "the cost of the mechanism stays at zero until someone writes the file"
    );
}

#[test]
fn each_source_reports_where_it_came_from() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
        r#"{"mcpServers":{"local":{"command":"a"}}}"#,
    );
    write(
        &global.join(DEFAULT_GLOBAL_MCP_FILE),
        r#"{"mcpServers":{"personal":{"command":"b"}}}"#,
    );

    let sources = discover(tmp.path(), &config(Some(global))).expect("discovery succeeds");

    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].scope, ContextScope::Workspace);
    assert!(sources[0].path.ends_with(DEFAULT_WORKSPACE_MCP_FILE));
    assert_eq!(sources[1].scope, ContextScope::Global);
    assert!(sources[1].path.ends_with(DEFAULT_GLOBAL_MCP_FILE));
}

#[test]
fn a_workspace_server_shadows_a_global_one_of_the_same_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
        r#"{"mcpServers":{"db":{"command":"project-db"}}}"#,
    );
    write(
        &global.join(DEFAULT_GLOBAL_MCP_FILE),
        r#"{"mcpServers":{"db":{"command":"personal-db"},"notes":{"command":"notes"}}}"#,
    );

    let found = servers(tmp.path(), &config(Some(global))).expect("layering succeeds");

    let names: Vec<&str> = found.iter().map(McpServer::name).collect();
    assert_eq!(
        names,
        vec!["db", "notes"],
        "one name is one server; everything else still layers in"
    );
    assert_eq!(found[0].as_stdio().expect("stdio").command, "project-db");
}

#[test]
fn a_broken_file_stops_the_run_rather_than_disappearing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
        r#"{"mcpServers": {"fs": {"args": ["-y"]}}}"#,
    );

    let error =
        servers(tmp.path(), &config(None)).expect_err("an entry with nothing to connect to");

    let rendered = error.to_string();
    assert!(matches!(error, McpError::Invalid { .. }), "{rendered}");
    assert!(
        rendered.contains("fs") && rendered.contains(DEFAULT_WORKSPACE_MCP_FILE),
        "the message must name the file and the server: {rendered}"
    );
}

#[test]
fn a_broken_file_never_quotes_what_it_read() {
    // `.mcp.json` is in basis's own `.gitignore`, and in most projects', because
    // `env` and `headers` carry credentials. This message reaches a client's
    // error pane and whatever the host logs, so it may name the file, the
    // server and the field — never a value.
    const SECRET: &str = "sk-live-do-not-print-me";

    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
        &format!(
            r#"{{"mcpServers":{{"gh":{{"command":"srv","env":{{"T":"{SECRET}${{NOPE}}"}}}}}}}}"#
        ),
    );

    let error = servers(tmp.path(), &config(None)).expect_err("NOPE is unset");
    let rendered = error.to_string();

    assert!(
        !rendered.contains(SECRET),
        "a value leaked into: {rendered}"
    );
    assert!(
        rendered.contains("gh") && rendered.contains("env.T") && rendered.contains("NOPE"),
        "the message must still be actionable: {rendered}"
    );
}

#[test]
fn a_parent_directorys_file_is_not_inherited() {
    // Instructions walk parents; credentials do not. A `.mcp.json` above the
    // workspace would mean spawning a program the operator never chose,
    // because of where they happened to be standing.
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("repo");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
        r#"{"mcpServers":{"inherited":{"command":"should-not-run"}}}"#,
    );

    let found = servers(&workspace, &config(None)).expect("discovery succeeds");

    assert!(
        found.is_empty(),
        "only the workspace root and the global config dir are searched: {found:?}"
    );
}

#[test]
fn a_misspelled_top_level_key_is_reported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
        r#"{"servers": {"fs": {"command": "npx"}}}"#,
    );

    let error = servers(tmp.path(), &config(None)).expect_err("no mcpServers object");

    assert!(
        matches!(error, McpError::NoServers { .. }),
        "a typo must not read as a workspace with no servers: {error}"
    );
}

#[test]
fn a_run_config_carries_discovery_settings_and_returns_new_values() {
    let base = RunConfig::new("/repo", "prompt");
    let derived = base
        .clone()
        .with_mcp(McpConfig::default().with_supplied(vec![McpServer::Sse(
            mentra::McpSseServerConfig::new("obs", "https://example.com/sse"),
        )]));

    assert!(
        base.mcp.supplied.is_empty(),
        "the original config must be untouched"
    );
    assert_eq!(derived.mcp.supplied.len(), 1);
    assert_eq!(
        derived.mcp.workspace_file,
        PathBuf::from(DEFAULT_WORKSPACE_MCP_FILE)
    );
}
