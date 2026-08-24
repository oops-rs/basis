//! The `.mcp.json` format.
//!
//! One object, `mcpServers`, keyed by name — the name is the map key rather
//! than a field, which is the one place this format and mentra's
//! [`McpServerConfig`] disagree.
//!
//! An entry names its transport with `type`, or omits it and lets its shape
//! say: `command` means stdio, `url` means a remote server. Both or neither is
//! a mistake worth reporting rather than guessing at, because either guess
//! silently starts something the operator did not ask for.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use mentra::{McpServerConfig, McpSseServerConfig, McpStreamableHttpServerConfig};
use serde::Deserialize;

use crate::expand::expand;

use super::{McpError, McpServer};

/// The whole file.
#[derive(Debug, Deserialize)]
struct McpFile {
    /// Optional so that its absence can be reported. A missing `mcpServers`
    /// is almost always a misspelled one, and defaulting to empty would turn
    /// that into a workspace whose servers quietly never start.
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<BTreeMap<String, RawServer>>,
}

/// One entry, before it is known which transport it describes.
///
/// Unknown fields are tolerated: these files are shared with other agents, and
/// rejecting a key basis has no opinion about would make a working file
/// unreadable for no gain.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawServer {
    #[serde(rename = "type")]
    transport: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// Which transport an entry asks for, once the question is settled.
enum Transport {
    Stdio,
    Sse,
    Http,
}

/// Reads `text` as the file at `path`.
pub(super) fn parse(path: &Path, text: &str) -> Result<Vec<McpServer>, McpError> {
    parse_with(path, text, &|name| std::env::var(name).ok())
}

