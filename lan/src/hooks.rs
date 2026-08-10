//! Subprocess hooks: a say over tool calls, in any language.
//!
//! ARCHITECTURE.md §3 lists "event interception (block/modify tool calls)" as
//! something pi gets from in-process TypeScript extensions. lan's answer is a
//! process: a hook is an external command that receives one JSON object on
//! stdin and answers with one JSON object on stdout — allow, deny, or modify.
//! Process isolation is the point; no scripting runtime is embedded (ADR-0001,
//! and `docs/proposals/0001`, which stays deferred).
//!
//! # The seam
//!
//! mentra's [`PreExecutionHook`](mentra::runtime::PreExecutionHook) fires after
//! authorization and before the tool runs. lan registers exactly one
//! implementation, [`HookRunner`], which fans out to every configured command —
//! not because it must (`with_pre_hook` appends) but because lan wants to own
//! the ordering and the short-circuit.
//!
//! # Configuration
//!
//! `.lan/hooks.json` in the workspace, and `hooks.json` in the global config
//! directory. JSON rather than TOML because the wire contract is already JSON
//! and lan already parses it; `.lan/` because that is where lan's other
//! workspace data lives (`.lan/skills`).
//!
//! ```json
//! {
//!   "schema": 1,
//!   "hooks": [
//!     {
//!       "name": "no-force-push",
//!       "command": ["./.lan/hooks/no-force-push.sh"],
//!       "tools": ["shell"],
//!       "timeout_ms": 5000,
//!       "on_failure": "deny"
//!     }
//!   ]
//! }
//! ```
//!
//! `command` is an argv array, never a shell string: lan execs the program
//! directly, so nothing in a tool's input can be reinterpreted as shell syntax.
//! A relative program path is resolved against the workspace root; a bare name
//! is left to `PATH`, which is what a person writing the file expects. Omitting
//! `tools` means every tool; listing them matches on the exact tool name.
//!
//! Global hooks run before workspace ones. Any deny wins and short-circuits, so
//! ordering decides only which hook gets to speak — and putting the operator's
//! own hooks first means their deny can stop a repository-supplied hook from
//! being spawned at all.
//!
//! # The wire contract
//!
//! Versioned the way [`crate::event`] versions its stream: the request carries
//! [`HOOK_SCHEMA_VERSION`], so a hook reads the version before anything else
//! and can refuse a shape it does not understand. See [`wire`] for both halves.
//!
//! # When a hook breaks
//!
//! Every hook has a deadline ([`DEFAULT_HOOK_TIMEOUT`]) and is killed at it, so
//! a hanging hook costs a turn its budget rather than the turn. Past that,
//! a hook that cannot answer — killed, exited non-zero, printed nothing,
//! printed something that is not a decision, asked for a rewrite lan cannot
//! use, could not be started at all — **denies the call by default**, and says
//! so on stderr either way.
//!
//! That default is the one real judgement call in this module, and the
//! reasoning is on [`OnFailure`]. In short: a hook's power is over whether the
//! call happens, so a configured hook is by construction something whose
//! opinion the operator wanted, and the two ways of being wrong are not
//! symmetric — failing open on a broken guard silently removes a control
//! someone believes is in place, while failing closed on a broken observer is
//! loud and gets fixed. A hook that would rather be ignored says
//! `"on_failure": "allow"`.
//!
//! # A hook blocks the turn while it runs
//!
//! mentra's hook trait is synchronous but is called from inside an async turn,
//! so consulting a hook occupies the thread that reached it. On a multi-thread
//! tokio runtime — what the `lan` binary uses — `block_in_place` hands that
//! worker's other tasks away first, and the cost is nothing worse than one
//! worker being busy. On a **current-thread** runtime there is nowhere to hand
//! them to: an embedder that runs lan on one is stalled for as long as the hook
//! takes, bounded only by [`DEFAULT_HOOK_TIMEOUT`]. That matters most for ACP,
//! where ADR-0007 makes "the dispatch loop is never blocked" an invariant.
//!
//! Timeouts keep this bounded rather than unbounded, which is why it is a wart
//! and not a hazard. The real fix is an async trait method in mentra, tracked
//! as [oops-rs/mentra#16](https://github.com/oops-rs/mentra/issues/16) — which
//! also covers `PreExecutionContext` carrying no working directory of its own,
//! the reason the wire contract sends the workspace root instead.
//!
//! # A hook is code from the workspace
//!
//! `.lan/hooks.json` is workspace data, so cloning a repository and running lan
//! on it can execute commands that repository chose, before any tool call. That
//! is the same exposure as [`crate::shell`] and is bounded the same way: by
//! whatever confines the process (ADR-0004), not by a check in here.

