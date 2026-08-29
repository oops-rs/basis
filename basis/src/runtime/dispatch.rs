//! Routing one runtime's execution hooks to many workspaces.
//!
//! A mentra runtime takes its hooks at build time and never again, and a
//! shared runtime is built before any workspace opens. So basis registers exactly
//! one dispatcher — [`HookDispatch`], on both seams — and the per-workspace
//! participants arrive and leave through it:
//! [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open) inserts a
//! [`WorkspaceGuardEntry`] keyed by the workspace's canonicalized root, and the
//! [`HookRegistration`] it gets back removes the entry when the
//! [`Workspace`](crate::Workspace) drops.
//!
//! Dispatch keys on `working_directory`, which is the agent's `base_dir` — the
//! same path basis scoped the agent to at open, and the same field on both of
//! mentra's contexts, so a call and its result route to one entry. Both sides
//! canonicalize through one helper, so a symlinked spelling and its target land
//! on one entry too.
//!
//! Both seams are registered unconditionally, because whether any workspace
//! will ever have a post hook is not knowable when the runtime is built. That
//! costs a runtime with no participants at all one map lookup per tool result,
//! and mentra one context to assemble; the alternative is a runtime that
//! silently cannot be given one later.
//!
//! # What runs, in what order
//!
//! On a hit: basis's own guards first — the shell posture and the `.git`
//! carve-out, and only for a workspace on a *shared* runtime, whose policy
//! cannot carry either per workspace; a private runtime's policy already does,
//! so its denials keep mentra's wording — then the workspace's [`HookRunner`],
//! which already folds the runtime's host interceptors ahead of the
//! workspace's hooks, so the chain order host interceptors → global hooks →
//! workspace hooks survives the move unchanged.
//!
//! On a miss — an agent whose working directory no live workspace claims — the
//! host interceptors still run, alone. Workspace hooks cannot: they belong to a
//! workspace, and there is none. A key mismatch therefore fails open for
//! *workspace hooks only*, never for the host's own guards, and the guards
//! basis adds here were policy-level hygiene rather than a boundary to begin
//! with (ADR-0004, ADR-0013).
//!
//! After the call it is the same routing without the guards: they answer
//! whether a call happens, and by then it has.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use mentra::{
    error::RuntimeError,
    runtime::{
        HookDecision, PostExecutionContext, PostExecutionHook, PreExecutionContext,
        PreExecutionHook, ResultDecision,
    },
};

use crate::{
    hooks::HookRunner,
    shell::ShellAccess,
    tools::{
        SPAWN,
        spawn::{SpawnMode, parse_spawn},
    },
};

/// One workspace's stake in the shared hook: its runner, its command posture,
/// and the root its guards measure paths against.
pub(crate) struct WorkspaceGuardEntry {
    /// The workspace's chain, host interceptors already folded first.
    pub(crate) runner: Arc<HookRunner>,
    /// Enforced here when [`shared`](Self::shared); baked into policy on the
    /// private path, where the runtime belongs to this workspace alone.
    pub(crate) shell: ShellAccess,
    /// Canonicalized. What the `.git` guard resolves candidate paths against.
    pub(crate) root: PathBuf,
    /// Whether the runtime's policy is shared across workspaces and therefore
    /// cannot carry this workspace's shell posture or `.git` carve-out. When
    /// true the guards run here; when false they do not, so the private path's
    /// denials keep coming from mentra's policy, word for word, exactly as
    /// before the split.
    pub(crate) shared: bool,
    /// The names [`Workspace::minted_agent`](crate::Workspace) hid from this
    /// workspace's model at its most recent mint: a sibling workspace's
    /// bridged `mcp__*` tools and a sibling's declared tools.
    ///
    /// Published here because `spawn` needs it and cannot ask mentra for it:
    /// a child spawned from a [`ChildSpec`](crate::ChildSpec) with a roster
    /// override replaces the cloned config's `ToolProfile` wholesale, which
    /// would hand the child the very names the parent is denied — and mentra
    /// 0.21 exposes no reader for a template's or an agent's effective
    /// profile (upstream candidate). Shared rather than copied so what
    /// `spawn` reads is the set the *live* parent was minted with, since a
    /// mint is exactly when both are settled: the agent's config freezes it
    /// and this cell is written in the same breath.
    pub(crate) foreign_tools: Arc<RwLock<BTreeSet<String>>>,
}

