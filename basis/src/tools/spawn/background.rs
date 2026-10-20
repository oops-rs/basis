//! A delegation that runs while the model keeps working (ADR-0042).
//!
//! `spawn` with `background: true` starts the work and returns its handle at
//! once — the id the child already has, or the name of the remote agent it
//! went to — instead of blocking the turn on the answer. What happens to the
//! answer is the whole of this module:
//!
//! - **It arrives on its own.** A child that finishes while the model is
//!   doing something else leaves its result with mentra's
//!   [`Deferred`] handle, and the runner puts it in front of the model before
//!   the next request — the same path a background command's notification
//!   takes, carrying the delegation's own record (the result entry and the
//!   child's end) rather than a plain note.
//! - **Or the model asks for it.** `wait: [handles]`, or `@<handle>` alone,
//!   blocks the turn on one or more of them and returns their answers as the
//!   tool result, recorded on the parent there and then. A handle a wait has
//!   claimed is never also delivered on its own: the two paths are
//!   serialized under one lock, see [`Children::settle`].
//! - **Nothing is left behind.** When the model ends its turn with children
//!   still running, [`JoinAtEnd`] — basis's own round strategy, composed in
//!   front of whatever the host set — waits for them, and mentra runs one
//!   more round so the model reads what came back. A turn's children are the
//!   turn's: they inherit its cancellation, stop, deadline and token counter
//!   through `RunOptions::child`, so a cancelled parent cancels them and a
//!   deadline ends them together.
//!
//! The start is eager — a `tokio::spawn`, not a future waiting to be polled —
//! because the point is overlap, and because a remote agent's task runs
//! whether or not anyone here is waiting. What is structured is the scope:
//! the turn owns its children the way a task group owns its tasks, and ends
//! only after they have.
//!
//! # Width
//!
//! Children share their parent's token counter, and each round of each child
//! is checked against the bound only after it has spent. N children running
//! at once can therefore overshoot by N rounds where a blocking child
//! overshoots by one. [`DEFAULT_DELEGATION_WIDTH`] bounds N; a runtime says
//! otherwise through
//! [`RuntimeBuilder::with_delegation_width`](crate::RuntimeBuilder::with_delegation_width).
//! The guard is checked in the preview like the depth floor, so no approver's
//! answer and no remembered rule can lift it.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use mentra::agent::{
    Deferred, DeferredItem, RoundBoundary, RoundContext, RoundDecision, RoundStrategy,
};
use tokio::task::JoinHandle;

use crate::runtime::agents::AgentRegistry;

/// How many delegations one agent may have running in the background at
/// once, absent a
/// [`RuntimeBuilder::with_delegation_width`](crate::RuntimeBuilder::with_delegation_width)
/// override.
///
/// Four is a fan-out — enough to split a review four ways — while keeping the
/// token overshoot the module docs describe to four rounds. A host that wants
/// a wider fan-out raises it and raises the token budget with it.
pub const DEFAULT_DELEGATION_WIDTH: usize = 4;

/// What a background delegation settled to.
pub(super) struct Settled {
    /// The words the model reads: the answer, or why there is none.
    pub(super) outcome: Result<String, String>,
    /// What the parent records about it, in order — the delegation's result
    /// entry, and for a child here that it finished — applied by mentra's
    /// runner when the result arrives on its own, or through the tool
    /// context when a wait collects it.
    pub(super) items: Vec<DeferredItem>,
}

/// What kind of thing a handle names, for the words the model reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Subagent,
    Teammate,
    Remote,
}

impl Kind {
    pub(super) fn noun(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::Teammate => "teammate",
            Self::Remote => "remote agent",
        }
    }
}

/// One running delegation.
struct Slot {
    /// A teammate's or a remote agent's name, so `@<name>` finds it.
    name: Option<String>,
    kind: Kind,
    /// Absent for the instant between reserving the slot and spawning the
    /// task, and after a wait has claimed it.
    task: Option<JoinHandle<Settled>>,
    /// Taken by a wait: the task's own completion then leaves the result to
    /// the waiter instead of delivering it.
    claimed: bool,
}

/// One agent's background delegations, kept on its ledger row for as long as
/// the row lives — which is as long as the run (`runtime::agents`).
#[derive(Clone, Default)]
pub(crate) struct Children {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

impl std::fmt::Debug for Children {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Children")
            .field("running", &self.running())
            .finish()
    }
}

/// A claimed handle: what a wait awaits.
pub(super) struct Claimed {
    pub(super) id: String,
    pub(super) name: Option<String>,
    pub(super) kind: Kind,
    pub(super) task: JoinHandle<Settled>,
}

impl Children {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many are running now, claimed by a wait or not.
    pub(crate) fn running(&self) -> usize {
        self.lock().len()
    }

    /// Every running handle with its name, in no particular order.
    pub(super) fn listed(&self) -> Vec<(String, Option<String>, Kind)> {
        self.lock()
            .iter()
            .map(|(id, slot)| (id.clone(), slot.name.clone(), slot.kind))
            .collect()
    }

