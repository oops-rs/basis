//! Independent-mint and reusable-generation posture for one opened workspace.

use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};

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

/// One reusable runtime generation's binding, escape, sealing, and lease state.
pub(crate) struct ReuseLifecycle {
    state: Arc<Mutex<ReuseState>>,
}

#[derive(Debug, Default)]
struct ReuseState {
    bound: bool,
    poisoned: bool,
    sealed: bool,
}

impl ReuseLifecycle {
    pub(crate) fn unbound() -> Self {
        Self {
            state: Arc::new(Mutex::new(ReuseState::default())),
        }
    }

    pub(crate) fn require_unbound(&self) -> Result<(), RunError> {
        let state = self.lock();
        if state.poisoned {
            return Err(RunError::ReusableWorkspaceRawAccess);
        }
        if state.bound {
            return Err(RunError::ReusableWorkspaceAlreadyBound);
        }
        Ok(())
    }

    pub(crate) fn mark_bound(&self) -> Result<(), RunError> {
        let mut state = self.lock();
        if state.poisoned {
            return Err(RunError::ReusableWorkspaceRawAccess);
        }
        if state.bound {
            return Err(RunError::ReusableWorkspaceAlreadyBound);
        }
        state.bound = true;
        Ok(())
    }

    pub(crate) fn lease_run(&self) -> Result<ReuseLease, RunError> {
        let state = self.lock();
        if !state.bound {
            return Err(RunError::ReusableWorkspaceToolsUnbound);
        }
        if state.poisoned {
            return Err(RunError::ReusableWorkspaceRawAccess);
        }
        if state.sealed {
            return Err(RunError::ReusableWorkspaceSealed);
        }
        Ok(ReuseLease {
            state: Arc::clone(&self.state),
        })
    }

    pub(crate) fn poison(&self) {
        self.lock().poisoned = true;
    }

    /// Seals this generation and reports every outstanding run-derived lease.
    pub(crate) fn seal_for_rebuild(&self) -> Result<(), RunError> {
        let mut state = self.lock();
        state.sealed = true;
        if !state.bound {
            return Err(RunError::ReusableWorkspaceToolsUnbound);
        }
        if state.poisoned {
            return Err(RunError::ReusableWorkspaceRawAccess);
        }

        // Keep the state lock through the ownership decision. A concurrent
        // raw accessor must retain its lease while it waits to set `poisoned`,
        // so the strong count refuses this rebuild. Unlocking before sampling
        // would let that accessor poison, drop its lease, and leave a stale
        // successful decision based on the earlier `poisoned` read.
        let leases = Arc::strong_count(&self.state).saturating_sub(1);
        if leases == 0 {
            Ok(())
        } else {
            Err(RunError::ReusableWorkspaceOutstanding { leases })
        }
    }

    fn lock(&self) -> MutexGuard<'_, ReuseState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One live run, observer, or forwarder derived from a reusable generation.
#[derive(Clone)]
pub(crate) struct ReuseLease {
    state: Arc<Mutex<ReuseState>>,
}

impl ReuseLease {
    pub(crate) fn poison(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .poisoned = true;
    }
}
