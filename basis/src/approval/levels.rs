//! Carrying a call's side-effect level from the authorizer to the approver.
//!
//! # This whole module is temporary
//!
//! It exists because mentra's `SessionEvent::PermissionRequested` carries five
//! strings and none of them is the classification the authorizer already had in
//! hand: the `ToolAuthorizationPreview` — `side_effect_level`, `capabilities`,
//! `durability` — is read to build the event and then dropped, and even the
//! event's `preview` field is the tool's `structured_input` rather than the
//! preview it is named for. So the one place in this process that *knows* a
//! call reaches the network is [`ApprovalGate`](super::ApprovalGate), and the
//! one place that needs to know is the forwarder that builds the
//! [`ApprovalRequest`](super::ApprovalRequest) — and there is no line between
//! them.
//!
//! The fix belongs upstream and is filed there:
//! <https://github.com/oops-rs/mentra/issues/21> — put the classification on the
//! event. **When that lands, delete this file**, take the level off the event
//! instead, and with it go [`SideEffectLevels`], `ApprovalGate::levels`,
//! `PreparedRun::with_side_effect_levels`, and the `Runtime` field that carries
//! the handle from the one to the other. Nothing else depends on it:
//! `ApprovalRequest::side_effect_level` stays exactly as it is, because
//! `Option` is also the honest shape for a fact the event may not carry.
//!
//! Carried at all only under ADR-0005's one exemption — "basis may carry a
//! temporary workaround only with a linked mentra issue and a removal note" —
//! and the alternative was worse: without it, the policy
//! [`Approver`](super::Approver)'s own module doc advertises ("allow edits but
//! deny the network") is unwritable except by re-deriving levels from tool
//! names, which is what a production host had to do.
//!
//! # Why this is sound rather than a race
//!
//! mentra's `SessionToolAuthorizer` calls the gate, and *only if the gate
//! answers `Prompt`* does it emit `PermissionRequested` and block the turn
//! (`session/permission.rs`). So the gate's write strictly happens-before the
//! event exists at all, and the forwarder's read comes after receiving it. The
//! mutex publishes it across the two tasks.
//!
//! The channel can still honestly miss — a host that never wired the handle
//! through, an entry evicted by the cap below, two live calls sharing one id —
//! and every miss reads as `None`, which the approver is told to treat as
//! unknown rather than harmless.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use mentra::tool::ToolSideEffectLevel;

/// How many unanswered levels one runtime remembers.
///
/// A bound is needed because not every recorded level is taken. A request the
/// forwarder never sees — its event dropped by a lagging broadcast receiver, or
/// its turn cancelled while mentra was still blocked on the answer — leaves an
/// entry nobody comes for, and a runtime that lives as long as the process
/// would accumulate them. Two hundred and fifty-six is far past the number of
/// approvals that can be in flight at once (each one blocks a turn), so
/// evicting the oldest can only reach entries that were already stranded — and
/// an entry evicted while still live reads as `None`, which fails closed.
const CAPACITY: usize = 256;

/// The side channel itself: a handle both the authorizer and the forwarder
/// hold.
///
/// Cheap to clone and safe to share — one per [`Runtime`](crate::Runtime),
/// which means one across every workspace and every concurrent run on it. That
/// sharing is why entries are keyed by `tool_call_id`: it is the only field
/// mentra's authorization request and its permission event have in common (the
/// request also carries `agent_id`; the event carries nothing that identifies a
/// run). Two calls that nevertheless answer to one id are both reported as
/// unknown rather than one of them being guessed at.
///
/// **Interim.** See this module's documentation and mentra#21.
#[derive(Debug, Clone, Default)]
pub struct SideEffectLevels {
    entries: Arc<Mutex<VecDeque<Entry>>>,
}

/// One recorded classification, waiting to be taken.
#[derive(Debug)]
struct Entry {
    tool_call_id: String,
    /// `None` once a second call claimed the same id: with two answers and no
    /// way to tell which belongs to the request being built, the honest report
    /// is that the level is unknown.
    level: Option<ToolSideEffectLevel>,
}

impl SideEffectLevels {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many levels are recorded and not yet taken.
    ///
    /// Zero between runs is the healthy state, and the reason this is public:
    /// a resolved request takes its entry, so a number that only grows is the
    /// visible symptom of requests going unanswered. Bounded either way, so a
    /// runtime that lives as long as the process cannot grow one here.
    pub fn pending(&self) -> usize {
        self.entries.lock().expect(POISONED).len()
    }

