//! What a reload of a workspace's remote agents did (ADR-0031 §1).

/// What [`Workspace::reload_a2a`](crate::Workspace::reload_a2a) changed, by
/// agent name.
///
/// Names only, for [`Workspace::remote_agents`](crate::Workspace::remote_agents)'s
/// reason: this is what a host logs, and a URL or a header value is a
/// credential as often as not. Each list is in configuration order, except
/// `removed`, which is in the order the agents were configured before.
///
/// `added`, `changed` and `refreshed` say what the *configuration* did to each
/// agent still configured, and every such agent is in exactly one of them.
/// `unreachable` says which of those agents' cards could not be fetched on
/// this reload, whichever list they are in — so a newly added agent that is
/// down is in both `added` and `unreachable`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct A2aReload {
    /// Configured now and not before: connected.
    pub added: Vec<String>,
    /// Configured before and not now: no longer routable. The conversation a
    /// session had with one is kept, and continues if the name comes back.
    pub removed: Vec<String>,
    /// Configured before and now under a different URL, header set or limits:
    /// reconnected with the new configuration.
    pub changed: Vec<String>,
    /// Configured the same as before: its card fetched again, since a card is
    /// the remote agent's to change.
    pub refreshed: Vec<String>,
    /// Configured, and the card could not be fetched on this reload.
    ///
    /// A changed or added agent here has nothing to send to until a later
    /// attempt succeeds — `@<agent>` retries it. An unchanged agent that was
    /// connected before keeps the connection it had: one failed card fetch is
    /// not evidence that the endpoint it already reaches has gone.
    pub unreachable: Vec<String>,
}

impl A2aReload {
    /// Whether the reload changed which agents are configured or how — the
    /// question a host asks before saying anything about it.
    pub fn changed_configuration(&self) -> bool {
        !(self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty())
    }
}
