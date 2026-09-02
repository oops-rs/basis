//! The session's permission mode, and the client's switch for it.
//!
//! ACP lets an agent name its own modes and lets a client change them mid
//! session. basis offers three — [`ApprovalMode`]: always allow, ask, refuse —
//! spelled with the same three words as `basis --approve`, so the protocol and
//! the command line name one set of things.
//!
//! # Why the modes live here and not in the core
//!
//! basis has no such enum. Approval there is the
//! [`Approver`] trait alone (ADR-0010): "always allow" is
//! [`AllowAll`](basis::AllowAll), "refuse" is [`DenyAll`](basis::DenyAll),
//! and "ask" is whatever the host installs. What ACP needs that a trait cannot
//! give it is an *enumerable* set — `session/new` reports every available mode
//! with an id, a name and a description, and the client picks one by id. That
//! list is a protocol binding, so it belongs with the protocol.
//!
//! # Where the mode is applied, and why not on the authorizer
//!
//! mentra 0.26 samples a session's current authorizer once per call and
//! treats its `Allow` and `Deny` as final — it could even host a stateful or
//! live-swapped one (`Session::with_tool_authorizer`). basis still does not
//! put the mode there, deliberately: the
//! [`ApprovalGate`](basis::approval::ApprovalGate) is one fixed, stateless
//! surface that answers nothing, so every consequential call is surfaced as
//! a `Prompt` — and the mode decides *here*, beside the protocol session the
//! client's `session/set_mode` actually arrives on, where it can change
//! between one call and the next without reaching two layers down.
//!
//! That is why [`ModedApprover`] wraps the approver that asks the client rather
//! than replacing it: `Always` and `Never` answer without asking, and `Prompt`
//! asks.
//!
//! # Why basis remembers "for this session" itself
//!
//! mentra can remember a decision — `PermissionDecision::allow_and_remember` —
//! and since 0.26 its remembered rules resolve the gate's `Prompt` *before*
//! the approver is consulted, from a store persisted under the conversation's
//! stable agent id. A rule stored there answers ahead of the mode on every
//! later call in the live session: someone who allowed `shell` for the
//! session and then moved to read-only would still be running commands. So
//! [`ModedApprover`] answers mentra with a plain allow or deny and keeps the
//! "…for this session" answer here, in process memory, where changing the
//! mode clears it — which is also why, on a stock basis-acp session, the
//! approver really does see every surfaced call: this layer never writes a
//! rule for mentra to answer from.
//!
//! # The bypass a seeded durable rule is
//!
//! That guarantee is about what *this layer writes*, not about the pipeline.
//! A host that seeds a **Global- or Project-scope** rule through the
//! session's permission handle (the seam `basis`'s `reviewed_shell` example
//! teaches, at Session scope) has installed an answer that resolves the
//! gate's `Prompt` with no `PermissionRequested` ever emitted — so the mode
//! is never consulted, and the rule survives everything this module relies
//! on: it outlives every mode switch (it is not in [`SessionApproval`]'s
//! memory) and outlives the attach-time clear too (that clear is
//! session-scope only). A seeded durable allow on a store whose sessions
//! offer a read-only mode is therefore a standing override of that mode.
//! The sound fix path is upstream and adopted later: mentra 0.26's
//! session-scoped authorizer replacement (mentra#38) over its revocable,
//! scope-addressed rules (mentra#43) would let a read-only session install
//! an authorizer whose `Deny` is final over any remembered rule. Until that
//! wave lands, do not seed durable allows on stores serving mode-switchable
//! sessions.
//!
//! # A request already put to the client stays put
//!
//! `session/set_mode` may arrive while a permission dialog is open. The mode
//! that was in force when the request was *put* decides that request; the new
//! mode governs the next one. [`ModedApprover`] reads the mode once, before
//! asking, and never again for the same call, and that is deliberate rather
//! than incidental:
//!
//! - The dialog is on screen, and ACP gives an agent no way to take it down.
//!   Answering the call from the new mode would leave a person looking at a
//!   question whose answer no longer matters, and whatever they then click
//!   would be discarded — which is a worse surprise than the one it avoids.
//! - The person at the client holds both controls. Someone who switches to
//!   read-only with a dialog open can refuse in the dialog; someone who
//!   switches to always-allow can allow in it. Nothing is lost by letting the
//!   dialog have the last word on the call it is about.
//! - A "…for this session" answer that lands after the switch is remembered
//!   like any other, and cannot outlive the switch either: it is only ever
//!   read under `Prompt`, and moving back to `Prompt` is itself a switch,
//!   which clears it.
//!
//! `tests/acp/permission.rs` pins both directions.

use agent_client_protocol::schema::v1::{SessionMode, SessionModeId, SessionModeState};
use basis_host::SessionApproval;

pub use basis_host::{ApprovalPolicy as ApprovalMode, PolicyApprover as ModedApprover};

/// Mode ids on the wire. basis chooses them, a client echoes them back, and
/// [`mode_for`] reads them — a contract with ourselves, so it lives in one
/// place.
/// The mode an id selects, or `None` for an id basis never offered.
fn mode_for(id: &str) -> Option<ApprovalMode> {
    ApprovalMode::from_type_tag(id)
}

