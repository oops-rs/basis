//! Discovery of `.mcp.json` — the MCP servers a workspace wants connected.
//!
//! Tools reach the model three ways: mentra's builtins, skills, and MCP
//! servers. mentra owns the client half — spawning the process, the
//! `initialize` handshake, bridging every advertised tool into the runtime's
//! roster. basis owns the convention half: which file names a server, where that
//! file lives, and what its fields mean. Nothing here interprets a server.
//!
//! # The format is not basis's
//!
//! An `mcpServers` object in a repo-root `.mcp.json` is what the agents that
//! already read a project-local MCP file write, so basis reads that rather than
//! inventing a spelling for the same idea. The one place basis is stricter: a
//! file that exists but names no `mcpServers` key is an error, because the
//! alternative is that a typo disables every server and says nothing.
//!
//! # Three places a server can come from, and no fourth
//!
//! In precedence order — supplied by the host, then the workspace file, then
//! the global one. "Supplied" is an ACP client's `mcpServers` on
//! `session/new`, or a Rust host's own list; the client is the most specific
//! authority there is, because it is answering for this session in particular.
//!
//! There is deliberately **no parent walk**, which is where this module parts
//! company with [`context`](crate::context) and [`skills`](crate::skills).
//! Those walk from the workspace root outward, and for instructions that is
//! right: a monorepo's house rules should reach every crate inside it, and the
//! worst case of picking one up is prose the model did not need.
//!
//! `.mcp.json` is not prose. It names commands to spawn and credentials to
//! spawn them with — it is in basis's own `.gitignore` for that reason, and in
//! most projects' — so inheriting one from a directory the operator did not
//! point basis at means running a program they never chose, with a token they
//! never offered, because of where they happened to `cd`. Two roots the
//! operator names explicitly (this workspace, their own config) are the whole
//! set. A server in a parent directory is one `cd` away from being asked for
//! properly.
//!
//! Nothing read here is ever repeated back: see [`McpError`].
//!
//! Names are the identity. mentra namespaces every bridged tool by its
//! server's name, so two servers sharing a name would collide in the tool
//! roster; a more specific one therefore *shadows* a weaker one instead of
//! joining it.
//!
//! # Transports
//!
//! stdio, the legacy HTTP+SSE transport, and Streamable HTTP — the three
//! mentra has clients for. Streamable HTTP is the transport current MCP
//! servers ship; a server that answers `404` on a legacy `/sse` path wants
//! [`McpServer::Http`]. One deliberate asymmetry survives in `.mcp.json`: a
//! bare `url` with no `type` still means SSE, because files written before
//! the third transport existed keep their meaning.
//!
//! mentra's `allow_plaintext_credentials` override is deliberately not a
//! `.mcp.json` key. A committed file must not be able to grant its own
//! headers plaintext passage to a non-loopback host — the same line drawn at
//! `base_url` — so the refusal stays mentra's, checked at parse, and the
//! override stays with hosts that construct the config in code.

pub(crate) mod connections;
mod file;

use std::path::{Path, PathBuf};

use mentra::{McpServerConfig, McpSseServerConfig, McpStreamableHttpServerConfig};
use thiserror::Error;

use crate::context::ContextScope;

/// Where basis looks inside a workspace, relative to its root.
pub const DEFAULT_WORKSPACE_MCP_FILE: &str = ".mcp.json";

/// Where basis looks inside the global config directory. Not the dotted name:
/// a hidden file inside a directory that exists to hold configuration would be
/// hiding it from the person who put it there.
pub const DEFAULT_GLOBAL_MCP_FILE: &str = "mcp.json";

/// One MCP server, as basis hands it to mentra.
///
/// A thin sum over mentra's three transport configurations rather than a type
/// of basis's own: mentra owns what a server *is*, and re-describing it here would
/// only create something to drift. The enum exists because mentra's own
/// equivalent is private, so a caller holding a mixed list has nowhere to put
/// it (see the module docs on transports).
#[derive(Clone)]
#[non_exhaustive]
pub enum McpServer {
    /// A child process speaking JSON-RPC over its standard streams.
    Stdio(McpServerConfig),
    /// The legacy HTTP+SSE transport from protocol revision 2024-11-05.
    Sse(McpSseServerConfig),
    /// The Streamable HTTP transport, which current MCP servers ship.
    Http(McpStreamableHttpServerConfig),
}