/// The same, against an explicit environment, so the rules are testable
/// without mutating the process's own.
fn parse_with(
    path: &Path,
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<McpServer>, McpError> {
    let file: McpFile = serde_json::from_str(text).map_err(|source| McpError::Parse {
        path: path.to_path_buf(),
        // serde's own message quotes the value it choked on, which in this
        // file is as likely as not a credential. Location and kind only.
        problem: match source.classify() {
            serde_json::error::Category::Syntax => "a syntax error",
            serde_json::error::Category::Data => "a value of the wrong type",
            serde_json::error::Category::Eof => "an unexpected end of input",
            serde_json::error::Category::Io => "a read error",
        },
        line: source.line(),
        column: source.column(),
    })?;

    let Some(entries) = file.mcp_servers else {
        return Err(McpError::NoServers {
            path: path.to_path_buf(),
        });
    };

    let origin = path.display().to_string();

    entries
        .into_iter()
        .map(|(name, raw)| raw.into_server(&origin, name, lookup))
        .collect()
}

impl RawServer {
    fn into_server(
        self,
        origin: &str,
        name: String,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<McpServer, McpError> {
        let invalid = |reason: String| McpError::Invalid {
            origin: origin.to_string(),
            name: name.clone(),
            reason,
        };

        // Expansion failures name the field rather than quoting it. `env` and
        // `headers` are where credentials live, so the field's *name* is the
        // most an error may say about it — see [`McpError`].
        let expanded = |field: &str, raw: &str| -> Result<String, McpError> {
            expand(raw, lookup)
                .map_err(|reason| invalid(format!("has a `{field}` value that {reason}")))
        };

        if name.trim().is_empty() {
            return Err(invalid("has an empty name".to_string()));
        }

        match self.transport(origin, &name)? {
            Transport::Stdio => {
                let command = self
                    .command
                    .as_deref()
                    .filter(|command| !command.trim().is_empty())
                    .ok_or_else(|| invalid("has no `command` to run".to_string()))?;

                Ok(McpServer::Stdio(McpServerConfig {
                    command: expanded("command", command)?,
                    args: self
                        .args
                        .iter()
                        .enumerate()
                        .map(|(index, arg)| expanded(&format!("args[{index}]"), arg))
                        .collect::<Result<_, _>>()?,
                    env: self
                        .env
                        .iter()
                        .map(|(key, value)| {
                            Ok((key.clone(), expanded(&format!("env.{key}"), value)?))
                        })
                        .collect::<Result<_, McpError>>()?,
                    cwd: self
                        .cwd
                        .as_deref()
                        .map(|cwd| expanded("cwd", cwd))
                        .transpose()?,
                    name,
                }))
            }
            Transport::Sse => {
                let url = self
                    .url
                    .as_deref()
                    .filter(|url| !url.trim().is_empty())
                    .ok_or_else(|| invalid("has no `url` to reach".to_string()))?;

                let config = McpSseServerConfig::new(name.clone(), expanded("url", url)?);

                self.headers
                    .iter()
                    .try_fold(config, |config, (key, value)| {
                        Ok(config
                            .with_header(key.clone(), expanded(&format!("headers.{key}"), value)?))
                    })
                    .map(McpServer::Sse)
            }
            Transport::Http => {
                let url = self
                    .url
                    .as_deref()
                    .filter(|url| !url.trim().is_empty())
                    .ok_or_else(|| invalid("has no `url` to reach".to_string()))?;

                let config =
                    McpStreamableHttpServerConfig::new(name.clone(), expanded("url", url)?);

                self.headers
                    .iter()
                    .try_fold(config, |config, (key, value)| {
                        Ok(config
                            .with_header(key.clone(), expanded(&format!("headers.{key}"), value)?))
                    })
                    .map(McpServer::Http)
            }
        }
    }

    /// Settles which transport the entry describes.
    fn transport(&self, origin: &str, name: &str) -> Result<Transport, McpError> {
        let invalid = |reason: String| McpError::Invalid {
            origin: origin.to_string(),
            name: name.to_string(),
            reason,
        };

        match self.transport.as_deref().map(str::trim) {
            Some("stdio") => Ok(Transport::Stdio),
            Some("sse") => Ok(Transport::Sse),
            // Both spellings are in the wild for the same transport.
            Some("http") | Some("streamable-http") => Ok(Transport::Http),
            Some(other) => Err(invalid(format!("names an unknown transport `{other}`"))),
            // The original format had no `type` field at all, so an entry's
            // shape is the older way of saying the same thing.
            None => match (self.command.is_some(), self.url.is_some()) {
                (true, false) => Ok(Transport::Stdio),
                // Deliberately still SSE, never Streamable HTTP: a bare `url`
                // has meant the 2024-11-05 HTTP+SSE transport since before the
                // third one existed, and a file keeps its meaning. A
                // Streamable HTTP server says `type: "http"`.
                (false, true) => Ok(Transport::Sse),
                (true, true) => Err(invalid(
                    "has both `command` and `url`; set `type` to say which is meant".to_string(),
                )),
                (false, false) => Err(invalid("has neither `command` nor `url`".to_string())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing_set(_: &str) -> Option<String> {
        None
    }

    fn parse_text(text: &str) -> Result<Vec<McpServer>, McpError> {
        parse_with(Path::new("/repo/.mcp.json"), text, &nothing_set)
    }

    #[test]
    fn the_documented_shape_becomes_a_stdio_server() {
        let servers = parse_text(
            r#"{
                "mcpServers": {
                    "filesystem": {
                        "command": "npx",
                        "args": ["-y", "@modelcontextprotocol/server-filesystem"],
                        "env": {"ROOT": "/repo"}
                    }
                }
            }"#,
        )
        .expect("a well-formed file");

        assert_eq!(servers.len(), 1);
        let config = servers[0].as_stdio().expect("stdio");
        assert_eq!(config.name, "filesystem", "the map key becomes the name");
        assert_eq!(config.command, "npx");
        assert_eq!(
            config.args,
            vec!["-y", "@modelcontextprotocol/server-filesystem"]
        );
        assert_eq!(config.env.get("ROOT").map(String::as_str), Some("/repo"));
        assert_eq!(config.cwd, None);
    }

    #[test]
    fn an_empty_server_object_is_allowed() {
        let servers = parse_text(r#"{"mcpServers": {}}"#).expect("explicitly empty is a choice");

        assert!(servers.is_empty());
    }

    #[test]
    fn a_missing_mcp_servers_key_is_an_error() {
        let error = parse_text(r#"{"mcpservers": {}}"#).expect_err("a misspelled key is caught");

        assert!(matches!(error, McpError::NoServers { .. }), "{error}");
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let servers = parse_text(
            r#"{
                "$schema": "https://example.com/mcp.json",
                "mcpServers": {"fs": {"command": "npx", "disabled": false}}
            }"#,
        )
        .expect("these files are shared with other agents");

        assert_eq!(servers.len(), 1);
    }

    #[test]
    fn cwd_is_carried_through() {
        let servers = parse_text(r#"{"mcpServers":{"fs":{"command":"srv","cwd":"/tmp"}}}"#)
            .expect("a well-formed file");

        assert_eq!(
            servers[0].as_stdio().expect("stdio").cwd.as_deref(),
            Some("/tmp")
        );
    }

    #[test]
    fn a_url_without_a_type_is_an_sse_server() {
        let servers = parse_text(r#"{"mcpServers":{"obs":{"url":"https://example.com/sse"}}}"#)
            .expect("shape names the transport");

        let config = servers[0].as_sse().expect("sse");
        assert_eq!(config.name, "obs");
        assert_eq!(config.url, "https://example.com/sse");
    }

    #[test]
    fn an_explicit_sse_type_is_honored() {
        let servers =
            parse_text(r#"{"mcpServers":{"obs":{"type":"sse","url":"https://example.com/sse"}}}"#)
                .expect("an explicit type");

        assert!(servers[0].as_sse().is_some());
    }

    #[test]
    fn sse_headers_are_carried_through() {
        let servers = parse_text(
            r#"{"mcpServers":{"obs":{"url":"https://example.com/sse","headers":{"authorization":"Bearer t"}}}}"#,
        )
        .expect("a well-formed file");

        let config = servers[0].as_sse().expect("sse");
        assert_eq!(
            config
                .headers
                .get("authorization")
                .map(mentra::mcp::SecretString::expose_secret),
            Some("Bearer t")
        );
    }

    #[test]
    fn an_http_type_is_a_streamable_http_server() {
        let servers = parse_text(r#"{"mcpServers":{"api":{"type":"http","url":"https://x/mcp"}}}"#)
            .expect("mentra speaks this transport");

        let config = servers[0].as_http().expect("http");
        assert_eq!(config.name, "api");
        assert_eq!(config.url, "https://x/mcp");
    }

    #[test]
    fn the_streamable_http_alias_names_the_same_transport() {
        let servers = parse_text(
            r#"{"mcpServers":{"api":{"type":"streamable-http","url":"https://x/mcp"}}}"#,
        )
        .expect("both spellings are in the wild");

        assert!(servers[0].as_http().is_some());
    }

    #[test]
    fn http_url_and_headers_are_expanded() {
        let servers = parse_with(
            Path::new("/repo/.mcp.json"),
            r#"{"mcpServers":{"api":{"type":"http","url":"https://${HOST}/mcp","headers":{"authorization":"Bearer ${API_TOKEN}"}}}}"#,
            &|name| match name {
                "HOST" => Some("example.com".to_string()),
                "API_TOKEN" => Some("secret".to_string()),
                _ => None,
            },
        )
        .expect("both are set");

        let config = servers[0].as_http().expect("http");
        assert_eq!(config.url, "https://example.com/mcp");
        assert_eq!(
            config
                .headers
                .get("authorization")
                .map(mentra::mcp::SecretString::expose_secret),
            Some("Bearer secret")
        );
    }

    #[test]
    fn a_remote_servers_debug_does_not_print_an_expanded_credential() {
        // The parser has already turned `${API_TOKEN}` into the real value by
        // the time this type exists, and `McpConfig` derives Debug around it.
        // The query string matters as much as the headers: mentra types only
        // the headers as `SecretString`, so the URL is basis's to redact.
        let servers = parse_with(
            Path::new("/repo/.mcp.json"),
            r#"{"mcpServers":{
                "api":{"type":"http","url":"https://x/mcp?key=${API_TOKEN}","headers":{"authorization":"${API_TOKEN}"}},
                "obs":{"type":"sse","url":"https://x/sse?key=${API_TOKEN}","headers":{"authorization":"${API_TOKEN}"}}
            }}"#,
            &|name| (name == "API_TOKEN").then(|| "sk-live-do-not-print-me".to_string()),
        )
        .expect("the token is set");

        for server in &servers {
            let rendered = format!("{server:?}");
            assert!(
                !rendered.contains("sk-live-do-not-print-me"),
                "an expanded credential reached Debug: {rendered}"
            );
            assert!(
                rendered.contains("https://x"),
                "the redaction must not hide where the server is: {rendered}"
            );
        }
    }

    #[test]
    fn an_unknown_transport_is_an_error() {
        let error = parse_text(r#"{"mcpServers":{"x":{"type":"carrier-pigeon","url":"u"}}}"#)
            .expect_err("unknown transports are errors");

        assert!(matches!(error, McpError::Invalid { .. }), "{error}");
    }

    #[test]
    fn an_entry_with_neither_command_nor_url_is_an_error() {
        let error = parse_text(r#"{"mcpServers":{"x":{"args":["-y"]}}}"#)
            .expect_err("nothing to connect to");

        assert!(matches!(error, McpError::Invalid { .. }), "{error}");
    }

    #[test]
    fn an_entry_with_both_command_and_url_is_an_error() {
        let error = parse_text(r#"{"mcpServers":{"x":{"command":"srv","url":"https://x"}}}"#)
            .expect_err("ambiguous rather than guessed at");

        assert!(matches!(error, McpError::Invalid { .. }), "{error}");
    }

    #[test]
    fn an_empty_command_is_an_error() {
        let error = parse_text(r#"{"mcpServers":{"x":{"type":"stdio","command":"  "}}}"#)
            .expect_err("nothing to spawn");

        assert!(matches!(error, McpError::Invalid { .. }), "{error}");
    }

    #[test]
    fn an_empty_name_is_an_error() {
        let error = parse_text(r#"{"mcpServers":{"":{"command":"srv"}}}"#)
            .expect_err("the name namespaces the tools");

        assert!(matches!(error, McpError::Invalid { .. }), "{error}");
    }

    #[test]
    fn environment_placeholders_are_expanded() {
        let servers = parse_with(
            Path::new("/repo/.mcp.json"),
            r#"{"mcpServers":{"gh":{"command":"srv","args":["--org","${ORG}"],"env":{"TOKEN":"${GH_TOKEN}"}}}}"#,
            &|name| match name {
                "ORG" => Some("oops-rs".to_string()),
                "GH_TOKEN" => Some("secret".to_string()),
                _ => None,
            },
        )
        .expect("both are set");

        let config = servers[0].as_stdio().expect("stdio");
        assert_eq!(config.args, vec!["--org", "oops-rs"]);
        assert_eq!(config.env.get("TOKEN").map(String::as_str), Some("secret"));
    }

    #[test]
    fn an_unset_placeholder_names_the_server_and_the_field() {
        let error =
            parse_text(r#"{"mcpServers":{"gh":{"command":"srv","env":{"T":"${GH_TOKEN}"}}}}"#)
                .expect_err("an unset variable is an error");

        let rendered = error.to_string();
        assert!(rendered.contains("gh"), "{rendered}");
        assert!(rendered.contains("env.T"), "{rendered}");
        assert!(rendered.contains("GH_TOKEN"), "{rendered}");
    }

    #[test]
    fn no_error_repeats_a_value_from_the_file() {
        // These files are gitignored because `env` and `headers` hold
        // credentials, and these messages travel to clients and logs.
        const SECRET: &str = "sk-live-do-not-print-me";

        let broken = [
            format!(r#"{{"mcpServers":{{"a":{{"command":"srv","env":{{"T":"{SECRET}${{"}}}}}}}}"#),
            format!(r#"{{"mcpServers":{{"b":{{"command":"srv","args":["{SECRET}${{NOPE}}"]}}}}}}"#),
            format!(
                r#"{{"mcpServers":{{"c":{{"url":"https://x/sse","headers":{{"authorization":"{SECRET}${{}}"}}}}}}}}"#
            ),
            // serde's own message would quote this one.
            format!(r#"{{"mcpServers":{{"d":{{"command":"srv","env":"{SECRET}"}}}}}}"#),
        ];

        for text in broken {
            let error = parse_text(&text).expect_err("each of these fails");

            assert!(
                !error.to_string().contains(SECRET),
                "a value leaked into: {error}"
            );
        }
    }

    #[test]
    fn servers_come_back_in_a_stable_order() {
        let servers = parse_text(
            r#"{"mcpServers":{"zeta":{"command":"z"},"alpha":{"command":"a"},"mid":{"command":"m"}}}"#,
        )
        .expect("a well-formed file");

        let names: Vec<&str> = servers.iter().map(McpServer::name).collect();
        assert_eq!(
            names,
            vec!["alpha", "mid", "zeta"],
            "registration order must not depend on hashing"
        );
    }
}