fn mode_id(mode: ApprovalMode) -> SessionModeId {
    SessionModeId::new(mode.type_tag())
}

/// How each mode is described in a client's picker.
fn describe(mode: ApprovalMode) -> SessionMode {
    let (name, description) = match mode {
        ApprovalMode::Always => (
            "Always allow",
            "Act without asking. What a confined or unattended session wants.",
        ),
        ApprovalMode::Prompt => (
            "Ask each time",
            "Ask before anything that changes state outside this process.",
        ),
        ApprovalMode::Never => (
            "Read only",
            "Refuse anything that changes state outside this process.",
        ),
    };

    SessionMode::new(mode_id(mode), name).description(description)
}

/// Why a `session/set_mode` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeError {
    /// An id basis never offered.
    Unknown,
    /// A real mode, but not one this session may move to.
    NotOffered,
}

impl std::fmt::Display for ModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown mode"),
            Self::NotOffered => {
                f.write_str("this session was opened read-only and cannot change mode")
            }
        }
    }
}

/// One session's mode, and what the client has already answered under it.
///
/// Cloneable and shared: the dispatch loop sets it while a spawned turn reads
/// it. The lock is sync and never held across an await, for the same reason
/// the cancellation token's is — `session/set_mode` arrives *during* a turn,
/// and ACP says explicitly that it may.
#[derive(Clone)]
pub struct SessionModes {
    approval: SessionApproval,
    /// False when the session was opened read-only. See [`SessionModes::new`].
    switchable: bool,
}

impl SessionModes {
    /// Opens a session at `initial`.
    ///
    /// A session that starts at [`ApprovalMode::Never`] offers no other mode.
    /// The other two both permit consequential work and differ only in
    /// ceremony, so moving between them is the person at the client changing
    /// their mind — the same authority they already exercise by answering a
    /// permission request. `Never` is a prohibition the operator set outside
    /// the protocol, and a client cannot lift what it was never given.
    pub fn new(initial: ApprovalMode) -> Self {
        Self {
            approval: SessionApproval::new(initial),
            switchable: !matches!(initial, ApprovalMode::Never),
        }
    }

    pub fn current(&self) -> ApprovalMode {
        self.approval.current()
    }

    pub(crate) fn approval(&self) -> &SessionApproval {
        &self.approval
    }

    /// The modes and the current one, as `session/new` and `session/load`
    /// report them.
    pub fn state(&self) -> SessionModeState {
        let current = self.current();
        let available = if self.switchable {
            vec![
                describe(ApprovalMode::Always),
                describe(ApprovalMode::Prompt),
                describe(ApprovalMode::Never),
            ]
        } else {
            vec![describe(current)]
        };

        SessionModeState::new(mode_id(current), available)
    }

    /// Switches to `id`, returning the mode now in force.
    ///
    /// Every "…for this session" answer is forgotten: choosing a mode is a
    /// statement about how the rest of the session should behave, and a stale
    /// allow that outlived it would be exactly the override this design exists
    /// to prevent.
    pub fn set(&self, id: &SessionModeId) -> Result<ApprovalMode, ModeError> {
        let mode = mode_for(&id.0).ok_or(ModeError::Unknown)?;

        if !self.switchable && mode != self.current() {
            return Err(ModeError::NotOffered);
        }

        self.approval.set(mode);
        Ok(mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_mode_maps_back_to_one_lan_can_read() {
        // An id basis sends but cannot read would be a switch that silently does
        // nothing.
        for mode in SessionModes::new(ApprovalMode::Prompt)
            .state()
            .available_modes
        {
            assert!(
                mode_for(&mode.id.0).is_some(),
                "offered {} but cannot read it back",
                mode.id.0
            );
        }
    }

    #[test]
    fn the_state_reports_the_current_mode_and_all_three() {
        let state = SessionModes::new(ApprovalMode::Prompt).state();

        assert_eq!(&*state.current_mode_id.0, ApprovalMode::Prompt.type_tag());
        assert_eq!(state.available_modes.len(), 3);
    }

    #[test]
    fn a_read_only_session_offers_nothing_else() {
        let modes = SessionModes::new(ApprovalMode::Never);
        let state = modes.state();

        assert_eq!(state.available_modes.len(), 1);
        assert_eq!(&*state.current_mode_id.0, ApprovalMode::Never.type_tag());
        assert_eq!(
            modes.set(&SessionModeId::new(ApprovalMode::Always.type_tag())),
            Err(ModeError::NotOffered),
            "a client cannot lift a prohibition it was never given"
        );
    }

    #[test]
    fn switching_reports_the_new_mode() {
        let modes = SessionModes::new(ApprovalMode::Prompt);

        assert_eq!(
            modes.set(&SessionModeId::new(ApprovalMode::Always.type_tag())),
            Ok(ApprovalMode::Always)
        );
        assert_eq!(modes.current(), ApprovalMode::Always);
    }

    #[test]
    fn an_unknown_mode_is_refused() {
        let modes = SessionModes::new(ApprovalMode::Prompt);

        assert_eq!(
            modes.set(&SessionModeId::new("architect")),
            Err(ModeError::Unknown)
        );
        assert_eq!(
            modes.current(),
            ApprovalMode::Prompt,
            "a refused switch must leave the session where it was"
        );
    }
}