/// Hand-written for the reason [`McpError`] reports so little: a stdio
/// server's `env` holds credentials, and by the time one reaches this type basis
/// has already expanded `${GITHUB_TOKEN}` into the real value. Deriving would
/// put those in every `{:?}` of an [`McpConfig`] or an [`McpSource`] — both
/// of which do derive, and either of which a host may log.
///
/// Variable *names* survive, because that is the same line the errors draw:
/// naming `env.GITHUB_TOKEN` is what makes a misconfiguration fixable, and it
/// repeats nothing that was read. The remote transports' headers need no help
/// — mentra types SSE and Streamable HTTP headers alike as `SecretString`,
/// which redacts itself — but their `url` is a plain `String`, and a query
/// string is where an expanded credential sits (`?key=…`), so both remote
/// arms print it through `redacted_url`.
impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio(config) => f
                .debug_struct("Stdio")
                .field("name", &config.name)
                .field("command", &config.command)
                .field("args", &config.args)
                .field("cwd", &config.cwd)
                .field(
                    "env",
                    &config
                        .env
                        .keys()
                        .map(|key| (key, "<redacted>"))
                        .collect::<std::collections::BTreeMap<_, _>>(),
                )
                .finish(),
            Self::Sse(config) => f
                .debug_struct("Sse")
                .field("name", &config.name)
                .field("url", &redacted_url(&config.url))
                .field("headers", &config.headers)
                .finish(),
            Self::Http(config) => f
                .debug_struct("Http")
                .field("name", &config.name)
                .field("url", &redacted_url(&config.url))
                .field("headers", &config.headers)
                .finish(),
        }
    }
}

/// A URL fit to print: scheme, host and path survive; userinfo and the query
/// string do not.
///
/// String surgery rather than a URL parser, deliberately: this must never
/// fail, because `Debug` is exactly where a malformed configuration goes to
/// be looked at, and a parse error here would trade one leak for another
/// (printing the raw string) or hide the field entirely.
fn redacted_url(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, _)) => (base, "?[redacted]"),
        None => (url, ""),
    };

    // Userinfo sits between the scheme separator and the first `@` of the
    // authority, which ends at the first `/` after `://`.
    let base = match base.split_once("://") {
        Some((scheme, rest)) => {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            match rest[..authority_end].rfind('@') {
                Some(at) => format!("{scheme}://[redacted]@{}", &rest[at + 1..]),
                None => base.to_string(),
            }
        }
        None => base.to_string(),
    };

    format!("{base}{query}")
}