mod exec;
mod runner;
pub mod wire;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::ContextScope;

pub use runner::{HookOutcome, HookRunner};
pub use wire::{HOOK_SCHEMA_VERSION, HookCall, HookRequest, HookResponse};

/// Where lan looks inside a workspace, relative to its root.
pub const DEFAULT_WORKSPACE_HOOKS_FILE: &str = ".lan/hooks.json";

/// Where lan looks inside the global config directory.
pub const DEFAULT_GLOBAL_HOOKS_FILE: &str = "hooks.json";

/// How long a hook gets to answer before it is killed.
///
/// A pre-execution hook sits on the critical path of every matching tool call,
/// so the budget is a check's worth of time, not a job's. A hook that needs
/// longer says so per hook.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// When a hook is consulted.
///
/// One variant, because mentra offers one interception point. It is spelled out
/// rather than assumed so a config file says when it fires, and so a second
/// point can arrive without changing the shape of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// After authorization, before the tool runs.
    #[default]
    PreToolUse,
}

/// What to do when a hook cannot answer.
///
/// The default is [`Self::Deny`]. A hook exists to have a say over whether a
/// call happens and in what form, so a configured hook is by construction
/// something whose opinion the operator wanted; when it cannot speak, nobody
/// knows what it would have said. The two failure modes are not symmetric:
/// allowing on a broken guard quietly removes a control the operator believes
/// is in place, while denying on a broken observer produces a loud failure that
/// gets fixed within the minute. Prefer the failure that announces itself.
///
/// [`Self::Allow`] is there because "observer" is a real use — a hook that logs
/// or notifies has no business stopping a turn when its network call times out.
/// It is a per-hook choice, and it is written down in the file rather than
/// inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    /// Treat a broken hook as a refusal.
    #[default]
    Deny,
    /// Treat a broken hook as no opinion, and carry on.
    Allow,
}

/// One configured hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    /// Names the hook in denials and failure reports. Not an identifier — two
    /// hooks may share a name; both still run.
    pub name: String,
    /// Program and arguments, exec'd directly. Never passed to a shell.
    pub command: Vec<String>,
    /// Tool names this hook is asked about. Absent means every tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub event: HookEvent,
    /// Absent means [`DEFAULT_HOOK_TIMEOUT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub on_failure: OnFailure,
}

impl HookSpec {
    /// A hook that runs `command` for every tool, on defaults.
    pub fn new(name: impl Into<String>, command: Vec<String>) -> Self {
        Self {
            name: name.into(),
            command,
            tools: None,
            event: HookEvent::default(),
            timeout_ms: None,
            on_failure: OnFailure::default(),
        }
    }

    pub fn with_tools(self, tools: Vec<String>) -> Self {
        Self {
            tools: Some(tools),
            ..self
        }
    }

    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self {
            timeout_ms: Some(timeout.as_millis() as u64),
            ..self
        }
    }

    pub fn with_on_failure(self, on_failure: OnFailure) -> Self {
        Self { on_failure, ..self }
    }

    pub fn timeout(&self) -> Duration {
        self.timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_HOOK_TIMEOUT)
    }

    /// Whether this hook has a say about `tool_name` at `event`.
    pub fn applies_to(&self, event: HookEvent, tool_name: &str) -> bool {
        self.event == event
            && self
                .tools
                .as_ref()
                .is_none_or(|tools| tools.iter().any(|name| name == tool_name))
    }
}

/// The on-disk shape of a hooks file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksFile {
    /// The config schema this file was written against.
    pub schema: u32,
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
}

