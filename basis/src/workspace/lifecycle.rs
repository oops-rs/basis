//! Independent-mint posture for one opened workspace.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::RunError;

/// Whether this workspace can mint many independent sessions or exactly one.
pub(crate) enum MintPosture {
    Multiple,
    FreshOnly(AtomicBool),
}

impl MintPosture {
    pub(crate) const fn new(fresh_only: bool) -> Self {
        if fresh_only {
            Self::FreshOnly(AtomicBool::new(false))
        } else {
            Self::Multiple
        }
    }

    /// Irreversibly claims the one fresh-only attempt, when configured.
    ///
    /// No rollback on a later error: mint/resume can mutate or persist runtime
    /// state before failing, and Gate 1a deliberately has no scrub contract.
    pub(crate) fn claim(&self) -> Result<(), RunError> {
        match self {
            Self::Multiple => Ok(()),
            Self::FreshOnly(claimed) if !claimed.swap(true, Ordering::Relaxed) => Ok(()),
            Self::FreshOnly(_) => Err(RunError::FreshOnlyRunAlreadyAttempted),
        }
    }

    pub(crate) const fn is_fresh_only(&self) -> bool {
        matches!(self, Self::FreshOnly(_))
    }
}

impl std::fmt::Debug for MintPosture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintPosture")
            .field("fresh_only", &self.is_fresh_only())
            .finish()
    }
}
