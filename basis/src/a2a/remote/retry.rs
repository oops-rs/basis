//! Reaching one agent by name, retrying an unreachable one when it is asked
//! for (ADR-0031 §2).
//!
//! Two questions, because `spawn` asks twice. Its preview runs before the
//! approver and cannot wait on the network, so [`RemoteAgents::route`] answers
//! from the set alone: connected, due a retry, or not worth asking about yet.
//! Its execution can wait, so [`RemoteAgents::reach`] hands back the client to
//! use — connecting an unreachable agent first when its backoff has passed.
//! The claim on a retry is taken under the agent's own lock, so two calls
//! racing for one down agent cost one connect attempt, not two.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use mentra::a2a::{A2aClient, A2aError as ClientError};

use super::{RemoteAgent, RemoteAgents, Roster};

/// Where `@<name>` stands, decided without waiting on anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// Connected: the task can be handed over.
    Connected,
    /// Configured and unreachable, and due a retry: the call may go ahead,
    /// and connecting is the first thing it does.
    Retry,
    /// Configured and unreachable, and tried too recently to try again for
    /// this long.
    Backoff(Duration),
    /// Not configured.
    Unknown,
}

/// Why [`RemoteAgents::reach`] has no client to hand back.
#[derive(Debug)]
pub(crate) enum Unreached {
    /// Tried too recently; tried again after this long.
    Backoff(Duration),
    /// Tried just now, and the card could not be fetched; tried again after
    /// `retry_in`.
    Failed {
        error: ClientError,
        retry_in: Duration,
    },
    /// Not configured.
    Unknown,
}

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
}

impl RemoteAgents {
    /// Where `@<name>` stands now.
    pub(crate) fn route(&self, name: &str) -> Route {
        let roster = self.snapshot();
        let Some(agent) = roster.get(name) else {
            return Route::Unknown;
        };
        if agent.client.is_some() {
            return Route::Connected;
        }
        match agent.retry_in(self.backoff) {
            wait if wait.is_zero() => Route::Retry,
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
    pub(crate) async fn reach(&self, name: &str) -> Result<Arc<A2aClient>, Unreached> {
        let roster = self.snapshot();
        let Some(agent) = roster.get(name) else {
            return Err(Unreached::Unknown);
        };
        if let Some(client) = &agent.client {
            return Ok(Arc::clone(client));
        }
        agent
            .claim_retry(self.backoff)
            .map_err(Unreached::Backoff)?;

        let client = A2aClient::connect(agent.agent.config())
            .await
            .map(Arc::new)
            .map_err(|error| Unreached::Failed {
                error,
                retry_in: self.backoff,
            })?;
        if self.install(agent, &client) {
            self.changed();
        }
        Ok(client)
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
        *roster = Arc::new(Roster { agents });
        true
    }
}

#[cfg(test)]
mod tests;
