//! Reaching one configured agent by name, retrying an unreachable one when it
//! is asked for (ADR-0031 §2).
//!
//! Two questions, because `spawn` asks twice. Its preview runs before the
//! approver and cannot wait on the network, so [`ConfiguredAgents::standing`]
//! answers from the set alone: known, due a retry, or not worth asking about
//! yet. Its execution can wait, so [`ConfiguredAgents::reach`] hands back the
//! client to use — connecting an unreachable agent first when its backoff has
//! passed. The claim on a retry is taken under the agent's own lock, so two
//! calls racing for one down agent cost one connect attempt, not two.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use mentra::a2a::A2aClient;

use super::{ConfiguredAgents, RemoteAgent, Roster};
use crate::a2a::resolver::{Resolved, Route, Unresolved};

impl RemoteAgent {
    /// How long until this agent is due a retry; zero when it is due now.
    fn retry_in(&self, backoff: Duration) -> Duration {
        let attempted = *self
            .attempted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        backoff.saturating_sub(attempted.elapsed())
    }

    /// Takes the retry, when it is due: the attempt is stamped now, under the
    /// lock, so a second caller sees the backoff rather than racing this one.
    fn claim_retry(&self, backoff: Duration) -> Result<(), Duration> {
        let mut attempted = self
            .attempted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let wait = backoff.saturating_sub(attempted.elapsed());
        if !wait.is_zero() {
            return Err(wait);
        }
        *attempted = Instant::now();
        Ok(())
    }

    /// This agent, resolved through `client`: its name is its id, and its
    /// endpoint is the configuration's fingerprint rather than the URL, so a
    /// record written before this resolver existed still reads the same.
    fn resolved(&self, client: Arc<A2aClient>) -> Resolved {
        Resolved::new(client, self.name()).with_endpoint(self.agent.endpoint())
    }
}

impl ConfiguredAgents {
    /// Where `@<name>` stands now, without waiting on anything.
    pub(crate) fn standing(&self, name: &str) -> Route {
        let roster = self.snapshot();
        let Some(agent) = roster.get(name) else {
            return roster.absent(name);
        };
        if agent.client.is_some() {
            return Route::Known;
        }
        match agent.retry_in(self.backoff) {
            // Due a retry: the call may go ahead, and reaching it is the
            // first thing it does.
            wait if wait.is_zero() => Route::Known,
            wait => Route::Backoff(wait),
        }
    }

    /// The client `@<name>` is handed to — connecting the agent first when it
    /// is unreachable and its backoff has passed.
    ///
    /// The caller holds the returned `Arc` for its whole exchange; a reload
    /// that swaps the set meanwhile changes what the next call reaches, never
    /// this one (ADR-0031 §6). A retry that succeeds is put into the set, and
    /// the set says it changed, so every agent holding it is taught the agent
    /// is reachable again.
    pub(crate) async fn reach(&self, name: &str) -> Result<Resolved, Unresolved> {
        let roster = self.snapshot();
        let Some(agent) = roster.get(name) else {
            return Err(match roster.absent(name) {
                Route::Refused(reason) => Unresolved::Refused(reason),
                _ => Unresolved::Unknown,
            });
        };
        if let Some(client) = &agent.client {
            return Ok(agent.resolved(Arc::clone(client)));
        }
        agent
            .claim_retry(self.backoff)
            .map_err(Unresolved::Backoff)?;

        let client = A2aClient::connect(agent.agent.config())
            .await
            .map(Arc::new)
            .map_err(|error| Unresolved::Unreachable {
                // mentra's error carries no server text and no header value.
                error: error.to_string(),
                retry_in: Some(self.backoff),
            })?;
        if self.install(agent, &client) {
            self.changed();
        }
        Ok(agent.resolved(client))
    }

    /// Puts `client` into the set for `agent` — unless the set no longer holds
    /// that very entry, because a reload replaced it while this connected: the
    /// reload's own connect is the newer answer. Whether it went in.
    fn install(&self, agent: &Arc<RemoteAgent>, client: &Arc<A2aClient>) -> bool {
        let mut roster = self
            .roster
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(at) = roster.agents.iter().position(|had| Arc::ptr_eq(had, agent)) else {
            return false;
        };
        let mut agents = roster.agents.clone();
        agents[at] = Arc::new(RemoteAgent::new(
            agent.agent.clone(),
            Some(Arc::clone(client)),
        ));
        *roster = Arc::new(Roster {
            agents,
            removed: roster.removed.clone(),
        });
        true
    }
}

impl Roster {
    /// Why a name the set does not hold is not there: refused by name when a
    /// reload took it away (ADR-0031 §5), so the model stops asking for it
    /// rather than guessing at a spelling; unknown otherwise.
    fn absent(&self, name: &str) -> Route {
        if self.removed.contains(name) {
            Route::Refused(format!(
                "the remote agent `{name}` is no longer configured for this workspace"
            ))
        } else {
            Route::Unknown
        }
    }
}

#[cfg(test)]
mod tests;