/// Removes its workspace's entry when dropped.
///
/// Held by the [`Workspace`](crate::Workspace), so a dropped workspace stops
/// being consulted without anyone remembering to say so. When two live
/// workspaces share one canonical root the later registration wins while both
/// live, and each drop removes only its own entry.
pub(crate) struct HookRegistration {
    dispatch: Arc<HookDispatch>,
    key: PathBuf,
    id: u64,
}

impl Drop for HookRegistration {
    fn drop(&mut self) {
        self.dispatch.deregister(&self.key, self.id);
    }
}

struct Registered {
    id: u64,
    entry: WorkspaceGuardEntry,
}

/// The one hook a basis runtime registers, on each of mentra's two seams.
pub(crate) struct HookDispatch {
    /// The host's interceptors, fixed at build like everything else on the
    /// runtime. Folded into each workspace's runner at open, and run alone on
    /// the miss path.
    interceptors: Vec<Arc<dyn crate::hooks::Interceptor>>,
    workspaces: RwLock<HashMap<PathBuf, Registered>>,
    next_id: AtomicU64,
}

impl HookDispatch {
    pub(crate) fn new(interceptors: Vec<Arc<dyn crate::hooks::Interceptor>>) -> Self {
        Self {
            interceptors,
            workspaces: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    pub(crate) fn interceptors(&self) -> &[Arc<dyn crate::hooks::Interceptor>] {
        &self.interceptors
    }

    pub(crate) fn register(self: &Arc<Self>, entry: WorkspaceGuardEntry) -> HookRegistration {
        let key = canonical(&entry.root);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.workspaces
            .write()
            .expect("workspace registry poisoned")
            .insert(key.clone(), Registered { id, entry });

        HookRegistration {
            dispatch: Arc::clone(self),
            key,
            id,
        }
    }

    /// What the workspace claiming `working_directory` hid from its own model
    /// at its last mint — what a delegated child must therefore not be
    /// offered either (see [`WorkspaceGuardEntry::foreign_tools`]).
    ///
    /// Empty on a miss, and that is the honest answer rather than a fallback:
    /// a miss means no live workspace claims this directory, so there is no
    /// sibling to be shielded from. Every private runtime — every
    /// `Workspace::open(path)`, the CLI, the free functions — has no siblings
    /// at all and reads empty here for the same reason.
    pub(crate) fn foreign_tools(&self, working_directory: &Path) -> BTreeSet<String> {
        let key = canonical(working_directory);
        self.workspaces
            .read()
            .expect("workspace registry poisoned")
            .get(&key)
            .map(|held| {
                held.entry
                    .foreign_tools
                    .read()
                    .expect("foreign tool set poisoned")
                    .clone()
            })
            .unwrap_or_default()
    }

    fn deregister(&self, key: &Path, id: u64) {
        let mut workspaces = self
            .workspaces
            .write()
            .expect("workspace registry poisoned");
        if workspaces.get(key).is_some_and(|held| held.id == id) {
            workspaces.remove(key);
        }
    }

    /// The registered entry for a working directory, cloned out so no lock is
    /// held across the await that follows.
    fn entry_for(
        &self,
        working_directory: &Path,
    ) -> Option<(Arc<HookRunner>, ShellAccess, PathBuf, bool)> {
        let key = canonical(working_directory);
        self.workspaces
            .read()
            .expect("workspace registry poisoned")
            .get(&key)
            .map(|held| {
                (
                    Arc::clone(&held.entry.runner),
                    held.entry.shell,
                    held.entry.root.clone(),
                    held.entry.shared,
                )
            })
    }
}

#[async_trait::async_trait]
impl PreExecutionHook for HookDispatch {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        let Some((runner, shell, root, shared)) = self.entry_for(&context.working_directory) else {
            if self.interceptors.is_empty() {
                return Ok(HookDecision::Allow);
            }
            // No workspace claims this directory, so there are no workspace
            // hooks to consult — the host's own guards still speak.
            let runner = self.interceptors.iter().cloned().fold(
                HookRunner::new(&context.working_directory, Vec::new()),
                |runner, interceptor| runner.with_interceptor(interceptor),
            );
            return runner.pre_tool_execution(context).await;
        };

        if shared && let Some(reason) = guard(context, shell, &root) {
            return Ok(HookDecision::Deny(reason));
        }

        runner.pre_tool_execution(context).await
    }
}

#[async_trait::async_trait]
impl PostExecutionHook for HookDispatch {
    /// The same routing, on the other side of the call.
    ///
    /// [`PostExecutionContext::working_directory`] is the key
    /// [`PreExecutionContext::working_directory`] was, so a result is judged
    /// by the workspace whose participants judged the call — the alternative
    /// being a rewrite from a workspace that never saw what it was rewriting.
    ///
    /// basis's own guards do not run here. They decide whether a call happens,
    /// and this one has; a shell posture cannot un-run a command, and the
    /// `.git` carve-out has nothing to say about the output of a write it
    /// already refused.
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        let Some((runner, ..)) = self.entry_for(&context.working_directory) else {
            if self.interceptors.is_empty() {
                return Ok(ResultDecision::Keep);
            }
            let runner = self.interceptors.iter().cloned().fold(
                HookRunner::new(&context.working_directory, Vec::new()),
                |runner, interceptor| runner.with_interceptor(interceptor),
            );
            return runner.post_tool_execution(context).await;
        };

