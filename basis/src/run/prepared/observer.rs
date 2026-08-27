//! Lifetime guard for one lossless session observer.

use std::any::Any;

/// Keeps one [`PreparedRun`](super::PreparedRun) agent-event tap registered.
///
/// Dropping this value unregisters the callback immediately. Keep it alive for
/// at least as long as the work being observed; binding it to `_` drops it at
/// once.
///
/// This is a Basis-owned opaque wrapper rather than Mentra's raw guard. That
/// keeps the public run contract stable while the private registration can
/// also retain a run-lifecycle lease when pooled runtimes arrive.
#[must_use = "dropping the guard unregisters the agent event tap immediately"]
pub struct AgentEventTapGuard {
    _registration: ObserverRegistration,
}

struct ObserverRegistration {
    // Fields drop in declaration order: unregister the callback before a
    // future lifecycle lease is released.
    _tap: mentra::agent::AgentEventTapGuard,
    _lifecycle_lease: Option<Box<dyn Any + Send + Sync>>,
}

impl AgentEventTapGuard {
    pub(super) fn new(tap: mentra::agent::AgentEventTapGuard) -> Self {
        Self {
            _registration: ObserverRegistration {
                _tap: tap,
                _lifecycle_lease: None,
            },
        }
    }
}
