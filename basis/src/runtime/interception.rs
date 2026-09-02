//! Who judges a tool call on this runtime: the host's own guards, and each
//! workspace's chain.
//!
//! Two registrations of one kind, and they are here together because they are
//! two ends of one chain. The host's [`Interceptor`]s are global — the runtime
//! is what they belong to — and a workspace's `.basis/hooks.json` runner is
//! registered for that workspace's audience, holder-counted so one directory
//! carries one chain however many times it is open. mentra joins them per call.
//!
//! # The host's own guards, on every session this runtime carries
//!
//! [`RuntimeBuilder::with_interceptor`](crate::RuntimeBuilder::with_interceptor)
//! promises runtime scope, because host scope *is* runtime scope (ADR-0018):
//! an interceptor is compiled into the embedding program and judges every call
//! that program's runtime executes. A workspace is a smaller thing than that,
//! so folding interceptors into each workspace's own chain would keep the
//! promise for the sessions basis mints and quietly break it for the ones a
//! host creates for itself through
//! [`Runtime::mentra_runtime`](crate::Runtime::mentra_runtime) — the session
//! with no tool audience, which mentra's audience-scoped registries never
//! consult.
//!
//! So the interceptors are registered **once, globally**, when the runtime is
//! built, and this is the participant that carries them.
//!
//! # Why this is still one chain
//!
//! mentra composes one participant snapshot per call out of every batch whose
//! audience matches — a global batch matches every session, an audience batch
//! matches its own — and walks that one list forward on both seams. So the
//! global batch registered here and each workspace's audience batch are *one*
//! chain, in registration order: this runtime is built before any workspace
//! opens, so the host's guards speak first, exactly as
//! [`crate::hooks`] describes. A refusal here short-circuits before a
//! repository's hook program is spawned, and a rewrite's attribution
//! accumulates across both batches rather than being lost between two chains.
//!
//! # Why the workspace is the call's, not the runtime's
//!
//! A [`HookRequest`](crate::hooks::HookRequest)'s `cwd` is the directory the
//! call happened in, and a runtime has no directory of its own. mentra hands
//! over the calling agent's working directory on both seams, so the runner is
//! built around that — one `Arc` clone per participant per call, which is what
//! basis's own dispatcher did on this path before hooks went live.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mentra::{
    error::RuntimeError,
    runtime::{
        AfterDecision, BeforeDecision, ExecutionHookParticipant, PostExecutionContext,
        PreExecutionContext,
    },
    tool::ToolAudience,
};

use crate::{
    error::RunError,
    hooks::{HookRunner, Interceptor},
};

use super::Runtime;

/// Every interceptor a runtime was built with, as one participant.
pub(crate) struct HostInterceptors {
    interceptors: Vec<Arc<dyn Interceptor>>,
}

impl HostInterceptors {
    /// `None` when the host registered none: there is nobody to ask, and a
    /// registration that always answers "continue" is a batch mentra would
    /// walk on every call of every session for nothing.
    pub(crate) fn new(interceptors: Vec<Arc<dyn Interceptor>>) -> Option<Self> {
        if interceptors.is_empty() {
            return None;
        }

        Some(Self { interceptors })
    }

    /// A runner holding these interceptors and no subprocess hooks, scoped to
    /// the directory the call came from.
    fn runner(&self, workspace: &Path) -> HookRunner {
        self.interceptors.iter().cloned().fold(
            HookRunner::new(workspace, Vec::new()),
            HookRunner::with_interceptor,
        )
    }
}

#[async_trait::async_trait]
impl ExecutionHookParticipant for HostInterceptors {
    fn name(&self) -> &str {
        "basis host interceptors"
    }

    async fn before(&self, context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        self.runner(&context.working_directory)
            .before(context)
            .await
    }

    async fn after(&self, context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        self.runner(&context.working_directory).after(context).await
    }
}

/// One workspace's interception chain, registered live for its audience.
///
/// The third join-and-count ledger on this runtime — [`super::claims`] holds
/// the other two — and it exists for the reason they do: a *root* may be open
/// twice — the shape
/// `basis-host` produces deliberately, one workspace per set of
/// client-supplied MCP servers — while the thing being registered is one per
/// root. An audience is derived from the root, so two such opens would
/// otherwise put two complete chains behind one audience and mentra would walk
/// both for either one's calls: every subprocess hook spawned twice per call,
/// an audit hook logging each call twice, and a rewrite that is not idempotent
/// fed its own output.
///
/// So the second open of a root **joins** the first's registration, and the
/// chain comes off when the last holder goes — exactly as a declared tool's
/// claim behaves. The price of joining is that both opens must agree about
/// what the chain *is*: see [`Runtime::register_hook_chain`].
#[derive(Debug)]
pub(super) struct HookChainClaim {
    /// The workspace root behind the audience, for the refusal's message.
    root: PathBuf,
    holders: usize,
    /// The chain the live registration is running. A same-root open presenting
    /// a different one is refused rather than silently subjected to this.
    hooks: Vec<crate::hooks::HookSpec>,
    /// mentra's own hold, kept beside the claim rather than by a workspace
    /// because the claim is what counts holders: the chain has to outlive the
    /// first of them to drop. Removing the claim drops this, and dropping it
    /// is the unregister.
    #[allow(dead_code, reason = "held for its Drop")]
    registration: mentra::runtime::ExecutionHookRegistration,
}

