//! Lifetime guard for one lossless session observer.

/// Keeps one [`PreparedRun`](super::PreparedRun) agent-event tap registered.
///
/// Dropping this value waits for any callback already in flight, then prevents
/// every future invocation. Keep it alive for at least as long as the work
/// being observed; do not drop it while holding a resource the callback needs.
///
/// This is a Basis-owned opaque wrapper rather than Mentra's raw guard. That
/// keeps the public run contract stable.
#[must_use = "dropping the guard unregisters the tap and may wait for an in-flight callback"]
pub struct AgentEventTapGuard {
    _tap: mentra::agent::AgentEventTapGuard,
}

impl AgentEventTapGuard {
    pub(super) fn new(tap: mentra::agent::AgentEventTapGuard) -> Self {
        Self { _tap: tap }
    }
}