impl McpServer {
    /// The server's name, which is also the namespace its tools land under.
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio(config) => &config.name,
            Self::Sse(config) => &config.name,
            Self::Http(config) => &config.name,
        }
    }

    /// The stdio configuration, for a caller that needs the concrete type.
    pub fn as_stdio(&self) -> Option<&McpServerConfig> {
        match self {
            Self::Stdio(config) => Some(config),
            Self::Sse(_) | Self::Http(_) => None,
        }
    }

    /// The SSE configuration, for a caller that needs the concrete type.
    pub fn as_sse(&self) -> Option<&McpSseServerConfig> {
        match self {
            Self::Sse(config) => Some(config),
            Self::Stdio(_) | Self::Http(_) => None,
        }
    }

    /// The Streamable HTTP configuration, for a caller that needs the concrete
    /// type.
    pub fn as_http(&self) -> Option<&McpStreamableHttpServerConfig> {
        match self {
            Self::Http(config) => Some(config),
            Self::Stdio(_) | Self::Sse(_) => None,
        }
    }

    /// Rejects a name mentra's own tool-namespacing cannot round-trip.
    ///
    /// mentra encodes every bridged tool as `mcp__{server}__{tool}` and
    /// recovers the split with `str::split_once("__")`, taking the *first*
    /// `__` as the boundary (mentra's `parse_mcp_tool_name`,
    /// `mcp/bridge.rs`). A server named e.g. `evil__foo` would put a second
    /// `__` ahead of that boundary: the encoded name
    /// `mcp__evil__foo__real_tool` parses back as server `evil`, tool
    /// `foo__real_tool` — indistinguishable from a server actually named
    /// `evil`. This crate's own foreign-tool check in `workspace.rs` trusts
    /// that same split, so a shared runtime would then attribute
    /// `evil__foo`'s tools to `evil`, and any two servers whose names differ
    /// only by where a `__` falls would collide in the roster.
    ///
    /// A name ending in a single `_` is the same hole in disguise: mentra
    /// joins it to the literal `__` in the format string, so server `evil_`
    /// with tool `_thing` encodes to `mcp__evil____thing` — byte-identical
    /// to server `evil` with tool `__thing`. Barring `__` inside the name
    /// and a trailing `_` together guarantee the first `__` in the encoded
    /// name is genuinely the separator mentra intended, which is what makes
    /// this the whole rule rather than half of it.
    ///
    /// This is the boundary check basis owns for names it hands to mentra,
    /// not a permanent stand-in for mentra validating its own encoding —
    /// see [oops-rs/mentra#29](https://github.com/oops-rs/mentra/issues/29).
    ///
    /// That issue closed in mentra 0.24: `McpManager::connect`, `connect_sse`
    /// and `connect_streamable_http` refuse the same shapes before opening a
    /// connection, as `McpServerNameError` surfaced through each transport's
    /// error (`mentra/src/mcp/bridge.rs:128, 161`). So this is no longer the
    /// only line — it is the *config-side early* one, and the reason to keep
    /// it is what it can say: a name read out of `.mcp.json` is refused with
    /// the file that named it and the entry that did, before any server is
    /// dialed, rather than as one connection's failure.
    ///
    /// mentra's rule has a third shape this one does not, the empty name. The
    /// `.mcp.json` loader (`mcp/file.rs`) rejects that separately, so an entry
    /// in a file keeps
    /// its file-naming refusal; a *host-supplied* server (`with_mcp`) with an
    /// empty name is the one case now caught at connect rather than at load.
    pub fn validate_name(name: &str) -> Result<(), &'static str> {
        if name.contains("__") {
            Err(
                "has a name containing `__`, which mentra's `mcp__{server}__{tool}` \
                 tool-name encoding uses as the separator between server and tool",
            )
        } else if name.ends_with('_') {
            Err(
                "has a name ending in `_`, which would join mentra's `__` separator \
                 into an earlier boundary — server `evil_` with tool `_thing` encodes \
                 to the same `mcp__evil____thing` as server `evil` with tool `__thing`",
            )
        } else {
            Ok(())
        }
    }
}

/// Which MCP servers a run gets: where to look for configured ones, and any
/// the host already has in hand.
#[derive(Debug, Clone)]
pub struct McpConfig {
    /// Path relative to the workspace root.
    pub workspace_file: PathBuf,
    /// The global config directory, if any. `mcp.json` inside it is used.
    pub global_dir: Option<PathBuf>,
    /// Servers the host supplies directly, outranking anything on disk of the
    /// same name. This is where an ACP client's `mcpServers` arrives.
    pub supplied: Vec<McpServer>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            workspace_file: PathBuf::from(DEFAULT_WORKSPACE_MCP_FILE),
            global_dir: crate::context::ContextConfig::default().global_dir,
            supplied: Vec::new(),
        }
    }
}

impl McpConfig {
    /// Adds servers the host already holds, replacing any set before.
    pub fn with_supplied(self, supplied: Vec<McpServer>) -> Self {
        Self { supplied, ..self }
    }
}

/// One `.mcp.json` that exists on disk, and what it configured.
#[derive(Debug, Clone)]
pub struct McpSource {
    pub path: PathBuf,
    pub scope: ContextScope,
    pub servers: Vec<McpServer>,
}

/// One server as configured, with what basis decided along the way kept
/// beside what it built.
///
/// `pub(crate)` plumbing rather than API: its one consumer is the connect
/// loop, which wants to say *why* a transport was chosen when the choice
/// then fails to connect.
pub(crate) struct ConfiguredServer {
    pub(crate) server: McpServer,
    /// True when no `type` named a transport and the bare-`url` rule chose
    /// SSE — the one inference worth diagnosing when the connection fails,
    /// because the server may simply speak Streamable HTTP.
    pub(crate) sse_inferred: bool,
}

/// A parsed file before the inference flags are stripped for the public
/// report.
struct ReadSource {
    path: PathBuf,
    scope: ContextScope,
    configured: Vec<ConfiguredServer>,
}

