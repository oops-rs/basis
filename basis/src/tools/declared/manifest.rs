//! Declaring a tool: what `.basis/tools.json` may say, and where basis looks
//! for it.
//!
//! The native binding needs none of this — a host holds an `ExecutableTool` as
//! a value, so there is nothing to discover and nothing to parse. Everything
//! here exists because this binding's tools are named in a file rather than in
//! code, and because a file that names a program to run has to be read with the
//! care [`DeclaredToolError`] describes.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use mentra::tool::ToolSideEffectLevel;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::{context::ContextScope, expand::expand};

/// Where basis looks inside a workspace, relative to its root.
pub const DEFAULT_WORKSPACE_TOOLS_FILE: &str = ".basis/tools.json";

/// Where basis looks inside the global config directory. Not the dotted name,
/// for `crate::mcp`'s reason: a hidden file inside a directory that exists to
/// hold configuration would be hiding it from the person who put it there.
pub const DEFAULT_GLOBAL_TOOLS_FILE: &str = "tools.json";

/// The manifest schema this basis understands.
pub const TOOLS_SCHEMA_VERSION: u32 = 1;

/// How long a declared tool gets before it is killed.
///
/// The runtime's own patience for a shell command, deliberately: a declared
/// tool is the same kind of work — somebody's program, doing the job the turn
/// asked for — and not a check on the critical path of every call, which is
/// why this is minutes where [`DEFAULT_HOOK_TIMEOUT`](crate::hooks::DEFAULT_HOOK_TIMEOUT)
/// is seconds. A tool that needs longer says so per tool.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest name a provider will carry. Every one of them rejects a longer
/// tool name, and the rejection arrives as an opaque request failure at the
/// first turn rather than as anything naming this file.
const MAX_NAME_LENGTH: usize = 64;

/// The prefix mentra bridges MCP tools under, which nothing else may claim.
const MCP_PREFIX: &str = "mcp__";

/// How much of the world a declared tool may touch.
///
/// **There is no read-only variant, and that is the point.** A declared tool
/// runs a program, so the mildest thing it can truthfully say about itself is
/// [`Process`](Self::Process) — and basis's approval gate lets a
/// [`None`](ToolSideEffectLevel::None) call through without asking
/// ([`is_consequential`](crate::approval::is_consequential)), because prompting
/// for reads trains people to approve without reading. A manifest able to spell
/// "read-only" would therefore be a manifest able to route a subprocess past
/// the approver by writing one word, which is not a thing a file a repository
/// ships should be able to do.
///
/// So the enum offers the two truthful answers and defaults to the milder of
/// them: a declaration that says nothing still reaches the approver.
/// [`External`](Self::External) is the honest word for a tool that leaves the
/// machine — the Jenkins trigger, the ticket update — and an approver, or a
/// host's [`Approver`](crate::approval::Approver) impl, can tell those apart
/// from a local filter without reading the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// Runs a program on this machine. The floor, and the default.
    #[default]
    Process,
    /// Reaches something outside this machine.
    External,
}

impl SideEffect {
    /// What mentra's descriptor and preview carry.
    pub const fn level(self) -> ToolSideEffectLevel {
        match self {
            Self::Process => ToolSideEffectLevel::Process,
            Self::External => ToolSideEffectLevel::External,
        }
    }
}

/// One declared tool, validated and with its `${VAR}` placeholders resolved.
#[derive(Clone, PartialEq, Eq)]
pub struct DeclaredToolSpec {
    /// What the model calls, an operator writes in a rule, and a
    /// `.basis/hooks.json` entry matches on. The manifest's key.
    pub name: String,
    /// What the model reads to learn the tool.
    pub description: String,
    /// The JSON schema the model fills in, and the object basis writes to the
    /// program's stdin.
    pub input_schema: Value,
    /// Program and arguments, exec'd directly. Never passed to a shell.
    pub command: Vec<String>,
    /// Where the program runs, relative to the workspace root. Absent means the
    /// root itself.
    pub cwd: Option<PathBuf>,
    /// Added to the baseline the program is handed, on top of the runtime's
    /// fixed command environment and overriding it for any name they share —
    /// this is the tool's own statement, and the more specific one holds (see
    /// the module docs' three layers). Sorted, so a spec is comparable; never
    /// printed, so a credential cannot reach a log.
    pub env: Vec<(String, String)>,
    /// Absent means [`DEFAULT_TOOL_TIMEOUT`].
    pub timeout_ms: Option<u64>,
    pub side_effect: SideEffect,
}

