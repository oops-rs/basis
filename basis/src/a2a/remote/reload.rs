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

use mentra::a2a::A2aClient;

use super::{RemoteAgent, RemoteAgents, Roster, connect_each, warned};
use crate::a2a::{A2aAgent, A2aReload};

/// What the configuration did to one agent that is configured now.
enum Fate {
    Added,
    Changed,
    Unchanged,
}

impl RemoteAgents {
    /// Makes `agents` — strongest source first, one per name, as discovery
    /// layers them — the set, and reports the difference by name.
    ///
    /// The caller serializes reloads ([`LiveA2a`](crate::a2a::live::LiveA2a)
    /// does), so the difference is always against the set the previous one
    /// left. The swap happens once, after every card is in, so a reader sees
    /// the old set or the new one and never a mixture.
    ///
    /// A retry is not serialized with a reload, and may reach an agent while
    /// this one is connecting. So what a failed card fetch keeps is decided at
    /// the swap, against the set as it is then: an agent whose fetch failed
    /// keeps whatever connection the current set holds for it, as long as that
    /// connection was made with the configuration still wanted.
    pub(crate) async fn reload(&self, agents: Vec<A2aAgent>) -> A2aReload {
        let before = self.snapshot();

        let fates: Vec<Fate> = agents
            .iter()
            .map(|agent| match before.get(agent.name()) {
                None => Fate::Added,
                Some(had) if had.agent.connects_like(agent) => Fate::Unchanged,
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

        let mut fetched = Vec::with_capacity(agents.len());
        for ((agent, client), fate) in connect_each(agents).await.into_iter().zip(fates) {
            let name = agent.name().to_string();
            match fate {
                Fate::Added => report.added.push(name.clone()),
                Fate::Changed => report.changed.push(name.clone()),
                Fate::Unchanged => report.refreshed.push(name.clone()),
            }
            if client.is_err() {
                report.unreachable.push(name.clone());
            }
            fetched.push((agent, warned(&name, client)));
        }

        self.swap_with(|current| Roster {
            agents: fetched
                .into_iter()
                .map(|(agent, client)| settled(current, agent, client))
                .collect(),
            removed,
        });
        self.changed();
        report
    }
}

/// The entry one reloaded agent gets, given the set as it is at the swap.
///
/// A fresh card wins. Without one, the current entry stands when it was made
/// with the configuration still wanted — its connection, whether this reload
/// saw it or a retry installed it since, still reaches the right agent the
/// right way. Otherwise there is nothing to send to.
fn settled(current: &Roster, agent: A2aAgent, client: Option<Arc<A2aClient>>) -> Arc<RemoteAgent> {
    if client.is_none()
        && let Some(had) = current.get(agent.name())
        && had.client.is_some()
        && had.agent.connects_like(&agent)
    {
        return Arc::clone(had);
    }
    Arc::new(RemoteAgent::new(agent, client))
}