    /// Writes down what the authorizer knows, for the forwarder to take.
    ///
    /// Called only on the path that raises a permission request. A call the
    /// gate allows outright never reaches an approver, so an entry for one
    /// would be litter with nobody coming for it.
    ///
    /// A `tool_call_id` already recorded is *blanked* rather than overwritten.
    /// Ids come from the provider, so two runs sharing this runtime can collide
    /// — and picking either answer would risk reporting a call that reaches the
    /// network as one that only edits a file, which is the one direction this
    /// must never be wrong in. Both reads then get `None`.
    pub(crate) fn record(&self, tool_call_id: &str, level: ToolSideEffectLevel) {
        let mut entries = self.entries.lock().expect(POISONED);

        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.tool_call_id == tool_call_id)
        {
            existing.level = None;
            return;
        }

        if entries.len() >= CAPACITY {
            entries.pop_front();
        }

        entries.push_back(Entry {
            tool_call_id: tool_call_id.to_string(),
            level: Some(level),
        });
    }

    /// Takes the level recorded for `tool_call_id`, if one still is.
    ///
    /// Removing rather than reading, because a permission request is resolved
    /// exactly once: leaving the entry behind would turn every answered request
    /// into a leak, and there is no later reader to leave it for.
    ///
    /// Scanned rather than looked up. The list is capped, and this runs once
    /// per consequential tool call — a human-scale event on the far side of a
    /// model round trip — so insertion order comes free instead of costing a
    /// second structure to keep in step with the first.
    pub(crate) fn take(&self, tool_call_id: &str) -> Option<ToolSideEffectLevel> {
        let mut entries = self.entries.lock().expect(POISONED);
        let index = entries
            .iter()
            .position(|entry| entry.tool_call_id == tool_call_id)?;

        entries.remove(index).and_then(|entry| entry.level)
    }
}

const POISONED: &str = "side-effect level channel poisoned";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_level_comes_back_once_and_then_is_gone() {
        // The whole contract in one line: a request is resolved exactly once,
        // so a second taker finding the entry still there would mean every
        // answered request leaks one.
        let levels = SideEffectLevels::new();
        levels.record("call-1", ToolSideEffectLevel::External);

        assert_eq!(levels.take("call-1"), Some(ToolSideEffectLevel::External));
        assert_eq!(levels.take("call-1"), None);
        assert_eq!(levels.pending(), 0);
    }

    #[test]
    fn a_level_nobody_recorded_is_unknown_rather_than_an_error() {
        // What a host that never wired the handle through gets, on every call.
        // It has to be an ordinary answer: the run continues either way, and
        // the approver decides what unknown means.
        assert_eq!(SideEffectLevels::new().take("call-1"), None);
    }

    #[test]
    fn two_calls_sharing_one_id_are_reported_as_unknown_rather_than_as_a_guess() {
        // Provider-assigned ids on a runtime shared by concurrent runs. With
        // two answers and no way to tell which request is being built, keeping
        // either could report a call that reaches the network as one that only
        // edits a file — so neither is kept.
        let levels = SideEffectLevels::new();
        levels.record("call-1", ToolSideEffectLevel::LocalState);
        levels.record("call-1", ToolSideEffectLevel::External);

        assert_eq!(levels.take("call-1"), None);
        assert_eq!(levels.take("call-1"), None, "and the entry is still taken");
    }

    #[test]
    fn levels_nobody_ever_takes_cannot_grow_without_bound() {
        // Requests whose events were dropped by a lagging receiver, or whose
        // turn was cancelled mid-wait. A process-lifetime runtime would
        // otherwise accumulate one entry per stranded request forever.
        let levels = SideEffectLevels::new();
        for index in 0..CAPACITY * 3 {
            levels.record(&format!("call-{index}"), ToolSideEffectLevel::Process);
        }

        assert_eq!(levels.pending(), CAPACITY);
        assert_eq!(
            levels.take("call-0"),
            None,
            "the oldest stranded entry is the one evicted"
        );
        assert_eq!(
            levels.take(&format!("call-{}", CAPACITY * 3 - 1)),
            Some(ToolSideEffectLevel::Process),
            "and the newest is still there to be taken"
        );
    }

    #[test]
    fn every_holder_of_the_handle_sees_one_channel() {
        // What makes the gate and the forwarder able to be at opposite ends of
        // it: they hold clones, not copies.
        let levels = SideEffectLevels::new();
        let forwarder = levels.clone();

        levels.record("call-1", ToolSideEffectLevel::LocalState);

        assert_eq!(
            forwarder.take("call-1"),
            Some(ToolSideEffectLevel::LocalState)
        );
        assert_eq!(levels.pending(), 0, "and taking from one empties both");
    }
}
