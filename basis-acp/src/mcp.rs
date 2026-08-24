//! The MCP servers an ACP client configures for a session.
//!
//! A client sends `mcpServers` on `session/new`, and it is the strongest
//! authority there is about what this session should reach: it is answering
//! for this workspace, in this editor, right now. basis's job is to translate,
//! not to filter.
//!
//! # Nothing is dropped
//!
//! ACP's three named transports — stdio, SSE, Streamable HTTP — all
//! translate, but the enum is `#[non_exhaustive]`, so a wildcard arm is
//! unavoidable. A variant this build has no name for returns an error rather
//! than silence, because a client that configured a server and got silence
//! cannot tell that apart from a server that advertised no tools. The same
//! rule covers a repeated header name: ACP hands headers as an ordered list,
//! mentra stores them as a map, and folding a duplicate into the map would
//! silently keep whichever came last — so a repeat is refused instead. Names
//! differing only in case stay distinct here; deciding they are one header
//! is the transport's business, not the translation's.
//!
//! Placeholders are *not* expanded here. `${VAR}` in a `.mcp.json` is a
//! convention for a file a human wrote; a value on the wire is one a client
//! already resolved, and re-expanding it would corrupt a literal.

use std::collections::HashMap;

use agent_client_protocol::schema::v1::{
    HttpHeader, McpServer as AcpServer, McpServerHttp, McpServerSse, McpServerStdio,
};
use mentra::{McpServerConfig, McpSseServerConfig, McpStreamableHttpServerConfig};

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
        AcpServer::Sse(sse) => sse_server(sse),
        AcpServer::Http(http) => http_server(http),
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

fn sse_server(sse: &McpServerSse) -> Result<McpServer, McpError> {
    let config = with_headers(
        &sse.name,
        &sse.headers,
        McpSseServerConfig::new(sse.name.clone(), sse.url.clone()),
        |config, name, value| config.with_header(name, value),
    )?;

    Ok(McpServer::Sse(config))
}

fn http_server(http: &McpServerHttp) -> Result<McpServer, McpError> {
    let config = with_headers(
        &http.name,
        &http.headers,
        McpStreamableHttpServerConfig::new(http.name.clone(), http.url.clone()),
        |config, name, value| config.with_header(name, value),
    )?;

    Ok(McpServer::Http(config))
}

/// Folds the client's ordered headers into a remote config, refusing a
/// repeated name rather than letting the map keep whichever came last.
///
/// The value stays out of the error for the usual reason: a header value is
/// where the credential is.
fn with_headers<C>(
    server: &str,
    headers: &[HttpHeader],
    config: C,
    add: impl Fn(C, String, String) -> C,
) -> Result<C, McpError> {
    let mut seen = std::collections::HashSet::with_capacity(headers.len());

    headers.iter().try_fold(config, |config, header| {
        if !seen.insert(header.name.as_str()) {
            return Err(McpError::Invalid {
                origin: ORIGIN.to_string(),
                name: server.to_string(),
                reason: format!("repeats the header `{}`", header.name),
            });
        }

        Ok(add(config, header.name.clone(), header.value.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::EnvVariable;

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
    fn an_http_server_carries_its_url_and_headers() {
        let servers = from_acp(&[AcpServer::Http(
            McpServerHttp::new("api", "https://example.com/mcp")
                .headers(vec![HttpHeader::new("authorization", "Bearer t")]),
        )])
        .expect("mentra speaks this transport");

        let config = servers[0].as_http().expect("http");
        assert_eq!(config.url, "https://example.com/mcp");
        assert_eq!(
            config
                .headers
                .get("authorization")
                .map(mentra::mcp::SecretString::expose_secret),
            Some("Bearer t")
        );
    }

    #[test]
    fn a_repeated_header_name_is_refused_not_last_wins() {
        // Silently keeping the second `authorization` is a debugging session
        // for whoever wrote the first; the module doc promises nothing is
        // dropped.
        let error = from_acp(&[AcpServer::Sse(
            McpServerSse::new("obs", "https://example.com/sse").headers(vec![
                HttpHeader::new("authorization", "Bearer one"),
                HttpHeader::new("authorization", "Bearer two"),
            ]),
        )])
        .expect_err("a duplicate must fail session/new");

        let rendered = error.to_string();
        assert!(rendered.contains("authorization"), "{rendered}");
        assert!(!rendered.contains("Bearer"), "values stay out: {rendered}");

        let error = from_acp(&[AcpServer::Http(
            McpServerHttp::new("api", "https://example.com/mcp").headers(vec![
                HttpHeader::new("x-key", "one"),
                HttpHeader::new("x-key", "two"),
            ]),
        )])
        .expect_err("both remote transports run the same fold");
        assert!(error.to_string().contains("x-key"), "{error}");
    }

    #[test]
    fn a_mixed_set_translates_every_server() {
        let servers = from_acp(&[
            AcpServer::Stdio(McpServerStdio::new("fs", "/usr/local/bin/mcp-fs")),
            AcpServer::Http(McpServerHttp::new("api", "https://example.com/mcp")),
        ])
        .expect("every transport a client can name translates");

        assert_eq!(servers.len(), 2);
        assert!(servers[0].as_stdio().is_some());
        assert!(servers[1].as_http().is_some());
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
