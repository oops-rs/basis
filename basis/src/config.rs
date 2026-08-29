//! `.basis/config.json` — what a repository says about which model runs in it.
//!
//! Every other knob basis has is a flag or an environment variable, and both
//! describe an *invocation*. Neither can say the one thing a repository knows
//! about itself: that its work is done with a particular model. Without
//! `--model` basis resolves [`ModelSelector::NewestAvailable`] by asking the
//! provider what it has and taking the newest, so the same prompt in the same
//! repository can run a different model tomorrow than it ran today — silently,
//! and for a reason nobody in the repository chose.
//!
//! That fact is repository-scoped, which is a scope basis already has
//! machinery for. `.mcp.json`, `.basis/tools.json` and `.basis/hooks.json` all
//! layer a workspace file over a file in the user's config directory, take the
//! most specific answer, expand `${VAR}`, and refuse a file they cannot read
//! rather than ignoring it. This is the same convention with a different
//! payload, and it is a seam: a host reads a [`Config`] and applies it, or
//! does not.
//!
//! It is deliberately **not** a model catalogue. No prices, no context
//! windows, no capability flags — a model's properties are the provider's to
//! state and would be stale in a file the day after they changed. What is here
//! is a choice, and only a choice.
//!
//! # What may be said, and by whom
//!
//! `provider`, `model` and `effort` may be said by either file. `base_url` may
//! be said **only by the global file**, and a workspace file that sets it is
//! refused by name rather than ignored.
//!
//! The asymmetry is not squeamishness. `.mcp.json` and `.basis/hooks.json`
//! name programs to run, and a program is bounded by whatever confines the
//! process it starts in — the OS's business, per ADR-0013, and a boundary the
//! operator can see and reason about. A `base_url` is a different class of
//! exposure: it redirects the traffic that carries the credential basis just
//! read out of the environment, to a host of the file's choosing. A leaked
//! secret is bounded by nothing, and a repository is a thing people clone
//! before they read.
//!
//! `provider` in a workspace file is safe by the same reasoning: it selects
//! which *preset* endpoint and which environment variable, and both of those
//! are the user's own. The worst a hostile one can do is pick a service the
//! user already holds a key for, and fail when they do not.
//!
//! There is **no `api_key` key at all**, for the reason
//! [`WorkspaceBuilder`](crate::WorkspaceBuilder) has no field for one: a
//! credential belongs to the environment the process runs in, not to a file
//! describing what to run.
//!
//! # Where each answer lands
//!
//! Reading the file settles nothing on its own — [`Config`] is a report, and
//! the layer that applies it decides what it may reach. `provider`, `model`
//! and `base_url` describe the *process's* connection to a provider, which is
//! ADR-0018's runtime scope
//! ([`RuntimeBuilder::with_config`](crate::RuntimeBuilder::with_config));
//! `model` is also a workspace override, and `effort` is a per-run default
//! that [`Workspace`](crate::Workspace) applies when a
//! [`RunSpec`](crate::workspace::RunSpec) asked for none.
//!
//! Precedence, strongest first: **an explicit builder call or CLI flag, the
//! workspace file, the global file, the environment, basis's default.** A file
//! outranks the environment because a variable in a shell describes whoever
//! started the process, and the file describes the repository the work is in.

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use mentra::{BuiltinProvider, ModelSelector};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    context::{ContextConfig, ContextScope},
    event::ContextFile,
    expand::expand,
    provider,
    run::Effort,
};

/// Where basis looks inside a workspace, relative to its root.
pub const DEFAULT_WORKSPACE_CONFIG_FILE: &str = ".basis/config.json";

/// Where basis looks inside the global config directory. Not the dotted name,
/// for `crate::mcp`'s reason: a hidden file inside a directory that exists to
/// hold configuration would be hiding it from the person who put it there.
pub const DEFAULT_GLOBAL_CONFIG_FILE: &str = "config.json";

/// The schema version this basis understands.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// One resolved value, with the file that decided it.
///
/// The provenance is not decoration. Two files can set the same key, the
/// environment can set two of them, and a flag beats all of it — so "which
/// model is this" and "who said so" are different questions, and a caller
/// reporting its own configuration needs the second one answered without
/// re-reading the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting<T> {
    pub value: T,
    /// The file this came from.
    pub path: PathBuf,
    /// [`ContextScope::Workspace`] or [`ContextScope::Global`]; there is no
    /// parent walk, for `crate::mcp`'s reason.
    pub scope: ContextScope,
}

impl<T> Setting<T> {
    fn new(value: T, path: &Path, scope: ContextScope) -> Self {
        Self {
            value,
            path: path.to_path_buf(),
            scope,
        }
    }
}