/// Anything that can go wrong turning configuration into servers.
///
/// These messages travel — into `basis spawn --json`, into an ACP client's error
/// pane, into whatever a host logs. A `.mcp.json` is gitignored in most
/// projects (basis's own included) because its `env` and `headers` hold
/// credentials, so a message may name the file, the server, the field, and an
/// environment variable, and nothing else it read.
///
/// The single deliberate exception is `type`, which is quoted back when it
/// names a transport basis does not know. It is a keyword slot — `stdio`,
/// `sse`, `http` — that cannot hold a credential, and repeating it is what
/// turns the error into a fix.
///
/// [`Parse`](Self::Parse) is why this is a rule rather than a habit: serde's
/// own message quotes the offending value (`invalid type: string "sk-live-…",
/// expected a map`), so basis reports the location and the kind of problem and
/// drops the message — including as a `source`, which `Debug` would print.
/// Line and column are what an operator needs anyway.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum McpError {
    #[error("failed to read MCP configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not valid JSON: {problem} at line {line}, column {column}")]
    Parse {
        path: PathBuf,
        problem: &'static str,
        line: usize,
        column: usize,
    },

    #[error("{path} has no `mcpServers` object")]
    NoServers { path: PathBuf },

    #[error("{origin}: MCP server `{name}` {reason}")]
    Invalid {
        origin: String,
        name: String,
        reason: String,
    },

    #[error("{origin}: an MCP server was configured over a transport basis does not recognize")]
    UnknownTransport { origin: String },
}

/// Every `.mcp.json` that exists, most specific first.
///
/// A missing file is not an error — most workspaces configure no servers. A
/// file that exists and cannot be read or understood is, because the operator
/// wrote it meaning something.
pub fn discover(workspace: &Path, config: &McpConfig) -> Result<Vec<McpSource>, McpError> {
    Ok(discovered(workspace, config)?
        .into_iter()
        .map(|source| McpSource {
            path: source.path,
            scope: source.scope,
            servers: source
                .configured
                .into_iter()
                .map(|configured| configured.server)
                .collect(),
        })
        .collect())
}

/// [`discover`], with the inference flags still attached.
fn discovered(workspace: &Path, config: &McpConfig) -> Result<Vec<ReadSource>, McpError> {
    let mut sources = Vec::new();

    let workspace_file = workspace.join(&config.workspace_file);
    if workspace_file.is_file() {
        sources.push(read(workspace_file, ContextScope::Workspace)?);
    }

    if let Some(global) = &config.global_dir {
        let global_file = global.join(DEFAULT_GLOBAL_MCP_FILE);
        // The same file reached twice is one source, not two: registering its
        // servers again would collide with itself in the tool roster.
        if global_file.is_file()
            && !sources
                .iter()
                .any(|source| crate::paths::same_dir(&source.path, &global_file))
        {
            sources.push(read(global_file, ContextScope::Global)?);
        }
    }

    Ok(sources)
}

/// Every server a run should connect, strongest source first.
///
/// This is what a caller almost always wants: [`discover`] reports where each
/// server came from, and this layers those reports into the one list a runtime
/// is built from.
pub fn servers(workspace: &Path, config: &McpConfig) -> Result<Vec<McpServer>, McpError> {
    Ok(configured(workspace, config)?
        .into_iter()
        .map(|configured| configured.server)
        .collect())
}

/// [`servers`], with the inference flags still attached — what the workspace
/// open hands the connect loop.
pub(crate) fn configured(
    workspace: &Path,
    config: &McpConfig,
) -> Result<Vec<ConfiguredServer>, McpError> {
    let discovered = discovered(workspace, config)?;

    // `.mcp.json` and an ACP client's `mcpServers` already check their own
    // entries at the door — see [`McpServer::validate_name`]. A Rust host's
    // own list, set through [`crate::workspace::WorkspaceBuilder::with_mcp`],
    // has no door of its own to check at: nothing stops it building an
    // [`McpServer`] directly. This is the one place every source has already
    // converged, so it is where that last gap gets closed.
    for server in &config.supplied {
        McpServer::validate_name(server.name()).map_err(|reason| McpError::Invalid {
            origin: "the supplied MCP server list".to_string(),
            name: server.name().to_string(),
            reason: reason.to_string(),
        })?;
    }

    Ok(layer(
        config
            .supplied
            .iter()
            .cloned()
            // A supplied server arrived in a typed variant; nothing about its
            // transport was inferred.
            .map(|server| ConfiguredServer {
                server,
                sse_inferred: false,
            })
            .chain(discovered.into_iter().flat_map(|source| source.configured)),
    ))
}

/// Keeps the first server seen under each name.
///
/// Callers pass strongest-first, so "first wins" is "most specific wins".
fn layer(servers: impl IntoIterator<Item = ConfiguredServer>) -> Vec<ConfiguredServer> {
    let mut kept: Vec<ConfiguredServer> = Vec::new();

    for candidate in servers {
        if !kept
            .iter()
            .any(|seen| seen.server.name() == candidate.server.name())
        {
            kept.push(candidate);
        }
    }

    kept
}

