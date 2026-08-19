//! The MCP servers an ACP client configures for a session.
//!
//! A client sends `mcpServers` on `session/new`, and it is the strongest
//! authority there is about what this session should reach: it is answering
//! for this workspace, in this editor, right now. basis's job is to translate,
//! not to filter.
//!
//! # Nothing is dropped
//!
//! ACP's `McpServer` has variants for transports mentra has no client for, and
//! is `#[non_exhaustive]` besides, so a wildcard arm is unavoidable. Every arm
//! basis cannot serve returns an error naming the server and the transport,
//! because a client that configured a server and got silence cannot tell that
//! apart from a server that advertised no tools.
//!
//! Placeholders are *not* expanded here. `${VAR}` in a `.mcp.json` is a
//! convention for a file a human wrote; a value on the wire is one a client
//! already resolved, and re-expanding it would corrupt a literal.

use std::collections::HashMap;

use agent_client_protocol::schema::v1::{McpServer as AcpServer, McpServerSse, McpServerStdio};
use mentra::{McpServerConfig, McpSseServerConfig};

use basis::mcp::{McpError, McpServer};

/// How an error from this module names where the configuration came from.
const ORIGIN: &str = "the ACP client";

/// Translates a client's `mcpServers` into servers basis can register.
///
/// All-or-nothing: one unserviceable entry fails the whole `session/new`,
/// because a session that came up missing a server the client asked for would
/// look identical to one where the server had nothing to offer.
pub fn from_acp(servers: &[AcpServer]) -> Result<Vec<McpServer>, McpError> {
    servers.iter().map(from_one).collect()
}

fn from_one(server: &AcpServer) -> Result<McpServer, McpError> {
    match server {
        AcpServer::Stdio(stdio) => stdio_server(stdio),
        AcpServer::Sse(sse) => Ok(sse_server(sse)),
        AcpServer::Http(http) => Err(McpError::UnsupportedTransport {
            origin: ORIGIN.to_string(),
            name: http.name.clone(),
            transport: "Streamable HTTP".to_string(),
        }),
        _ => Err(McpError::UnknownTransport {
            origin: ORIGIN.to_string(),
        }),
    }
}

fn stdio_server(stdio: &McpServerStdio) -> Result<McpServer, McpError> {
    // A path that is not UTF-8 cannot become mentra's `String` command. Saying
    // so beats a lossy conversion, which would produce a command that spawns
    // nothing and blames the server. The path itself stays out of the message
    // for the same reason the file reader keeps values out of its own — see
    // [`McpError`].
    let command = stdio
        .command
        .to_str()
        .ok_or_else(|| McpError::Invalid {
            origin: ORIGIN.to_string(),
            name: stdio.name.clone(),
            reason: "has a command path that is not valid UTF-8".to_string(),
        })?
        .to_string();

    Ok(McpServer::Stdio(McpServerConfig {
        name: stdio.name.clone(),
        command,
        args: stdio.args.clone(),
        env: stdio
            .env
            .iter()
            .map(|variable| (variable.name.clone(), variable.value.clone()))
            .collect::<HashMap<_, _>>(),
        // ACP has no working directory for a server; mentra's default of
        // inheriting basis's is the same thing the client's own agent would do.
        cwd: None,
    }))
}

fn sse_server(sse: &McpServerSse) -> McpServer {
    let config = sse.headers.iter().fold(
        McpSseServerConfig::new(sse.name.clone(), sse.url.clone()),
        |config, header| config.with_header(header.name.clone(), header.value.clone()),
    );

    McpServer::Sse(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{EnvVariable, HttpHeader, McpServerHttp};

    #[test]
    fn no_servers_maps_to_no_servers() {
        assert!(from_acp(&[]).expect("nothing to translate").is_empty());
    }

    #[test]
    fn a_stdio_server_carries_its_command_args_and_environment() {
        let servers = from_acp(&[AcpServer::Stdio(
            McpServerStdio::new("fs", "/usr/local/bin/mcp-fs")
                .args(vec!["--root".to_string(), "/repo".to_string()])
                .env(vec![EnvVariable::new("TOKEN", "secret")]),
        )])
        .expect("stdio is the transport every agent must support");

        let config = servers[0].as_stdio().expect("stdio");
        assert_eq!(config.name, "fs");
        assert_eq!(config.command, "/usr/local/bin/mcp-fs");
        assert_eq!(config.args, vec!["--root", "/repo"]);
        assert_eq!(config.env.get("TOKEN").map(String::as_str), Some("secret"));
    }

    #[test]
    fn an_sse_server_carries_its_url_and_headers() {
        let servers = from_acp(&[AcpServer::Sse(
            McpServerSse::new("obs", "https://example.com/sse")
                .headers(vec![HttpHeader::new("authorization", "Bearer t")]),
        )])
        .expect("mentra speaks this transport");

        let config = servers[0].as_sse().expect("sse");
        assert_eq!(config.url, "https://example.com/sse");
        assert_eq!(
            config
                .headers
                .get("authorization")
                .map(mentra::mcp::SecretString::expose_secret),
            Some("Bearer t")
        );
    }

    #[test]
    fn an_http_server_is_refused_rather_than_dropped() {
        let error = from_acp(&[AcpServer::Http(McpServerHttp::new(
            "api",
            "https://example.com/mcp",
        ))])
        .expect_err("mentra has no Streamable HTTP client");

        let rendered = error.to_string();
        assert!(
            rendered.contains("api"),
            "the server must be named: {rendered}"
        );
        assert!(matches!(error, McpError::UnsupportedTransport { .. }));
    }

    #[test]
    fn one_unserviceable_server_fails_the_whole_set() {
        let error = from_acp(&[
            AcpServer::Stdio(McpServerStdio::new("fs", "/usr/local/bin/mcp-fs")),
            AcpServer::Http(McpServerHttp::new("api", "https://example.com/mcp")),
        ])
        .expect_err("a partly-configured session is worse than none");

        assert!(
            matches!(error, McpError::UnsupportedTransport { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_placeholder_on_the_wire_stays_literal() {
        let servers = from_acp(&[AcpServer::Stdio(
            McpServerStdio::new("gh", "/bin/srv").env(vec![EnvVariable::new("T", "${GH_TOKEN}")]),
        )])
        .expect("the client already resolved its own configuration");

        assert_eq!(
            servers[0]
                .as_stdio()
                .expect("stdio")
                .env
                .get("T")
                .map(String::as_str),
            Some("${GH_TOKEN}")
        );
    }
}