/// Where to look for hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HooksConfig {
    /// Path relative to the workspace root.
    pub workspace_file: PathBuf,
    /// The global config directory, if any. `hooks.json` inside it is used.
    pub global_dir: Option<PathBuf>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            workspace_file: PathBuf::from(DEFAULT_WORKSPACE_HOOKS_FILE),
            global_dir: crate::context::ContextConfig::default().global_dir,
        }
    }
}

/// A hooks file that exists on disk, and what it contained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HooksSource {
    pub path: PathBuf,
    pub scope: ContextScope,
    pub hooks: Vec<HookSpec>,
}

/// Why a hooks file could not be used.
///
/// Every one of these is an `Err` rather than an empty hook list: a file that
/// exists and does not parse means the operator asked for something lan is not
/// doing, and starting the run anyway would be exactly the silent removal of a
/// control that [`OnFailure::Deny`] exists to prevent.
#[derive(Debug, Error)]
pub enum HookConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not a valid hooks file: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "{path} declares hooks schema {schema}, but this lan understands {HOOK_SCHEMA_VERSION}"
    )]
    UnsupportedSchema { path: PathBuf, schema: u32 },

    #[error("hook '{name}' in {path} has no command to run")]
    EmptyCommand { path: PathBuf, name: String },
}

/// Every hooks file that exists, in the order its hooks are consulted.
///
/// Global first: see the module docs — the operator's own hooks get first say,
/// and their deny stops a repository-supplied hook from being spawned.
pub fn discover(
    workspace: &Path,
    config: &HooksConfig,
) -> Result<Vec<HooksSource>, HookConfigError> {
    let mut sources = Vec::new();

    if let Some(global) = &config.global_dir {
        let path = global.join(DEFAULT_GLOBAL_HOOKS_FILE);
        if path.is_file() {
            sources.push(read_source(&path, ContextScope::Global)?);
        }
    }

    let workspace_path = workspace.join(&config.workspace_file);
    // A global directory pointed at the workspace would otherwise run every
    // hook twice, which for a guard means two denials for one call.
    if workspace_path.is_file()
        && !sources
            .iter()
            .any(|source| same_file(&source.path, &workspace_path))
    {
        sources.push(read_source(&workspace_path, ContextScope::Workspace)?);
    }

    Ok(sources)
}

/// Every configured hook, flattened into consultation order.
pub fn load(workspace: &Path, config: &HooksConfig) -> Result<Vec<HookSpec>, HookConfigError> {
    Ok(discover(workspace, config)?
        .into_iter()
        .flat_map(|source| source.hooks)
        .collect())
}