/// One holder's share of a live interception chain, released on drop.
///
/// The sibling of [`DeclaredTools`](crate::tools::declared::DeclaredTools) and
/// [`SkillRoots`](crate::skills::SkillRoots), and held by the
/// [`Workspace`](crate::Workspace) for the same reason: dropping it is what
/// stops a dropped workspace being consulted.
pub(crate) struct HookChainHold {
    runtime: Arc<Runtime>,
    /// The audience this chain answers for — the key, and the only thing the
    /// release needs.
    audience: String,
}

impl std::fmt::Debug for HookChainHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookChainHold")
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl Drop for HookChainHold {
    fn drop(&mut self) {
        self.runtime.release_hook_chain(&self.audience);
    }
}

impl Runtime {
    /// Puts a workspace's interception chain on the runtime for its audience,
    /// or joins the identical one already there.
    ///
    /// `Ok` on two shapes and refuses a third:
    ///
    /// - **Nobody holds this audience.** The runner is registered as one
    ///   atomic [`ExecutionHookParticipant`](mentra::runtime::ExecutionHookParticipant)
    ///   batch and the guard goes into the claim.
    /// - **A live open of this same root presents the same chain.** It joins:
    ///   one registration, one holder more. That is what keeps a repository's
    ///   hook programs spawned once per call rather than once per call per
    ///   live open, and what keeps a non-idempotent rewrite from being fed its
    ///   own output — the thing the deleted directory-keyed registry counted
    ///   holders for.
    /// - **A live open of this root presents a *different* chain.** Refused
    ///   with [`RunError::WorkspaceGuardConflict`]. Joining would subject the
    ///   first open's sessions to a chain their caller never configured, and
    ///   registering a second batch would run both for either — so the honest
    ///   answers are "the same chain" or "no". A host that genuinely needs two
    ///   hook configurations for one directory needs two runtimes.
    ///
    /// Compared on the chain itself rather than on a digest of it, because a
    /// [`HookSpec`](crate::hooks::HookSpec) is small, `Eq`, and already the
    /// complete statement of what a participant will do.
    pub(crate) fn register_hook_chain(
        self: &Arc<Self>,
        audience: &ToolAudience,
        root: &Path,
        runner: crate::hooks::HookRunner,
    ) -> Result<HookChainHold, RunError> {
        let key = audience.as_str().to_string();
        let mut chains = self.hook_chains.lock().expect("hook chain map poisoned");

        match chains.get_mut(&key) {
            Some(claim) if claim.hooks != *runner.hooks() => {
                return Err(RunError::WorkspaceGuardConflict {
                    root: claim.root.clone(),
                });
            }
            Some(claim) => claim.holders += 1,
            None => {
                let hooks = runner.hooks().to_vec();
                // Under the claim lock, so nothing can observe an audience
                // counted but unregistered, or free one between the register
                // and the count.
                let registration = self
                    .mentra
                    .register_execution_hook_for_audience(audience.clone(), runner);
                chains.insert(
                    key.clone(),
                    HookChainClaim {
                        root: root.to_path_buf(),
                        holders: 1,
                        hooks,
                        registration,
                    },
                );
            }
        }
        drop(chains);

        Ok(HookChainHold {
            runtime: Arc::clone(self),
            audience: key,
        })
    }

    /// Releases one holder's share, taking the chain off the runtime when the
    /// last of them goes.
    ///
    /// Under the claim lock, like the declared-tool and skills-root releases in
    /// [`super::claims`], so no other opener can see an audience free while its
    /// chain is still registered: the removed claim owns mentra's guard, and
    /// dropping it here is the unregister.
    fn release_hook_chain(&self, audience: &str) {
        let mut chains = self.hook_chains.lock().expect("hook chain map poisoned");

        let Some(claim) = chains.get_mut(audience) else {
            return;
        };
        claim.holders = claim.holders.saturating_sub(1);
        if claim.holders == 0 {
            chains.remove(audience);
        }
    }
}
