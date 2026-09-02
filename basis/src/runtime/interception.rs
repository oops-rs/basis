//! The host's own guards, on every session this runtime carries.
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

use std::{path::Path, sync::Arc};

use mentra::{
    error::RuntimeError,
    runtime::{
        AfterDecision, BeforeDecision, ExecutionHookParticipant, PostExecutionContext,
        PreExecutionContext,
    },
};

use crate::hooks::{HookRunner, Interceptor};

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