/// What the discovered files say, layered.
///
/// Every field is optional because every key is: a file that sets `model` and
/// nothing else has said one thing, and basis must still answer the rest the
/// way it always did. An empty `Config` — [`Default`] — is exactly "no file
/// said anything", which is also how a host turns discovery off
/// ([`WorkspaceBuilder::with_config`](crate::WorkspaceBuilder::with_config)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Which provider preset to use, and therefore which credential variable.
    pub provider: Option<Setting<BuiltinProvider>>,
    /// The model id, as [`ModelSelector::Id`].
    pub model: Option<Setting<String>>,
    /// The reasoning effort a run gets when it asked for none.
    pub effort: Option<Setting<Effort>>,
    /// An OpenAI-compatible endpoint. Global file only; see the module docs.
    pub base_url: Option<Setting<String>>,
    /// The files that took effect, most specific first — for a caller that
    /// reports which conventions it read.
    pub files: Vec<ContextFile>,
}

impl Config {
    /// Every config file that exists, layered field by field with the
    /// workspace's own winning.
    ///
    /// A missing file is not an error — most workspaces say nothing. A file
    /// that exists and cannot be read or understood is, because the operator
    /// wrote it meaning something: `.mcp.json`'s rule, and for the same
    /// reason. A repository that pinned a model and then misspelled the key
    /// should not run a different model in silence.
    ///
    /// `global_dir` is the directory [`ContextConfig::global_dir`] resolves —
    /// `$BASIS_CONFIG_DIR`, else `$XDG_CONFIG_HOME/basis`, else
    /// `$HOME/.config/basis`. Passed in rather than resolved here so one
    /// process cannot read two different global directories.
    pub fn discover(workspace: &Path, global_dir: Option<&Path>) -> Result<Self, ConfigError> {
        Self::discover_with(workspace, global_dir, &|name| std::env::var(name).ok())
    }

    /// [`discover`](Self::discover) against the process's environment and the
    /// default global directory.
    pub fn discover_default(workspace: &Path) -> Result<Self, ConfigError> {
        Self::discover(workspace, ContextConfig::default().global_dir.as_deref())
    }

    /// The same, against an explicit environment, so `${VAR}` expansion is
    /// testable without mutating the process's own — [`crate::provider`]'s
    /// rule, for [`crate::provider`]'s reason.
    pub(crate) fn discover_with(
        workspace: &Path,
        global_dir: Option<&Path>,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let mut sources = Vec::new();

        let workspace_file = workspace.join(DEFAULT_WORKSPACE_CONFIG_FILE);
        if workspace_file.is_file() {
            sources.push(read(workspace_file, ContextScope::Workspace, lookup)?);
        }

        if let Some(global) = global_dir {
            let global_file = global.join(DEFAULT_GLOBAL_CONFIG_FILE);
            // The same file reached twice is one source, not two: layering it
            // against itself would say nothing and report two files.
            if global_file.is_file()
                && !sources
                    .iter()
                    .any(|(path, _)| crate::paths::same_dir(path, &global_file))
            {
                sources.push(read(global_file, ContextScope::Global, lookup)?);
            }
        }

        Ok(layer(sources))
    }

    /// Whether any file said anything at all.
    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && self.effort.is_none()
            && self.base_url.is_none()
    }

    /// The model as a selector, for the two builders that take one.
    pub fn model_selector(&self) -> Option<ModelSelector> {
        self.model
            .as_ref()
            .map(|model| ModelSelector::Id(model.value.clone()))
    }
}

/// Why a config file could not be used.
///
/// These messages travel — into `basis spawn --json`, into an ACP client's
/// error pane — and they follow
#[cfg_attr(feature = "mcp", doc = "[`McpError`](crate::mcp::McpError)'s rule")]
#[cfg_attr(not(feature = "mcp"), doc = "`McpError`'s rule")]
/// even though nothing here is meant to hold a credential: `${VAR}` expansion
/// resolves real values before validation, so an error names the file, the
/// key, and an environment variable, and nothing else it read. That is also
/// why [`Parse`](Self::Parse) carries a location and a category instead of
/// serde's own message, which quotes the value it choked on.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not a valid basis config: {problem} at line {line}, column {column}")]
    Parse {
        path: PathBuf,
        problem: &'static str,
        line: usize,
        column: usize,
    },

    #[error("{path} declares no `schema`; this basis understands {CONFIG_SCHEMA_VERSION}")]
    NoSchema { path: PathBuf },

    #[error(
        "{path} declares config schema {schema}, but this basis understands {CONFIG_SCHEMA_VERSION}"
    )]
    UnsupportedSchema { path: PathBuf, schema: u32 },

    #[error("{path}: `{key}` {reason}")]
    Invalid {
        path: PathBuf,
        key: &'static str,
        reason: String,
    },

    /// The refusal the module docs argue for: a committed file may not
    /// redirect the traffic that carries the user's credential.
    #[error(
        "{path} sets `base_url`, which basis honors only from your own \
         {DEFAULT_GLOBAL_CONFIG_FILE}: a file a repository ships must not be able to point the \
         model's traffic — and the API key on it — at a host you did not choose"
    )]
    WorkspaceBaseUrl { path: PathBuf },
}

