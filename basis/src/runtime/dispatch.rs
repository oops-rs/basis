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
//! On a hit: the workspace's [`HookRunner`], which already folds the runtime's
//! host interceptors ahead of the workspace's hooks, so the chain order host
//! interceptors → supplied hooks → global hooks → workspace hooks survives the
//! move unchanged. basis adds no guards of its own here: what a workspace
//! allows is its [`RuntimePolicy`](mentra::RuntimePolicy), handed to every
//! session it mints, so a denial arrives in mentra's words on a shared runtime
//! exactly as it always did on a private one.
//!
//! On a miss — an agent whose working directory no live workspace claims — the
//! host interceptors still run, alone. Workspace hooks cannot: they belong to a
//! workspace, and there is none. A key mismatch therefore fails open for
//! *workspace hooks only*, never for the host's own interceptors.
//!
//! After the call it is the same routing.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use mentra::{
    error::RuntimeError,
    runtime::{
        HookDecision, PostExecutionContext, PostExecutionHook, PreExecutionContext,
        PreExecutionHook, ResultDecision,
    },
};

use crate::{
    RunError,
    hooks::{HookRunner, HookSpec},
};

/// One workspace's stake in the shared hook: its runner, and the participants
/// that identify it.
pub(crate) struct WorkspaceGuardEntry {
    /// The workspace's chain, host interceptors already folded first.
    pub(crate) runner: Arc<HookRunner>,
    /// The subprocess half of `runner`, in effective consultation order.
    /// Runtime interceptors are fixed on the dispatcher itself, so these are
    /// the complete per-workspace participant identity.
    pub(crate) hooks: Vec<HookSpec>,
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
/// being consulted without anyone remembering to say so. Identical live opens
/// of one canonical root share one entry and holder count; a differing set of
/// participants is refused before it can weaken the first.
pub(crate) struct HookRegistration {
    dispatch: Arc<HookDispatch>,
    key: PathBuf,
    foreign_tools: Arc<RwLock<BTreeSet<String>>>,
}

impl HookRegistration {
    /// The path this workspace is registered and dispatched under.
    ///
    /// Exposed so the open's own tests can assert it against
    /// [`Workspace::root`](crate::Workspace::root): the dispatcher key is one
    /// of the five names an open promises to settle on one directory, and
    /// without a reader nothing failed if an edit reintroduced a second
    /// spelling of it. `#[cfg(test)]` because that assertion is the only
    /// caller — `register` already holds the value, and `deregister` reads the
    /// field directly.
    #[cfg(test)]
    pub(crate) fn key(&self) -> &Path {
        &self.key
    }

    /// The one cell every identical holder under this key publishes into.
    pub(crate) fn foreign_tools(&self) -> Arc<RwLock<BTreeSet<String>>> {
        Arc::clone(&self.foreign_tools)
    }
}

impl Drop for HookRegistration {
    fn drop(&mut self) {
        self.dispatch.deregister(&self.key);
    }
}

struct Registered {
    entry: WorkspaceGuardEntry,
    holders: usize,
}

/// The one hook a basis runtime registers, on each of mentra's two seams.
pub(crate) struct HookDispatch {
    /// The host's interceptors, fixed at build like everything else on the
    /// runtime. Folded into each workspace's runner at open, and run alone on
    /// the miss path.
    interceptors: Vec<Arc<dyn crate::hooks::Interceptor>>,
    workspaces: RwLock<HashMap<PathBuf, Registered>>,
}

impl HookDispatch {
    pub(crate) fn new(interceptors: Vec<Arc<dyn crate::hooks::Interceptor>>) -> Self {
        Self {
            interceptors,
            workspaces: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn interceptors(&self) -> &[Arc<dyn crate::hooks::Interceptor>] {
        &self.interceptors
    }

    /// A runner holding this dispatcher's host interceptors and nothing else,
    /// for a call in a directory no workspace claims.
    ///
    /// `None` when the host registered no interceptors: there is nobody to ask,
    /// and building an empty runner to be told so would be the same answer at
    /// the price of a `HookRunner`. Both miss paths — before a call and after
    /// it — take exactly this runner, from here, so they cannot come to differ
    /// about which participants speak for an unclaimed directory.
    fn interceptors_only(&self, working_directory: &Path) -> Option<HookRunner> {
        if self.interceptors.is_empty() {
            return None;
        }

        Some(self.interceptors.iter().cloned().fold(
            HookRunner::new(working_directory, Vec::new()),
            |runner, interceptor| runner.with_interceptor(interceptor),
        ))
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        root: &Path,
        entry: WorkspaceGuardEntry,
    ) -> Result<HookRegistration, RunError> {
        let key = canonical(root);
        let mut workspaces = self
            .workspaces
            .write()
            .expect("workspace registry poisoned");
        let foreign_tools = match workspaces.get_mut(&key) {
            Some(held) if held.entry.hooks == entry.hooks => {
                held.holders += 1;
                Arc::clone(&held.entry.foreign_tools)
            }
            Some(_) => return Err(RunError::WorkspaceGuardConflict { root: key }),
            None => {
                let foreign_tools = Arc::clone(&entry.foreign_tools);
                workspaces.insert(key.clone(), Registered { entry, holders: 1 });
                foreign_tools
            }
        };
        drop(workspaces);

        Ok(HookRegistration {
            dispatch: Arc::clone(self),
            key,
            foreign_tools,
        })
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

    fn deregister(&self, key: &Path) {
        let mut workspaces = self
            .workspaces
            .write()
            .expect("workspace registry poisoned");
        let Some(held) = workspaces.get_mut(key) else {
            return;
        };
        held.holders = held.holders.saturating_sub(1);
        if held.holders == 0 {
            workspaces.remove(key);
        }
    }

    /// The runner registered for a working directory, cloned out so no lock is
    /// held across the await that follows.
    fn entry_for(&self, working_directory: &Path) -> Option<Arc<HookRunner>> {
        let key = canonical(working_directory);
        self.workspaces
            .read()
            .expect("workspace registry poisoned")
            .get(&key)
            .map(|held| Arc::clone(&held.entry.runner))
    }
}

#[async_trait::async_trait]
impl PreExecutionHook for HookDispatch {
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        let Some(runner) = self.entry_for(&context.working_directory) else {
            // No workspace claims this directory, so there are no workspace
            // hooks to consult — the host's own interceptors still speak.
            let Some(runner) = self.interceptors_only(&context.working_directory) else {
                return Ok(HookDecision::Allow);
            };
            return runner.pre_tool_execution(context).await;
        };

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
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        let Some(runner) = self.entry_for(&context.working_directory) else {
            let Some(runner) = self.interceptors_only(&context.working_directory) else {
                return Ok(ResultDecision::Keep);
            };
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

#[cfg(test)]
mod tests;