    /// The id of the running delegation `key` names — by id, then by name —
    /// or `None`.
    pub(super) fn find(&self, key: &str) -> Option<String> {
        let slots = self.lock();
        if slots.contains_key(key) {
            return Some(key.to_string());
        }
        slots
            .iter()
            .find(|(_, slot)| slot.name.as_deref() == Some(key))
            .map(|(id, _)| id.clone())
    }

    /// Starts `work` under `id`: reserves the slot, spawns the task, and
    /// settles the slot when the task finishes.
    ///
    /// `deferred` is where the result goes when nothing has claimed it by
    /// then. The slot is reserved before the task exists, so a task that
    /// finishes before the handle is in place still finds its slot.
    pub(super) fn start(
        &self,
        id: String,
        name: Option<String>,
        kind: Kind,
        deferred: Deferred,
        work: impl Future<Output = Settled> + Send + 'static,
    ) {
        self.lock().insert(
            id.clone(),
            Slot {
                name,
                kind,
                task: None,
                claimed: false,
            },
        );
        let children = self.clone();
        let settled_id = id.clone();
        let task = tokio::spawn(async move {
            let settled = work.await;
            children.settle(&settled_id, &deferred, settled)
        });
        if let Some(slot) = self.lock().get_mut(&id) {
            slot.task = Some(task);
        }
        // Else the task already settled and removed the slot; the handle is
        // dropped, which detaches a task that is finished anyway.
    }

    /// The task's own last act: deliver the result unless a wait has claimed
    /// it. Under the slot lock, so a claim and a completion are ordered.
    fn settle(&self, id: &str, deferred: &Deferred, settled: Settled) -> Settled {
        let mut slots = self.lock();
        let claimed = slots.get(id).is_some_and(|slot| slot.claimed);
        if claimed {
            // The waiter holds the handle and reads this value from it.
            return settled;
        }
        for item in &settled.items {
            deferred.push(item.clone());
        }
        slots.remove(id);
        settled
    }

    /// Takes the handle under `id` for a wait, or says why it cannot.
    pub(super) fn claim(&self, id: &str) -> Result<Claimed, String> {
        let mut slots = self.lock();
        let Some(slot) = slots.get_mut(id) else {
            return Err(format!("nothing is running under `{id}`"));
        };
        if slot.claimed {
            return Err(format!("`{id}` is already being waited on"));
        }
        let Some(task) = slot.task.take() else {
            return Err(format!("`{id}` has not started yet; try again"));
        };
        slot.claimed = true;
        Ok(Claimed {
            id: id.to_string(),
            name: slot.name.clone(),
            kind: slot.kind,
            task,
        })
    }

    /// Hands a claimed handle back — a wait for *any* that this one did not
    /// win. Returns it instead when the task finished meanwhile, because
    /// that completion found the slot claimed and left the result to us.
    pub(super) fn release(&self, claimed: Claimed) -> Option<Claimed> {
        if claimed.task.is_finished() {
            return Some(claimed);
        }
        let mut slots = self.lock();
        match slots.get_mut(&claimed.id) {
            Some(slot) => {
                slot.task = Some(claimed.task);
                slot.claimed = false;
                None
            }
            // The slot is gone only if something removed it, which nothing
            // does while it is claimed; keep the handle rather than lose it.
            None => Some(claimed),
        }
    }

    /// Removes the slot a wait has collected.
    pub(super) fn finish(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Waits for everything running that no wait has claimed. Each task
    /// delivers its own result as it finishes; this only waits.
    pub(super) async fn join_all(&self) {
        let handles: Vec<JoinHandle<Settled>> = {
            let mut slots = self.lock();
            slots
                .values_mut()
                .filter(|slot| !slot.claimed)
                .filter_map(|slot| slot.task.take())
                .collect()
        };
        for handle in handles {
            // A task that panicked has no result to deliver; the slot it
            // left behind is removed below so the width count is honest.
            let _ = handle.await;
        }
        self.lock()
            .retain(|_, slot| slot.claimed || slot.task.is_some());
    }
}

impl Claimed {
    /// Waits for the task and takes what it settled to.
    pub(super) async fn settled(self) -> (String, Option<String>, Kind, Settled) {
        let settled = match self.task.await {
            Ok(settled) => settled,
            Err(error) => Settled {
                outcome: Err(format!("the delegated run stopped unexpectedly: {error}")),
                items: Vec::new(),
            },
        };
        (self.id, self.name, self.kind, settled)
    }
}

/// basis's own round strategy: at the end of the model's turn, wait for the
/// children it started in the background, so their results reach it before
/// its answer is final.
///
/// Composed in front of the host's strategy, when there is one: the host is
/// asked first and its decision stands; only a `Continue` at the assistant
/// boundary — the run about to return — is held until the children are
/// done. mentra then sees what they left (`Deferred`) and runs one more
/// round. A child's own run gets one of these too, so a grandchild is joined
/// by the child that started it.
pub(crate) struct JoinAtEnd {
    agents: Arc<AgentRegistry>,
    agent_id: String,
    host: Option<Arc<dyn RoundStrategy>>,
}

impl JoinAtEnd {
    pub(crate) fn new(
        agents: Arc<AgentRegistry>,
        agent_id: impl Into<String>,
        host: Option<Arc<dyn RoundStrategy>>,
    ) -> Self {
        Self {
            agents,
            agent_id: agent_id.into(),
            host,
        }
    }