/// The whole file.
///
/// `deny_unknown_fields`, unlike `.mcp.json`'s reader and like
/// `.basis/tools.json`'s: this format is basis's own, so an unknown key is a
/// typo — and a silently ignored `modle` is a repository that believes it
/// pinned a model and did not.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    /// Optional so its absence can be named, exactly as `.basis/tools.json`
    /// does. A file written against a schema nobody stated is a file basis is
    /// guessing about.
    schema: Option<u32>,
    provider: Option<String>,
    model: Option<String>,
    effort: Option<EffortName>,
    base_url: Option<String>,
}

/// The five spellings `--effort` accepts, so the file and the flag cannot come
/// to mean different words.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EffortName {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl From<EffortName> for Effort {
    fn from(effort: EffortName) -> Self {
        match effort {
            EffortName::Low => Self::Low,
            EffortName::Medium => Self::Medium,
            EffortName::High => Self::High,
            EffortName::XHigh => Self::XHigh,
            EffortName::Max => Self::Max,
        }
    }
}

/// One file's answers, before layering.
struct Read {
    provider: Option<BuiltinProvider>,
    model: Option<String>,
    effort: Option<Effort>,
    base_url: Option<String>,
    scope: ContextScope,
}

fn read(
    path: PathBuf,
    scope: ContextScope,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(PathBuf, Read), ConfigError> {
    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let parsed = parse(&path, &text, scope.clone(), lookup)?;

    Ok((path, parsed))
}

fn parse(
    path: &Path,
    text: &str,
    scope: ContextScope,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Read, ConfigError> {
    let file: ConfigFile = serde_json::from_str(text).map_err(|source| ConfigError::Parse {
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
            return Err(ConfigError::NoSchema {
                path: path.to_path_buf(),
            });
        }
        Some(schema) if schema != CONFIG_SCHEMA_VERSION => {
            return Err(ConfigError::UnsupportedSchema {
                path: path.to_path_buf(),
                schema,
            });
        }
        Some(_) => {}
    }

    // Before anything is expanded or parsed: what this refuses is the file
    // existing at all with that key in it, and an unset `${VAR}` inside the
    // value would otherwise answer first with the wrong complaint.
    if file.base_url.is_some() && scope == ContextScope::Workspace {
        return Err(ConfigError::WorkspaceBaseUrl {
            path: path.to_path_buf(),
        });
    }

    // Expansion failures name the key rather than quoting it — the same line
    // `.mcp.json` and `.basis/tools.json` draw, because the same `${VAR}`
    // machinery resolves the same kind of value.
    let expanded = |key: &'static str, raw: &str| -> Result<String, ConfigError> {
        expand(raw, lookup).map_err(|reason| ConfigError::Invalid {
            path: path.to_path_buf(),
            key,
            reason,
        })
    };

    let provider = file
        .provider
        .as_deref()
        .map(|name| {
            let name = expanded("provider", name)?;
            provider::parse(&name).map_err(|error| ConfigError::Invalid {
                path: path.to_path_buf(),
                key: "provider",
                reason: error.to_string(),
            })
        })
        .transpose()?;

    let model = file
        .model
        .as_deref()
        .map(|model| expanded("model", model))
        .transpose()?
        .map(|model| model.trim().to_string())
        // An empty id would reach the provider as a request for a model called
        // nothing; saying so here names the file instead.
        .map(|model| {
            if model.is_empty() {
                Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    key: "model",
                    reason: "is empty; remove the key to take the provider's newest".to_string(),
                })
            } else {
                Ok(model)
            }
        })
        .transpose()?;

    let base_url = file
        .base_url
        .as_deref()
        .map(|url| {
            let url = expanded("base_url", url)?;
            // Normalized here rather than at resolution so a typo names this
            // file, which is the only place it can be fixed.
            provider::normalize_base_url(&url).map_err(|error| ConfigError::Invalid {
                path: path.to_path_buf(),
                key: "base_url",
                reason: error.to_string(),
            })
        })
        .transpose()?;

    Ok(Read {
        provider,
        model,
        effort: file.effort.map(Effort::from),
        base_url,
        scope,
    })
}

/// Keeps the first answer to each key, and reports every file that was read.
///
/// Sources arrive most specific first, so "first wins" is "the workspace's own
/// answer shadows the personal one" — [`crate::mcp`]'s rule. Layering is per
/// key rather than per file: a workspace that pins a model has not thereby
/// unsaid the effort its owner prefers everywhere.
fn layer(sources: Vec<(PathBuf, Read)>) -> Config {
    let mut config = Config::default();

    for (path, read) in sources {
        config.files.push(ContextFile {
            path: path.clone(),
            scope: read.scope.label(),
        });

        if config.provider.is_none()
            && let Some(provider) = read.provider
        {
            config.provider = Some(Setting::new(provider, &path, read.scope.clone()));
        }
        if config.model.is_none()
            && let Some(model) = read.model
        {
            config.model = Some(Setting::new(model, &path, read.scope.clone()));
        }
        if config.effort.is_none()
            && let Some(effort) = read.effort
        {
            config.effort = Some(Setting::new(effort, &path, read.scope.clone()));
        }
        if config.base_url.is_none()
            && let Some(base_url) = read.base_url
        {
            config.base_url = Some(Setting::new(base_url, &path, read.scope));
        }
    }

    config
}