/// Hand-written for the reason `McpServer`'s is: by the time a
/// declaration reaches this type `${JENKINS_TOKEN}` has been expanded into the
/// real value, and a derived impl would put it in every `{:?}` of anything
/// holding one. Variable *names* survive, because naming `env.JENKINS_TOKEN` is
/// what makes a misconfiguration fixable and repeats nothing that was read.
impl std::fmt::Debug for DeclaredToolSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclaredToolSpec")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field(
                "env",
                &crate::redaction::redacted_env(self.env.iter().map(|(key, _)| key)),
            )
            .field("timeout_ms", &self.timeout_ms)
            .field("side_effect", &self.side_effect)
            .finish_non_exhaustive()
    }
}

impl DeclaredToolSpec {
    /// How long this tool gets before it is killed.
    pub fn timeout(&self) -> Duration {
        self.timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TOOL_TIMEOUT)
    }

    /// Where the program runs, resolved against the workspace root. An absolute
    /// `cwd` is left as it is, which is what `Path::join` already does.
    pub fn working_directory(&self, workspace: &Path) -> PathBuf {
        self.cwd
            .as_ref()
            .map_or_else(|| workspace.to_path_buf(), |cwd| workspace.join(cwd))
    }
}

/// Where to look for declared tools.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolsConfig {
    /// Path relative to the workspace root. Empty names no file to look for;
    /// [`ToolsConfig::supplied_only`] writes it.
    pub workspace_file: PathBuf,
    /// The global config directory, if any. `tools.json` inside it is used.
    pub global_dir: Option<PathBuf>,
    /// Typed, final declarations supplied by the embedding host. They outrank
    /// file declarations of the same name and are never environment-expanded.
    pub supplied: Vec<DeclaredToolSpec>,
}

impl std::fmt::Debug for ToolsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolsConfig")
            .field("workspace_file", &self.workspace_file)
            .field("global_dir", &self.global_dir)
            .field("supplied", &format_args!("{} tools", self.supplied.len()))
            .finish()
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            workspace_file: PathBuf::from(DEFAULT_WORKSPACE_TOOLS_FILE),
            global_dir: crate::context::default_global_dir(),
            supplied: Vec::new(),
        }
    }
}

impl ToolsConfig {
    /// The host's own declarations, and no file discovery at all: neither
    /// `.basis/tools.json` nor the global one is read.
    ///
    /// What `WorkspaceBuilder::without_discovery` leaves of this config.
    /// Supplied declarations survive and are still registered, because a tool
    /// the host handed basis is one it named rather than one basis found.
    pub fn supplied_only(self) -> Self {
        Self {
            workspace_file: PathBuf::new(),
            global_dir: None,
            ..self
        }
    }

    /// Replaces the declarations supplied directly by the embedding host.
    pub fn with_supplied(self, supplied: Vec<DeclaredToolSpec>) -> Self {
        Self { supplied, ..self }
    }
}

/// A manifest that exists on disk, and what it declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsSource {
    pub path: PathBuf,
    pub scope: ContextScope,
    pub tools: Vec<DeclaredToolSpec>,
}

