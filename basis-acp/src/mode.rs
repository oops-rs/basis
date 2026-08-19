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
//! mentra takes a `ToolAuthorizer` when a runtime is built and never hands it
//! back, so a decision made there is fixed for the session's life — which is
//! precisely what a switchable mode cannot be. basis's
//! [`ApprovalGate`](basis::approval::ApprovalGate) therefore surfaces every
//! consequential call without answering any, and the mode decides *here*, where
//! it can still change between one call and the next.
//!
//! That is why [`ModedApprover`] wraps the approver that asks the client rather
//! than replacing it: `Always` and `Never` answer without asking, and `Prompt`
//! asks.
//!
//! # Why basis remembers "for this session" itself
//!
//! mentra can remember a decision — `PermissionDecision::allow_and_remember` —
//! and its rule store is consulted *before* the authorizer runs. A rule stored
//! there would survive a switch to a stricter mode and silently override it:
//! someone who allowed `shell` for the session and then moved to read-only
//! would still be running commands. So [`ModedApprover`] answers mentra with a
//! plain allow or deny and keeps the "…for this session" answer here, where
//! changing the mode clears it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::schema::v1::{SessionMode, SessionModeId, SessionModeState};

use basis::approval::{ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver};

/// What a session does about a call that changes state outside the process.
///
/// The three answers a person at a client can hold an opinion about. Each is a
/// way of answering an [`Approver`]'s question rather than a policy the runtime
/// enforces — see the module docs on where the mode is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    /// Act without asking.
    Always,
    /// Ask the client, every time. The default over ACP: there is a client to
    /// ask, which is the whole reason the protocol carries a permission
    /// request.
    #[default]
    Prompt,
    /// Refuse, so the session can read a workspace and report and cannot touch
    /// it.
    Never,
}

/// Mode ids on the wire. basis chooses them, a client echoes them back, and
/// [`mode_for`] reads them — a contract with ourselves, so it lives in one
/// place.
const ALWAYS: &str = "always";
const PROMPT: &str = "prompt";
const NEVER: &str = "never";

/// The mode an id selects, or `None` for an id basis never offered.
fn mode_for(id: &str) -> Option<ApprovalMode> {
    match id {
        ALWAYS => Some(ApprovalMode::Always),
        PROMPT => Some(ApprovalMode::Prompt),
        NEVER => Some(ApprovalMode::Never),
        _ => None,
    }
}

fn mode_id(mode: ApprovalMode) -> SessionModeId {
    SessionModeId::new(match mode {
        ApprovalMode::Always => ALWAYS,
        ApprovalMode::Prompt => PROMPT,
        ApprovalMode::Never => NEVER,
    })
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
    inner: Arc<Mutex<State>>,
}

struct State {
    current: ApprovalMode,
    /// False when the session was opened read-only. See [`SessionModes::new`].
    switchable: bool,
    /// Tools the client answered "…for this session" about, and how.
    remembered: HashMap<String, bool>,
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
            inner: Arc::new(Mutex::new(State {
                current: initial,
                switchable: !matches!(initial, ApprovalMode::Never),
                remembered: HashMap::new(),
            })),
        }
    }

    pub fn current(&self) -> ApprovalMode {
        self.lock().current
    }

    /// The modes and the current one, as `session/new` and `session/load`
    /// report them.
    pub fn state(&self) -> SessionModeState {
        let state = self.lock();
        let available = if state.switchable {
            vec![
                describe(ApprovalMode::Always),
                describe(ApprovalMode::Prompt),
                describe(ApprovalMode::Never),
            ]
        } else {
            vec![describe(state.current)]
        };

        SessionModeState::new(mode_id(state.current), available)
    }

    /// Switches to `id`, returning the mode now in force.
    ///
    /// Every "…for this session" answer is forgotten: choosing a mode is a
    /// statement about how the rest of the session should behave, and a stale
    /// allow that outlived it would be exactly the override this design exists
    /// to prevent.
    pub fn set(&self, id: &SessionModeId) -> Result<ApprovalMode, ModeError> {
        let mode = mode_for(&id.0).ok_or(ModeError::Unknown)?;

        let mut state = self.lock();
        if !state.switchable && mode != state.current {
            return Err(ModeError::NotOffered);
        }

        state.current = mode;
        state.remembered.clear();
        Ok(mode)
    }

    fn remember(&self, tool_name: &str, allow: bool) {
        self.lock().remembered.insert(tool_name.to_string(), allow);
    }

    fn remembered(&self, tool_name: &str) -> Option<bool> {
        self.lock().remembered.get(tool_name).copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned mode means some other task panicked mid-update. The mode
        // itself is a plain enum and a map; refusing to serve the rest of the
        // session over it would turn one panic into a dead conversation.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Applies the session's mode to each approval request, asking `inner` only
/// when the mode says to ask.
pub struct ModedApprover<A> {
    modes: SessionModes,
    inner: A,
}

impl<A> ModedApprover<A> {
    pub fn new(modes: SessionModes, inner: A) -> Self {
        Self { modes, inner }
    }
}

#[async_trait::async_trait]
impl<A: Approver> Approver for ModedApprover<A> {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        // Read-only calls never reach here: basis's gate allows them
        // outright, because prompting for reads trains people to approve
        // without reading.
        match self.modes.current() {
            ApprovalMode::Always => ApprovalDecision::Allow.into(),
            ApprovalMode::Never => ApprovalAnswer::new(ApprovalDecision::Deny).because(format!(
                "{} changes state outside this process, and this session is set to refuse that",
                request.tool_name
            )),
            ApprovalMode::Prompt => self.ask(request).await,
        }
    }
}