        runner.post_tool_execution(context).await
    }
}

/// Forwards to the dispatcher inside, so one dispatcher can be both the hook
/// mentra owns and the registry basis keeps a handle on.
pub(crate) struct DispatchHook(pub(crate) Arc<HookDispatch>);

#[async_trait::async_trait]
impl PreExecutionHook for DispatchHook {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        self.0.pre_tool_execution(context).await
    }
}

#[async_trait::async_trait]
impl PostExecutionHook for DispatchHook {
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        self.0.post_tool_execution(context).await
    }
}

/// basis's own guards, ahead of every configured participant.
///
/// These carry rules that lived in the private runtime's `RuntimePolicy` —
/// which a shared runtime cannot hold per-workspace — into the one seam that
/// knows which workspace a call belongs to. Hygiene, not a boundary, exactly
/// as the policy versions were: a shell redirect still reaches any path,
/// because nothing here parses shell (ADR-0004, ADR-0013).
fn guard(context: &PreExecutionContext, shell: ShellAccess, root: &Path) -> Option<String> {
    if context.tool_name == SPAWN && !shell.is_granted() {
        // The `!` prefix is read by the same parser `spawn` itself uses, so
        // this guard and the tool can never disagree about which calls are
        // commands (the "one reader" rule on `tools::spawn::parse`).
        let input: serde_json::Value = serde_json::from_str(&context.input_json).ok()?;
        if parse_spawn(&input).is_ok_and(|spawn| spawn.mode() == SpawnMode::Command) {
            return Some(
                "command execution is denied: this workspace was opened with commands off"
                    .to_string(),
            );
        }
        return None;
    }

    // The builtin file tools are the route this guard closes, mirroring the
    // `with_denied_write_root` entries the private path bakes into policy (see
    // `git_protected` in `runtime::builder`). *Both* rosters, because the
    // policy this stands in for binds at the workspace engine — mentra's
    // `WorkspaceEditor::authorize_write`, which the batched ops and the split
    // `write`/`edit` both call — so a guard that knew one profile's names
    // would be narrower on a shared runtime than the policy it mirrors.
    let input: serde_json::Value = serde_json::from_str(&context.input_json).ok()?;
    let targets = write_targets(&context.tool_name, &input);
    if targets.is_empty() {
        return None;
    }

    let denied = [root.join(".git/hooks"), root.join(".git/config")].map(|p| resolved(root, &p));
    for raw in targets {
        let candidate = resolved(root, Path::new(raw));
        if denied.iter().any(|root| candidate.starts_with(root)) {
            return Some(format!(
                "path '{}' is under this workspace's protected git paths \
                 (.git/hooks, .git/config decide what runs)",
                candidate.display()
            ));
        }
    }

    None
}

