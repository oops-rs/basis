//! Lifetime guard for one lossless session observer.

use crate::workspace::lifecycle::ReuseLease;

/// Keeps one [`PreparedRun`](super::PreparedRun) agent-event tap registered.
///
/// Dropping this value waits for any callback already in flight, then prevents
/// every future invocation. Keep it alive for at least as long as the work
/// being observed; do not drop it while holding a resource the callback needs.
///
/// This is a Basis-owned opaque wrapper rather than Mentra's raw guard. That
/// keeps the public run contract stable while the private registration can
/// also retain a run-lifecycle lease when pooled runtimes arrive.
#[must_use = "dropping the guard unregisters the tap and may wait for an in-flight callback"]
pub struct AgentEventTapGuard {
    _registration: ObserverRegistration,
}

struct ObserverRegistration {
    // Fields drop in declaration order: unregister the callback before a
    // future lifecycle lease is released.
    _tap: mentra::agent::AgentEventTapGuard,
    _lifecycle_lease: Option<ReuseLease>,
}

impl AgentEventTapGuard {
    pub(super) fn new(
        tap: mentra::agent::AgentEventTapGuard,
        lifecycle_lease: Option<ReuseLease>,
    ) -> Self {
        Self {
            _registration: ObserverRegistration {
                _tap: tap,
                _lifecycle_lease: lifecycle_lease,
            },
        }
    }
}