fn read_source(path: &Path, scope: ContextScope) -> Result<HooksSource, HookConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| HookConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    let file: HooksFile = serde_json::from_str(&text).map_err(|source| HookConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    if file.schema != HOOK_SCHEMA_VERSION {
        return Err(HookConfigError::UnsupportedSchema {
            path: path.to_path_buf(),
            schema: file.schema,
        });
    }

    if let Some(broken) = file.hooks.iter().find(|hook| hook.command.is_empty()) {
        return Err(HookConfigError::EmptyCommand {
            path: path.to_path_buf(),
            name: broken.name.clone(),
        });
    }

    Ok(HooksSource {
        path: path.to_path_buf(),
        scope,
        hooks: file.hooks,
    })
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(global: Option<PathBuf>) -> HooksConfig {
        HooksConfig {
            workspace_file: PathBuf::from(DEFAULT_WORKSPACE_HOOKS_FILE),
            global_dir: global,
        }
    }

    fn write_hooks(dir: &Path, relative: &str, body: &str) -> PathBuf {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dirs");
        std::fs::write(&path, body).expect("write hooks file");
        path
    }

    const ONE_HOOK: &str = r#"{
        "schema": 1,
        "hooks": [{"name": "guard", "command": ["/bin/true"]}]
    }"#;

    #[test]
    fn nothing_on_disk_means_no_hooks() {
        let tmp = tempfile::tempdir().expect("tempdir");

        assert!(
            load(tmp.path(), &config(None))
                .expect("no file is not an error")
                .is_empty()
        );
    }

    #[test]
    fn a_workspace_file_is_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_hooks(tmp.path(), DEFAULT_WORKSPACE_HOOKS_FILE, ONE_HOOK);

        let found = discover(tmp.path(), &config(None)).expect("parses");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, path);
        assert_eq!(found[0].scope, ContextScope::Workspace);
        assert_eq!(found[0].hooks[0].name, "guard");
    }

    #[test]
    fn the_operator_speaks_before_the_repository() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        write_hooks(tmp.path(), DEFAULT_WORKSPACE_HOOKS_FILE, ONE_HOOK);
        write_hooks(
            &global,
            DEFAULT_GLOBAL_HOOKS_FILE,
            r#"{"schema": 1, "hooks": [{"name": "personal", "command": ["/bin/true"]}]}"#,
        );

        let hooks = load(tmp.path(), &config(Some(global))).expect("parses");

        assert_eq!(
            hooks.iter().map(|hook| &hook.name).collect::<Vec<_>>(),
            vec!["personal", "guard"],
            "a global deny must be able to stop a workspace hook from ever spawning"
        );
    }

    #[test]
    fn one_file_reached_twice_is_read_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_hooks(tmp.path(), DEFAULT_GLOBAL_HOOKS_FILE, ONE_HOOK);

        let found = discover(
            tmp.path(),
            &HooksConfig {
                workspace_file: PathBuf::from(DEFAULT_GLOBAL_HOOKS_FILE),
                global_dir: Some(tmp.path().to_path_buf()),
            },
        )
        .expect("parses");

        assert_eq!(
            found.len(),
            1,
            "one guard must not deny the same call twice"
        );
    }

    #[test]
    fn a_broken_file_is_an_error_not_an_empty_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_hooks(tmp.path(), DEFAULT_WORKSPACE_HOOKS_FILE, "{ not json");

        let error = load(tmp.path(), &config(None)).expect_err("rejected");

        assert!(matches!(error, HookConfigError::Parse { .. }));
    }

    #[test]
    fn a_future_schema_is_refused_by_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_hooks(
            tmp.path(),
            DEFAULT_WORKSPACE_HOOKS_FILE,
            r#"{"schema": 99, "hooks": []}"#,
        );

        let error = load(tmp.path(), &config(None)).expect_err("rejected");

        assert!(matches!(
            error,
            HookConfigError::UnsupportedSchema { schema: 99, .. }
        ));
        assert!(error.to_string().contains("99"));
    }

    #[test]
    fn a_hook_with_no_command_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_hooks(
            tmp.path(),
            DEFAULT_WORKSPACE_HOOKS_FILE,
            r#"{"schema": 1, "hooks": [{"name": "empty", "command": []}]}"#,
        );

        let error = load(tmp.path(), &config(None)).expect_err("rejected");

        assert!(matches!(error, HookConfigError::EmptyCommand { .. }));
    }

    #[test]
    fn omitted_fields_take_the_safe_defaults() {
        let spec: HookSpec =
            serde_json::from_str(r#"{"name": "g", "command": ["/bin/true"]}"#).expect("parses");

        assert_eq!(spec.event, HookEvent::PreToolUse);
        assert_eq!(spec.on_failure, OnFailure::Deny);
        assert_eq!(spec.timeout(), DEFAULT_HOOK_TIMEOUT);
        assert!(spec.applies_to(HookEvent::PreToolUse, "anything"));
    }

    #[test]
    fn listing_tools_narrows_the_hook() {
        let spec =
            HookSpec::new("g", vec!["/bin/true".to_string()]).with_tools(vec!["shell".to_string()]);

        assert!(spec.applies_to(HookEvent::PreToolUse, "shell"));
        assert!(!spec.applies_to(HookEvent::PreToolUse, "files"));
    }

    #[test]
    fn builders_return_new_values() {
        let base = HookSpec::new("g", vec!["/bin/true".to_string()]);
        let derived = base
            .clone()
            .with_on_failure(OnFailure::Allow)
            .with_timeout(Duration::from_millis(250));

        assert_eq!(
            base.on_failure,
            OnFailure::Deny,
            "the original is untouched"
        );
        assert_eq!(base.timeout(), DEFAULT_HOOK_TIMEOUT);
        assert_eq!(derived.on_failure, OnFailure::Allow);
        assert_eq!(derived.timeout(), Duration::from_millis(250));
    }
}