fn read(path: PathBuf, scope: ContextScope) -> Result<ReadSource, McpError> {
    let text = std::fs::read_to_string(&path).map_err(|source| McpError::Read {
        path: path.clone(),
        source,
    })?;

    let configured = file::parse(&path, &text)?;

    Ok(ReadSource {
        path,
        scope,
        configured,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn one_stdio(name: &str, command: &str) -> String {
        format!(r#"{{"mcpServers":{{"{name}":{{"command":"{command}"}}}}}}"#)
    }

    #[test]
    fn a_redacted_url_keeps_the_address_and_drops_the_secrets() {
        assert_eq!(
            redacted_url("https://user:pass@example.com/mcp?key=sk-live"),
            "https://[redacted]@example.com/mcp?[redacted]"
        );
        assert_eq!(
            redacted_url("https://example.com/mcp"),
            "https://example.com/mcp",
            "an innocent URL passes through recognizably"
        );
        assert_eq!(
            redacted_url("not a url?token=x"),
            "not a url?[redacted]",
            "surgery must survive what a parser would refuse"
        );
    }

    #[test]
    fn nothing_on_disk_means_no_servers() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let found = discover(tmp.path(), &config(None)).expect("no file is not an error");

        assert!(found.is_empty());
    }

    #[test]
    fn a_workspace_file_is_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
            &one_stdio("fs", "npx"),
        );

        let found = discover(tmp.path(), &config(None)).expect("discovery succeeds");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Workspace);
        assert_eq!(found[0].servers.len(), 1);
        assert_eq!(found[0].servers[0].name(), "fs");
    }

    #[test]
    fn the_workspace_file_outranks_the_global_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        write(
            &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
            &one_stdio("fs", "workspace-command"),
        );
        write(
            &global.join(DEFAULT_GLOBAL_MCP_FILE),
            &one_stdio("fs", "global-command"),
        );

        let found =
            discover(tmp.path(), &config(Some(global.clone()))).expect("discovery succeeds");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].scope, ContextScope::Workspace);
        assert_eq!(found[1].scope, ContextScope::Global);

        let layered = servers(tmp.path(), &config(Some(global))).expect("layering succeeds");
        assert_eq!(layered.len(), 1, "one name is one server");
        assert_eq!(
            layered[0].as_stdio().expect("stdio").command,
            "workspace-command"
        );
    }

    #[test]
    fn a_global_server_survives_alongside_a_workspace_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        write(
            &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
            &one_stdio("local", "a"),
        );
        write(
            &global.join(DEFAULT_GLOBAL_MCP_FILE),
            &one_stdio("shared", "b"),
        );

        let layered = servers(tmp.path(), &config(Some(global))).expect("layering succeeds");

        let names: Vec<&str> = layered.iter().map(McpServer::name).collect();
        assert_eq!(names, vec!["local", "shared"]);
    }

    #[test]
    fn a_supplied_server_outranks_both_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            &tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE),
            &one_stdio("fs", "from-the-file"),
        );

        let config = McpConfig {
            supplied: vec![McpServer::Stdio(McpServerConfig {
                name: "fs".to_string(),
                command: "from-the-client".to_string(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
            })],
            ..config(None)
        };

        let layered = servers(tmp.path(), &config).expect("layering succeeds");

        assert_eq!(layered.len(), 1);
        assert_eq!(
            layered[0].as_stdio().expect("stdio").command,
            "from-the-client",
            "the client is answering for this session in particular"
        );
    }

    #[test]
    fn a_supplied_server_with_a_double_underscore_name_is_rejected() {
        // Nothing stops a Rust host building an `McpServer` directly and
        // handing it to `WorkspaceBuilder::with_mcp` — this is the one
        // producer `.mcp.json` and an ACP client's `mcpServers` cannot check
        // at their own door, so `configured` is where the check lands for
        // this source instead. See [`McpServer::validate_name`].
        let tmp = tempfile::tempdir().expect("tempdir");

        let config = McpConfig {
            supplied: vec![McpServer::Stdio(McpServerConfig {
                name: "evil__foo".to_string(),
                command: "from-the-host".to_string(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
            })],
            ..config(None)
        };

        let error =
            servers(tmp.path(), &config).expect_err("mentra's split would misparse this name");

        assert!(matches!(error, McpError::Invalid { .. }), "{error}");
        assert!(error.to_string().contains("tool-name encoding"), "{error}");
    }

    #[test]
    fn a_supplied_server_with_a_trailing_underscore_name_is_rejected() {
        // `evil_` doesn't contain `__` itself, but a tool named `_thing`
        // joins onto it: `mcp__evil____thing` is byte-identical to server
        // `evil` with tool `__thing`. See [`McpServer::validate_name`].
        let tmp = tempfile::tempdir().expect("tempdir");

        let config = McpConfig {
            supplied: vec![McpServer::Stdio(McpServerConfig {
                name: "evil_".to_string(),
                command: "from-the-host".to_string(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
            })],
            ..config(None)
        };

        let error = servers(tmp.path(), &config)
            .expect_err("a trailing `_` can join a tool's leading `_` into a fake separator");

        assert!(matches!(error, McpError::Invalid { .. }), "{error}");
        assert!(error.to_string().contains("ending in `_`"), "{error}");
    }

    #[test]
    fn the_same_file_reached_twice_is_read_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        write(
            &global.join(DEFAULT_GLOBAL_MCP_FILE),
            &one_stdio("fs", "npx"),
        );

        // Point the workspace file at the very same place.
        let found = discover(
            &global,
            &McpConfig {
                workspace_file: PathBuf::from(DEFAULT_GLOBAL_MCP_FILE),
                global_dir: Some(global.clone()),
                supplied: Vec::new(),
            },
        )
        .expect("discovery succeeds");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Workspace);
    }

    #[test]
    fn a_directory_where_the_file_should_be_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE))
            .expect("create a directory with the file's name");

        let found = discover(tmp.path(), &config(None)).expect("discovery succeeds");

        assert!(found.is_empty());
    }

    #[test]
    fn a_malformed_file_is_an_error_not_a_silent_skip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(&tmp.path().join(DEFAULT_WORKSPACE_MCP_FILE), "{not json");

        let error = discover(tmp.path(), &config(None)).expect_err("malformed is an error");

        assert!(matches!(error, McpError::Parse { .. }), "{error}");
    }

    #[test]
    fn defaults_configure_no_servers_of_their_own() {
        let config = McpConfig::default();

        assert_eq!(config.workspace_file, PathBuf::from(".mcp.json"));
        assert!(config.supplied.is_empty());
    }

    #[test]
    fn supplying_servers_returns_a_new_config() {
        let base = McpConfig::default();
        let supplied = base
            .clone()
            .with_supplied(vec![McpServer::Sse(McpSseServerConfig::new(
                "obs",
                "https://example.com/sse",
            ))]);

        assert!(base.supplied.is_empty(), "the original is untouched");
        assert_eq!(supplied.supplied.len(), 1);
    }

    #[test]
    fn a_servers_environment_is_not_printed() {
        // By the time a server is one of these, `${GITHUB_TOKEN}` has been
        // expanded — so this is the real value, not the placeholder.
        let server = McpServer::Stdio(McpServerConfig {
            name: "gh".to_string(),
            command: "server".to_string(),
            args: vec!["--org".to_string(), "acme".to_string()],
            env: [("GITHUB_TOKEN".to_string(), "ghp-secret-value".to_string())]
                .into_iter()
                .collect(),
            cwd: None,
        });

        let printed = format!("{server:?}");

        assert!(!printed.contains("ghp-secret-value"));
        assert!(printed.contains("redacted"));
        assert!(
            printed.contains("GITHUB_TOKEN"),
            "the variable's name is what makes a misconfiguration fixable"
        );
        assert!(
            printed.contains("server") && printed.contains("acme"),
            "the command and its arguments are how a spawn is debugged"
        );
    }

    #[test]
    fn a_configured_server_is_not_printed_by_whatever_holds_it() {
        // `McpConfig` derives `Debug`, and other configs hold one — so the
        // redaction has to survive being nested rather than being something a
        // caller has to remember to reach for.
        let config = McpConfig::default().with_supplied(vec![McpServer::Stdio(McpServerConfig {
            name: "gh".to_string(),
            command: "server".to_string(),
            args: Vec::new(),
            env: [("GITHUB_TOKEN".to_string(), "ghp-secret-value".to_string())]
                .into_iter()
                .collect(),
            cwd: None,
        })]);

        assert!(!format!("{config:?}").contains("ghp-secret-value"));
    }
}