/// The paths one file-tool call writes, whichever profile the runtime offers.
///
/// A tool that writes nothing is absent by name rather than by inspection: the
/// readers (`read`, `ls`, `grep`, `glob`, and the batched read ops) never reach
/// `authorize_write`, so answering for them would refuse what the policy this
/// mirrors allows.
fn write_targets<'a>(tool_name: &str, input: &'a serde_json::Value) -> Vec<&'a str> {
    match tool_name {
        "files" => input
            .get("operations")
            .and_then(serde_json::Value::as_array)
            .map(|operations| operations.iter().flat_map(batched_targets).collect())
            .unwrap_or_default(),
        // mentra's split writers take one path each, under any of three
        // spellings: `path`, `file_path` and `filePath` are serde aliases for
        // one field (`tool/coding/input.rs`). Reading only the first would be
        // a guard bypassed by asking for the second.
        "write" | "edit" => ["path", "file_path", "filePath"]
            .iter()
            .filter_map(|name| input.get(*name).and_then(serde_json::Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// The paths a batched `files` operation writes, by the ops mentra's tool defines.
///
/// `move` touches both ends: writing into a protected path plants a program,
/// and moving one out from under git's feet changes what runs just as surely.
fn batched_targets(operation: &serde_json::Value) -> Vec<&str> {
    let field = |name: &str| operation.get(name).and_then(serde_json::Value::as_str);

    match field("op") {
        Some("create" | "set" | "replace" | "insert" | "delete") => {
            field("path").into_iter().collect()
        }
        Some("move") => field("from").into_iter().chain(field("to")).collect(),
        _ => Vec::new(),
    }
}

/// One canonicalization for the registration key and the lookup key.
///
/// A path that does not resolve is used as written rather than rejected — the
/// same ruling as [`store::runtime_identifier`](crate::store::runtime_identifier),
/// and for the same reason: keying the map is not the place to validate a
/// workspace.
///
/// Simplified the way `context::discovery::validate_workspace` simplifies the
/// root it hands out, so a workspace registered under its own
/// resolved root keys the map under the path it calls itself rather than under
/// a Windows verbatim spelling of it. Lookup and registration would agree
/// either way — both come through here — but a key that is not the root is a
/// second spelling of the same directory, which is the thing resolving once
/// exists to prevent.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .map(|resolved| dunce::simplified(&resolved).to_path_buf())
        .unwrap_or_else(|_| path.to_path_buf())
}

/// A candidate path as the guard measures it: absolute against the workspace
/// root, `.` and `..` folded, and the deepest existing prefix resolved so a
/// symlinked spelling of a protected place answers the same as the plain one.
///
/// The mirror of mentra's `normalize_policy_root`, ported rather than shared
/// because mentra keeps its own private.
fn resolved(root: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let normalized = lexically_normalized(&joined);

    let mut existing = normalized.clone();
    let mut tail = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return normalized,
        }
    }

    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    resolved
}

/// Folds `.` and `..` without touching the filesystem. `..` at a root is
/// clamped rather than an error: the guard prefers a conservative spelling to
/// letting a strange path through unjudged.
fn lexically_normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests;
