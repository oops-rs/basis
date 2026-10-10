//! A secondary sink for a task's events, alongside the durable journal.
//!
//! Every attach writes every event to `events.jsonl` regardless — that is the
//! durable record `watch` and a later `wait` read back. Showing a run *live*,
//! on top of that, is a property of who asked for it rather than of the task
//! itself: a shell blocked on `basis wait` wants to watch, a host polling
//! `--json` wants only the settled object, and a child driven incidentally by
//! its parent's settle pass was not asked for at all. So it is supplied per
//! call, through [`DriveContext`], rather than fixed on [`Tasks`](crate::Tasks).

use std::{sync::Arc, time::Instant};

use serde_json::Value;

use crate::{approve::PromptHost, stop::Stop};

/// Shown a task's events as they are journaled. `basis-cli`'s own terminal
/// renderer is the first implementation, but the trait carries no opinion
/// about a terminal — a host could log, forward over a socket, or update a
/// UI just as well.
pub trait LiveSink: Send + Sync {
    fn on_event(&self, event: &Value);
}

/// What a process attaching to drive a task brings to it: how to show
/// progress live, how to answer `Approve::Prompt`, and how long it is
/// prepared to stay. The first two are independent — a child driven quietly
/// by its parent's settle pass still answers prompts through the same host
/// the parent would, it simply is not shown (see [`hidden`](Self::hidden)).
///
/// The third is the waiter's own deadline, distinct from the task's: a
/// bounded `wait` is a promise to *observe* for so long, never to own the
/// task for that long (README, "Waiting is not owning"). The executor
/// honors it at the turn boundary, the granularity ADR-0019 fixed. A turn in
/// flight when it passes runs to its end under the task's deadline, not the
/// wait's; the next one does not start. A cancel marker, unlike it, cuts the
/// turn in flight short (ADR-0032).
#[derive(Clone, Default)]
pub(crate) struct DriveContext {
    pub(crate) live: Option<Arc<dyn LiveSink>>,
    pub(crate) prompt_host: Option<Arc<dyn PromptHost>>,
    /// When the process driving under this context stops starting turns.
    /// `None` is an attach with no bound of its own — a settle pass driving a
    /// child owes the scope rule a finished subtree, whatever the caller's
    /// patience.
    waiter_deadline: Option<Instant>,
    /// The host's [`Stop`]: once it is flipped, nothing more is driven or
    /// recorded under this context, and the turn in flight is abandoned.
    stop: Option<Stop>,
}

impl DriveContext {
    pub(crate) fn new(
        live: Option<Arc<dyn LiveSink>>,
        prompt_host: Option<Arc<dyn PromptHost>>,
    ) -> Self {
        Self {
            live,
            prompt_host,
            waiter_deadline: None,
            stop: None,
        }
    }

    /// This context, answering to the host's `stop`.
    #[must_use]
    pub(crate) fn stopping_on(self, stop: Option<Stop>) -> Self {
        Self { stop, ..self }
    }

    /// Whether the host this context drives for is going away.
    pub(crate) fn is_stopping(&self) -> bool {
        self.stop.as_ref().is_some_and(Stop::is_stopping)
    }

    /// Resolves once the host is going away; never, for a host with no
    /// [`Stop`].
    pub(crate) async fn stopping(&self) {
        match &self.stop {
            Some(stop) => stop.stopping().await,
            None => std::future::pending().await,
        }
    }

    /// This context, bounded by the waiter's own deadline: no turn starts
    /// past it. `None` waits as long as the task takes.
    #[must_use]
    pub(crate) fn until(self, waiter_deadline: Option<Instant>) -> Self {
        Self {
            waiter_deadline,
            ..self
        }
    }

    /// This context, with progress silenced but the same say over prompts —
    /// what a child driven by its parent's settle pass gets: nobody asked to
    /// watch it, but it is still this process answering for it. The waiter's
    /// deadline is dropped with the sink: the settle pass runs the child to
    /// its record because the parent may not settle before it does
    /// (ADR-0019's ordering constraint), and a child that yielded here would
    /// only be re-attached by the next pass of the same loop. The host's
    /// [`Stop`] is kept: a process going away stops driving children too.
    pub(crate) fn hidden(&self) -> Self {
        Self {
            live: None,
            prompt_host: self.prompt_host.clone(),
            waiter_deadline: None,
            stop: self.stop.clone(),
        }
    }

    /// Whether the waiter this context drives for has run out of patience —
    /// checked once per turn boundary, never mid-turn.
    pub(crate) fn waiter_expired(&self) -> bool {
        self.waiter_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(crate) fn show(&self, event: &Value) {
        if let Some(live) = &self.live {
            live.on_event(event);
        }
    }

    pub(crate) fn can_ask(&self) -> bool {
        self.prompt_host.as_deref().is_some_and(PromptHost::can_ask)
    }

    pub(crate) fn approver(&self) -> Option<Box<dyn basis::Approver>> {
        self.prompt_host.as_deref().map(PromptHost::approver)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::DriveContext;

    #[test]
    fn a_context_without_a_waiter_deadline_never_expires() {
        assert!(!DriveContext::default().waiter_expired());
        assert!(!DriveContext::default().until(None).waiter_expired());
    }

    #[test]
    fn a_waiter_deadline_expires_at_and_after_the_instant() {
        let now = Instant::now();
        assert!(DriveContext::default().until(Some(now)).waiter_expired());
        assert!(
            !DriveContext::default()
                .until(Some(now + Duration::from_secs(3600)))
                .waiter_expired()
        );
    }

    /// A child driven by the settle pass owes the scope rule a finished
    /// subtree; the parent's waiter has no say over that.
    #[test]
    fn hidden_drops_the_waiter_deadline_with_the_sink() {
        let bounded = DriveContext::default().until(Some(Instant::now()));
        assert!(bounded.waiter_expired());
        assert!(!bounded.hidden().waiter_expired());
    }
}
