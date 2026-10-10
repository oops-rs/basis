//! Applying a re-read configuration to the live set (ADR-0031 §1).
//!
//! Every agent still configured is connected again — added and changed ones
//! because they must be, unchanged ones because a card is the remote agent's
//! to change and an unreachable one because it may have come up — and the
//! results are swapped in as one new roster. Nothing here reads a file: the
//! caller hands over the agents discovery produced, so a configuration that
//! failed to read or validate never reaches this point and the set it would
//! have replaced stands untouched.

use std::{collections::BTreeSet, sync::Arc};

use super::{RemoteAgent, RemoteAgents, Roster, connect_each, warned};
use crate::a2a::{A2aAgent, A2aReload};

/// What the configuration did to one agent that is configured now.
enum Fate {
    Added,
    Changed,
    /// Configured as before; carries what it had, so a failed card fetch can
    /// keep a working connection.
    Unchanged(Arc<RemoteAgent>),
}

impl RemoteAgents {
    /// Makes `agents` — strongest source first, one per name, as discovery
    /// layers them — the set, and reports the difference by name.
    ///
    /// The caller serializes reloads ([`LiveA2a`](crate::a2a::live::LiveA2a)
    /// does), so the difference is always against the set the previous one
    /// left. The swap happens once, after every card is in, so a reader sees
    /// the old set or the new one and never a mixture.
    pub(crate) async fn reload(&self, agents: Vec<A2aAgent>) -> A2aReload {
        let before = self.snapshot();

        let fates: Vec<Fate> = agents
            .iter()
            .map(|agent| match before.get(agent.name()) {
                None => Fate::Added,
                Some(had) if had.agent.connects_like(agent) => Fate::Unchanged(Arc::clone(had)),
                Some(_) => Fate::Changed,
            })
            .collect();

        let mut report = A2aReload {
            removed: before
                .agents
                .iter()
                .map(|had| had.name())
                .filter(|name| !agents.iter().any(|agent| agent.name() == *name))
                .map(str::to_string)
                .collect(),
            ..A2aReload::default()
        };
        let removed: BTreeSet<String> = before
            .removed
            .iter()
            .chain(&report.removed)
            .filter(|name| !agents.iter().any(|agent| agent.name() == name.as_str()))
            .cloned()
            .collect();

        let mut connected = Vec::with_capacity(agents.len());
        for ((agent, client), fate) in connect_each(agents).await.into_iter().zip(fates) {
            let name = agent.name().to_string();
            let reached = client.is_ok();
            let client = warned(&name, client).or_else(|| match &fate {
                // The configuration it was connected with is the one still
                // wanted, so the connection it has is still the right one.
                Fate::Unchanged(had) => had.client.clone(),
                Fate::Added | Fate::Changed => None,
            });
            match fate {
                Fate::Added => report.added.push(name.clone()),
                Fate::Changed => report.changed.push(name.clone()),
                Fate::Unchanged(_) => report.refreshed.push(name.clone()),
            }
            if !reached {
                report.unreachable.push(name);
            }
            connected.push(Arc::new(RemoteAgent::new(agent, client)));
        }

        self.swap(Roster {
            agents: connected,
            removed,
        });
        self.changed();
        report
    }
}