/// Why a manifest could not be used, or a tool it declared could not be
/// registered.
///
/// Every one of these is an `Err` rather than a shorter tool list: a file that
/// exists and does not parse means the operator asked for a tool basis is not
/// offering, and starting the run anyway would leave the model missing a
/// capability its instructions assume — silently, and only at the point where
/// it needed it.
///
/// The messages travel, so they follow `McpError`'s rule: a
/// manifest's `env` holds credentials, and by the time one is read the
/// `${VAR}`s in it are resolved. An error may name the file, the tool, the
/// field and an environment variable, and nothing else it read. That is why
/// [`Parse`](Self::Parse) carries a location and a category instead of serde's
/// own message, which quotes the value it choked on — and it is why a
/// misspelled key, which `deny_unknown_fields` does catch, is reported as a
/// place in the file rather than as the word that was wrong.
#[derive(Debug, Error)]
pub enum DeclaredToolError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not a valid tool manifest: {problem} at line {line}, column {column}")]
    Parse {
        path: PathBuf,
        problem: &'static str,
        line: usize,
        column: usize,
    },

    #[error("{path} has no `tools` object")]
    NoTools { path: PathBuf },

    #[error("{path} declares no `schema`; this basis understands {TOOLS_SCHEMA_VERSION}")]
    NoSchema { path: PathBuf },

    #[error(
        "{path} declares tools schema {schema}, but this basis understands {TOOLS_SCHEMA_VERSION}"
    )]
    UnsupportedSchema { path: PathBuf, schema: u32 },

    #[error("{path}: tool `{name}` {reason}")]
    Invalid {
        path: PathBuf,
        name: String,
        reason: String,
    },

    #[error("supplied tool `{name}` {reason}")]
    InvalidSupplied { name: String, reason: String },

    #[error("{path}: tool `{name}` cannot be registered because {reason}")]
    NameTaken {
        path: PathBuf,
        name: String,
        reason: String,
    },

    #[error("supplied tool `{name}` cannot be registered because {reason}")]
    SuppliedNameTaken { name: String, reason: String },
}

/// Every manifest that exists, most specific first.
///
/// A missing file is not an error — most workspaces declare no tools. A file
/// that exists and cannot be read or understood is, because the operator wrote
/// it meaning something.
pub fn discover(
    workspace: &Path,
    config: &ToolsConfig,
) -> Result<Vec<ToolsSource>, DeclaredToolError> {
    let mut sources = Vec::new();

    if let Some(workspace_file) = crate::paths::candidate(workspace, &config.workspace_file)
        && workspace_file.is_file()
    {
        sources.push(read(workspace_file, ContextScope::Workspace)?);
    }

    if let Some(global) = &config.global_dir {
        let global_file = global.join(DEFAULT_GLOBAL_TOOLS_FILE);
        // The same file reached twice is one source, not two: its tools would
        // otherwise be layered against themselves for no purpose.
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

/// Every tool a workspace declares, after layering: supplied first, then the
/// workspace and global files, name-ordered within each manifest since its
/// entries are a JSON object and an object has no order of its own to preserve.
pub fn load(
    workspace: &Path,
    config: &ToolsConfig,
) -> Result<Vec<DeclaredToolSpec>, DeclaredToolError> {
    let supplied = load_supplied(config)?;
    Ok(layer(&supplied, &discover(workspace, config)?)
        .into_iter()
        .map(|(_, spec)| spec)
        .collect())
}

/// The supplied typed declarations only, with no file discovery.
pub(crate) fn load_supplied(
    config: &ToolsConfig,
) -> Result<Vec<DeclaredToolSpec>, DeclaredToolError> {
    for spec in &config.supplied {
        validate_spec(spec).map_err(|reason| DeclaredToolError::InvalidSupplied {
            name: spec.name.clone(),
            reason,
        })?;
    }
    Ok(config.supplied.clone())
}

/// Keeps the first declaration of each name, with the file it came from.
///
/// Sources arrive strongest first — supplied, workspace, global — so "first
/// wins" is the precedence rule. This is `crate::mcp`'s rule, for the same
/// reason: the name *is* the tool, so two declarations under one name are one
/// tool with a precedence question, never two tools.
pub(super) fn layer(
    supplied: &[DeclaredToolSpec],
    sources: &[ToolsSource],
) -> Vec<(Option<PathBuf>, DeclaredToolSpec)> {
    let mut kept: Vec<(Option<PathBuf>, DeclaredToolSpec)> = Vec::new();

    for spec in supplied {
        if !kept.iter().any(|(_, seen)| seen.name == spec.name) {
            kept.push((None, spec.clone()));
        }
    }

    for source in sources {
        for spec in &source.tools {
            if !kept.iter().any(|(_, seen)| seen.name == spec.name) {
                kept.push((Some(source.path.clone()), spec.clone()));
            }
        }
    }

    kept
}

fn read(path: PathBuf, scope: ContextScope) -> Result<ToolsSource, DeclaredToolError> {
    let text = std::fs::read_to_string(&path).map_err(|source| DeclaredToolError::Read {
        path: path.clone(),
        source,
    })?;

    let tools = parse(&path, &text, &|name| std::env::var(name).ok())?;

    Ok(ToolsSource { path, scope, tools })
}

/// The whole file.
///
/// `deny_unknown_fields`, unlike `.mcp.json`'s reader: that format is shared
/// with other agents and a key basis has no opinion about is theirs, while this
/// one is basis's own, so an unknown key is a typo — and a silently ignored
/// `timout_ms` is a tool running with a timeout nobody chose.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsFile {
    /// Optional so its absence can be named. A file written against a schema
    /// nobody stated is a file basis is guessing about.
    schema: Option<u32>,
    /// Optional for the same reason `mcpServers` is: a misspelled key would
    /// otherwise be a manifest whose tools quietly never appear.
    tools: Option<BTreeMap<String, RawTool>>,
}

/// One entry, before it is known to be a tool.
///
/// Every field is optional so that a missing one is reported by name. serde's
/// own "missing field" message would be dropped by [`DeclaredToolError::Parse`],
/// which keeps only a location — the right rule for a file that holds
/// credentials, and the reason the checks below are written out.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    description: Option<String>,
    input_schema: Option<Value>,
    command: Option<Vec<String>>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    side_effect: SideEffect,
}