impl<A: Approver> ModedApprover<A> {
    async fn ask(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        if let Some(allow) = self.modes.remembered(&request.tool_name) {
            return if allow {
                ApprovalDecision::Allow.into()
            } else {
                ApprovalAnswer::new(ApprovalDecision::Deny).because(format!(
                    "{} was refused earlier in this session, and that answer still stands",
                    request.tool_name
                ))
            };
        }

        // The two "…for this session" answers are collapsed to a plain one
        // before mentra sees them, and remembered here instead — see the
        // module docs. The reason survives the collapse, because it is what
        // the model reads.
        let answer = self.inner.approve(request).await;
        match answer.decision {
            ApprovalDecision::AllowForSession => {
                self.modes.remember(&request.tool_name, true);
                ApprovalAnswer {
                    decision: ApprovalDecision::Allow,
                    ..answer
                }
            }
            ApprovalDecision::DenyForSession => {
                self.modes.remember(&request.tool_name, false);
                ApprovalAnswer {
                    decision: ApprovalDecision::Deny,
                    ..answer
                }
            }
            _ => answer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts how often it was consulted, so a test can prove a mode answered
    /// without asking.
    struct Counting {
        asked: Arc<AtomicUsize>,
        answer: ApprovalDecision,
    }

    #[async_trait::async_trait]
    impl Approver for Counting {
        async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer.into()
        }
    }

    fn request(tool_name: &str) -> ApprovalRequest {
        ApprovalRequest {
            request_id: "r1".to_string(),
            tool_call_id: "c1".to_string(),
            tool_name: tool_name.to_string(),
            description: "wants to write".to_string(),
            input: json!({}),
        }
    }

    fn gate(
        initial: ApprovalMode,
        answer: ApprovalDecision,
    ) -> (SessionModes, ModedApprover<Counting>, Arc<AtomicUsize>) {
        let modes = SessionModes::new(initial);
        let asked = Arc::new(AtomicUsize::new(0));
        let approver = ModedApprover::new(
            modes.clone(),
            Counting {
                asked: Arc::clone(&asked),
                answer,
            },
        );
        (modes, approver, asked)
    }

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

        assert_eq!(&*state.current_mode_id.0, PROMPT);
        assert_eq!(state.available_modes.len(), 3);
    }

    #[test]
    fn a_read_only_session_offers_nothing_else() {
        let modes = SessionModes::new(ApprovalMode::Never);
        let state = modes.state();

        assert_eq!(state.available_modes.len(), 1);
        assert_eq!(&*state.current_mode_id.0, NEVER);
        assert_eq!(
            modes.set(&SessionModeId::new(ALWAYS)),
            Err(ModeError::NotOffered),
            "a client cannot lift a prohibition it was never given"
        );
    }

    #[test]
    fn switching_reports_the_new_mode() {
        let modes = SessionModes::new(ApprovalMode::Prompt);

        assert_eq!(
            modes.set(&SessionModeId::new(ALWAYS)),
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

    #[tokio::test]
    async fn allow_and_refuse_answer_without_asking() {
        for (mode, expected) in [
            (ApprovalMode::Always, ApprovalDecision::Allow),
            (ApprovalMode::Never, ApprovalDecision::Deny),
        ] {
            let (_modes, mut approver, asked) = gate(mode, ApprovalDecision::Allow);

            assert_eq!(approver.approve(&request("shell")).await.decision, expected);
            assert_eq!(
                asked.load(Ordering::SeqCst),
                0,
                "{mode:?} has nothing to ask about"
            );
        }
    }

    #[tokio::test]
    async fn a_read_only_session_says_so_when_it_refuses() {
        // The model reads this as the tool result, and "denied" on its own
        // would have it try the same write again.
        let (_modes, mut approver, _asked) = gate(ApprovalMode::Never, ApprovalDecision::Allow);

        assert_eq!(
            approver.approve(&request("shell")).await.reason.as_deref(),
            Some(
                "shell changes state outside this process, \
                 and this session is set to refuse that"
            )
        );
    }

    #[tokio::test]
    async fn asking_puts_the_request_to_the_client() {
        let (_modes, mut approver, asked) = gate(ApprovalMode::Prompt, ApprovalDecision::Allow);

        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_answer_for_the_session_is_not_asked_twice() {
        let (_modes, mut approver, asked) =
            gate(ApprovalMode::Prompt, ApprovalDecision::AllowForSession);

        // Collapsed to a plain allow so mentra does not store a rule of its
        // own — the one basis keeps is the one a mode change can clear.
        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1);

        // A different tool was never answered for.
        assert_eq!(
            approver.approve(&request("files")).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn changing_mode_forgets_what_was_allowed_for_the_session() {
        let (modes, mut approver, _asked) =
            gate(ApprovalMode::Prompt, ApprovalDecision::AllowForSession);

        approver.approve(&request("shell")).await;
        modes.set(&SessionModeId::new(NEVER)).expect("switches");

        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Deny,
            "a stale allow must not survive the mode that replaced it"
        );

        // And it stays forgotten on the way back, rather than reappearing.
        modes.set(&SessionModeId::new(PROMPT)).expect("switches");
        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Allow,
            "the client is asked again, and answered again"
        );
    }

    #[tokio::test]
    async fn a_refusal_for_the_session_is_also_remembered() {
        let (_modes, mut approver, asked) =
            gate(ApprovalMode::Prompt, ApprovalDecision::DenyForSession);

        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Deny
        );

        let repeated = approver.approve(&request("shell")).await;
        assert_eq!(repeated.decision, ApprovalDecision::Deny);
        assert_eq!(
            repeated.reason.as_deref(),
            Some("shell was refused earlier in this session, and that answer still stands"),
            "a remembered refusal still owes the model a reason"
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }
}