    fn children(&self) -> Option<Children> {
        self.agents
            .of(&self.agent_id)
            .map(|tools| tools.background.clone())
    }
}

#[async_trait]
impl RoundStrategy for JoinAtEnd {
    async fn on_round(&self, ctx: RoundContext<'_>) -> RoundDecision {
        let boundary = ctx.boundary();
        let decision = match &self.host {
            Some(host) => host.on_round(ctx).await,
            None => RoundDecision::proceed(),
        };
        if boundary == RoundBoundary::AssistantMessageCommitted
            && matches!(decision, RoundDecision::Continue(_))
            && let Some(children) = self.children()
        {
            children.join_all().await;
        }
        decision
    }
}

/// The refusal for one delegation too many.
pub(super) fn too_wide(running: usize, width: usize) -> String {
    format!(
        "{running} delegations are already running in the background, and spawn runs no more \
         than {width} at once; wait for one with `wait` before starting another, or do this \
         one here"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settled(text: &str) -> Settled {
        Settled {
            outcome: Ok(text.to_string()),
            items: vec![DeferredItem::Context(text.to_string())],
        }
    }

    /// A provider that is never asked: a `Deferred` needs a runtime, a
    /// runtime needs a provider, and the slot bookkeeping these pin needs
    /// neither. The delivery half is pinned in `tests/background.rs`.
    struct Stub;

    #[async_trait]
    impl mentra::Provider for Stub {
        fn descriptor(&self) -> mentra::provider::ProviderDescriptor {
            mentra::provider::ProviderDescriptor::new("stub")
        }

        async fn list_models(
            &self,
        ) -> Result<Vec<mentra::ModelInfo>, mentra::provider::ProviderError> {
            Ok(vec![mentra::ModelInfo::new("stub-model", "stub")])
        }

        async fn stream(
            &self,
            _request: mentra::provider::Request<'_>,
        ) -> Result<mentra::provider::ProviderEventStream, mentra::provider::ProviderError>
        {
            std::future::pending().await
        }
    }

    fn deferred() -> Deferred {
        mentra::Runtime::empty_builder()
            .with_provider_instance(Stub)
            .build()
            .expect("a runtime with a stub provider builds")
            .deferred("owner")
    }

    #[tokio::test]
    async fn a_claimed_task_is_left_to_its_waiter() {
        let children = Children::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        children.start(
            "child".to_string(),
            None,
            Kind::Subagent,
            deferred(),
            async move {
                let _ = rx.await;
                settled("the answer")
            },
        );
        assert_eq!(children.running(), 1);
        assert_eq!(children.find("child").as_deref(), Some("child"));

        let claimed = children.claim("child").expect("claims");
        assert!(
            children.claim("child").is_err(),
            "a second wait on one handle is refused"
        );
        tx.send(()).expect("the task is waiting");
        let (id, _, _, settled) = claimed.settled().await;
        assert_eq!(id, "child");
        assert_eq!(settled.outcome.as_deref(), Ok("the answer"));
        assert_eq!(
            children.running(),
            1,
            "the waiter removes the slot once it has recorded the result"
        );
        children.finish("child");
        assert_eq!(children.running(), 0);
    }

    #[tokio::test]
    async fn a_released_handle_that_finished_meanwhile_is_handed_back() {
        let children = Children::default();
        children.start(
            "quick".to_string(),
            Some("q".to_string()),
            Kind::Teammate,
            deferred(),
            async { settled("done") },
        );
        let claimed = children.claim("quick").expect("claims");
        // Let the task finish while it is claimed.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let back = children
            .release(claimed)
            .expect("finished meanwhile, so it comes back to be collected");
        let (_, name, kind, settled) = back.settled().await;
        assert_eq!(name.as_deref(), Some("q"));
        assert_eq!(kind, Kind::Teammate);
        assert_eq!(settled.outcome.as_deref(), Ok("done"));
    }

    #[tokio::test]
    async fn join_all_waits_for_what_nobody_claimed() {
        let children = Children::default();
        children.start("a".to_string(), None, Kind::Subagent, deferred(), async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            settled("a done")
        });
        children.join_all().await;
        assert_eq!(
            children.running(),
            0,
            "the task delivered its own result and left"
        );
        assert_eq!(
            too_wide(4, 4),
            "4 delegations are already running in the background, and spawn runs no more than 4 \
             at once; wait for one with `wait` before starting another, or do this one here"
        );
    }
}