fn parse(
    path: &Path,
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<DeclaredToolSpec>, DeclaredToolError> {
    let file: ToolsFile =
        serde_json::from_str(text).map_err(|source| DeclaredToolError::Parse {
            path: path.to_path_buf(),
            problem: match source.classify() {
                serde_json::error::Category::Syntax => "a syntax error",
                serde_json::error::Category::Data => "an unknown key or a value of the wrong type",
                serde_json::error::Category::Eof => "an unexpected end of input",
                serde_json::error::Category::Io => "a read error",
            },
            line: source.line(),
            column: source.column(),
        })?;

    match file.schema {
        None => {
            return Err(DeclaredToolError::NoSchema {
                path: path.to_path_buf(),
            });
        }
        Some(schema) if schema != TOOLS_SCHEMA_VERSION => {
            return Err(DeclaredToolError::UnsupportedSchema {
                path: path.to_path_buf(),
                schema,
            });
        }
        Some(_) => {}
    }

    let Some(entries) = file.tools else {
        return Err(DeclaredToolError::NoTools {
            path: path.to_path_buf(),
        });
    };

    entries
        .into_iter()
        .map(|(name, raw)| raw.into_spec(path, name, lookup))
        .collect()
}

impl RawTool {
    fn into_spec(
        self,
        path: &Path,
        name: String,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<DeclaredToolSpec, DeclaredToolError> {
        let invalid = |reason: String| DeclaredToolError::Invalid {
            path: path.to_path_buf(),
            name: name.clone(),
            reason,
        };

        // Expansion failures name the field rather than quoting it: `env` is
        // where credentials live, so the field's name is the most an error may
        // say about it — see [`DeclaredToolError`].
        let expanded = |field: &str, raw: &str| -> Result<String, DeclaredToolError> {
            expand(raw, lookup)
                .map_err(|reason| invalid(format!("has a `{field}` value that {reason}")))
        };

        check_name(&name).map_err(&invalid)?;

        let description = self.description.ok_or_else(|| {
            invalid(
                "has no `description`, which is the only thing telling the model what it is \
                     for"
                .to_string(),
            )
        })?;
        check_description(&description).map_err(&invalid)?;
        let description = description.trim().to_string();

        let input_schema = self
            .input_schema
            .ok_or_else(|| invalid("has no `input_schema`".to_string()))?;
        check_schema(&input_schema).map_err(&invalid)?;
        let command = self
            .command
            .ok_or_else(|| invalid("has no `command` to run".to_string()))?
            .iter()
            .enumerate()
            .map(|(index, argument)| expanded(&format!("command[{index}]"), argument))
            .collect::<Result<Vec<_>, _>>()?;
        check_command(&command).map_err(&invalid)?;
        check_timeout(self.timeout_ms).map_err(&invalid)?;

        Ok(DeclaredToolSpec {
            description,
            input_schema,
            command,
            cwd: self
                .cwd
                .as_deref()
                .map(|cwd| expanded("cwd", cwd))
                .transpose()?
                .map(PathBuf::from),
            env: self
                .env
                .iter()
                .map(|(key, value)| Ok((key.clone(), expanded(&format!("env.{key}"), value)?)))
                .collect::<Result<Vec<_>, DeclaredToolError>>()?,
            timeout_ms: self.timeout_ms,
            side_effect: self.side_effect,
            name,
        })
    }
}

fn validate_spec(spec: &DeclaredToolSpec) -> Result<(), String> {
    check_name(&spec.name)?;
    check_description(&spec.description)?;
    check_schema(&spec.input_schema)?;
    check_command(&spec.command)?;
    check_timeout(spec.timeout_ms)
}

fn check_description(description: &str) -> Result<(), String> {
    if description.trim().is_empty() {
        return Err(
            "has no `description`, which is the only thing telling the model what it is for"
                .to_string(),
        );
    }
    Ok(())
}

fn check_command(command: &[String]) -> Result<(), String> {
    let Some(program) = command.first() else {
        return Err("has no `command` to run".to_string());
    };
    if program.trim().is_empty() {
        return Err("names an empty program".to_string());
    }
    Ok(())
}

fn check_timeout(timeout_ms: Option<u64>) -> Result<(), String> {
    if timeout_ms == Some(0) {
        return Err(
            "has a `timeout_ms` of 0, which is a deadline that has already passed".to_string(),
        );
    }
    Ok(())
}

/// A name is the tool's identity everywhere it appears — the model's roster, a
/// remembered rule, a hook's `tools` list — so what may be one is checked here
/// rather than discovered at the first turn.
///
/// The charset is the one every provider accepts. `mcp__` is refused because
/// mentra parses that prefix to find a bridged tool's server, so a declaration
/// wearing it would be a workspace naming a server it does not own.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("has an empty name".to_string());
    }

    if name.len() > MAX_NAME_LENGTH {
        return Err(format!(
            "has a name of {} characters, and a provider takes at most {MAX_NAME_LENGTH}",
            name.len()
        ));
    }

    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        return Err(
            "has a name outside the letters, digits, `_` and `-` a provider accepts".to_string(),
        );
    }

    if name.starts_with(MCP_PREFIX) {
        return Err(format!(
            "has a name starting with `{MCP_PREFIX}`, which is how mentra names a bridged MCP \
             server's tools"
        ));
    }

    Ok(())
}

/// What a provider will accept as an input schema, checked here so a manifest
/// that cannot work says so at open rather than as an opaque request failure on
/// the first turn.
fn check_schema(schema: &Value) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        return Err("has an `input_schema` that is not a JSON object".to_string());
    };

    match object.get("type").and_then(Value::as_str) {
        None | Some("object") => Ok(()),
        Some(_) => Err(
            "has an `input_schema` whose `type` is not `object`, and a tool call's input always \
             is one"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests;
