//! Shared approval-mode machinery for long-lived hosts.
//!
//! basis itself deliberately exports the [`Approver`] trait and nothing more:
//! "always allow" and "refuse" are ordinary approvers, and "ask" is whatever
//! host is in charge of the run. A long-lived frontend host needs one more
//! thing, though: a switchable three-mode policy over that trait, plus
//! session-scoped remembered answers that clear when the policy changes.
//!
//! The two invariants are the ones basis-acp had to get right first and every
//! later frontend would otherwise rebuild:
//!
//! - A request already put to the client is decided by the policy that was in
//!   force when it was put. [`PolicyApprover`] therefore reads the policy once
//!   per request, before asking, and never again for that request.
//! - Every "for this session" answer is forgotten on any policy switch. A stale
//!   remembered allow surviving a switch to a stricter mode would be a silent
//!   override of the very policy switch the host just made.
//!
//! Both invariants live *here*, in process memory, which is what makes them
//! enforceable: [`PolicyApprover`] keeps the "…for this session" answer in
//! [`SessionApproval`] and hands basis a plain allow or deny, so nothing
//! reaches mentra's persisted rule store and a policy switch can genuinely
//! clear everything. A host that passes the raw `…ForSession` decisions
//! through instead gets basis's own duration for them — remembered on the
//! live session, cleared at the next attach — which is documented on
//! `basis::ApprovalDecision`.
//!
//! [`PolicyGate`] is the third piece, and the one that is not about what this
//! layer writes. Both invariants above are enforced on the *approver*, and an
//! approver only ever sees what the runtime gate surfaced and no remembered
//! rule already answered. That is fine for a policy that decides between
//! asking and allowing, and not fine for one that refuses: a durable rule
//! seeded on the store would answer ahead of it. So
//! [`ApprovalPolicy::Never`]'s refusal is stated as an authorizer, where
//! mentra treats it as final.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use basis::approval::{
    ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, RuntimeError,
    ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer, is_consequential,
};

/// What a host does about a consequential call.
///
/// The same three answers basis-acp, basis-tasks, and the CLI had each been
/// spelling for themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicy {
    /// Act without asking.
    Always,
    /// Ask whichever host is answering for this session.
    #[default]
    Prompt,
    /// Refuse anything that changes state outside the process.
    Never,
}

impl ApprovalPolicy {
    /// The stable lowercase identifier shared by persisted task metadata,
    /// protocol adapters, and command-line parsers.
    pub const fn type_tag(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Prompt => "prompt",
            Self::Never => "never",
        }
    }

    /// Reads a stable lowercase identifier.
    pub fn from_type_tag(tag: &str) -> Option<Self> {
        match tag {
            "always" => Some(Self::Always),
            "prompt" => Some(Self::Prompt),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// One session's active approval policy and its remembered answers.
///
/// Cloneable and shared: a dispatch loop may switch the mode while a spawned
/// turn reads it. The lock is sync and never held across an await.
#[derive(Clone)]
pub struct SessionApproval {
    inner: Arc<Mutex<State>>,
}

struct State {
    current: ApprovalPolicy,
    remembered: HashMap<String, bool>,
}

impl SessionApproval {
    /// Opens a session at `initial`.
    pub fn new(initial: ApprovalPolicy) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                current: initial,
                remembered: HashMap::new(),
            })),
        }
    }

    pub fn current(&self) -> ApprovalPolicy {
        self.lock().current
    }

    /// Sets the policy and forgets every session-scoped answer.
    pub fn set(&self, policy: ApprovalPolicy) {
        let mut state = self.lock();
        state.current = policy;
        state.remembered.clear();
    }

    fn remember(&self, tool_name: &str, allow: bool) {
        self.lock().remembered.insert(tool_name.to_string(), allow);
    }

    fn remembered(&self, tool_name: &str) -> Option<bool> {
        self.lock().remembered.get(tool_name).copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Why a read-only session refused, in the words the model reads.
///
/// One function because two layers now refuse for the same reason —
/// [`PolicyGate`] before the call is surfaced at all, [`PolicyApprover`] for a
/// call some other authorizer surfaced anyway — and a model that got two
/// different explanations of one prohibition would be reading a difference
/// that is not there.
fn read_only_refusal(tool_name: &str) -> String {
    format!(
        "{tool_name} changes state outside this process, and this session is set to refuse that"
    )
}

/// The tool authorizer a long-lived host installs on each of its sessions:
/// the same filter basis's [`ApprovalGate`](basis::ApprovalGate) applies, plus
/// the one answer that has to be terminal.
///
/// # Why the refusal is here and the rest is not
///
/// `ApprovalGate` answers nothing — every consequential call comes back as a
/// `Prompt` — and that is what lets the policy be read per call, one layer up,
/// where `session/set_mode` arrives. mentra resolves a `Prompt` against the
/// conversation's remembered rules *before* the approver is consulted, so a
/// durable Global- or Project-scope allow seeded through the session's
/// permission handle answers ahead of the policy. For [`Always`] and
/// [`Prompt`] that is unremarkable: both permit consequential work, and a
/// standing allow is a host saying so in advance. For [`Never`] it is a
/// standing override of the one thing that mode promises.
///
/// So this gate answers [`Never`] itself, with a `Deny` mentra returns
/// unchanged — no rule is read, no `PermissionRequested` is emitted, and the
/// approver is never reached. Every other policy still surfaces the call
/// exactly as `ApprovalGate` did, remembered rules and all.
///
/// The policy is read from the shared [`SessionApproval`] on each call, so one
/// installed gate follows a session across every switch. It is deliberately
/// read *per call* rather than per turn: a gate that cached the policy it was
/// installed under would be the stale answer this whole module exists to
/// prevent.
///
/// # What it does not close
///
/// A durable rule still answers ahead of the client under [`Always`] and
/// [`Prompt`], because under those policies the gate still says `Prompt`.
/// Revoking one is mentra's business
/// ([mentra#43](https://github.com/oops-rs/mentra/issues/43)), not something a
/// posture can express. And a rule remembered against a *read-only* tool is
/// never consulted under any policy — this gate allows a non-consequential
/// call outright, exactly as `ApprovalGate` does, for the reason written on
/// [`basis::approval::is_consequential`].
///
/// [`Always`]: ApprovalPolicy::Always
/// [`Prompt`]: ApprovalPolicy::Prompt
/// [`Never`]: ApprovalPolicy::Never
/// Cloneable for the same reason [`SessionApproval`] is, and not `Debug` for
/// the same reason either: what it holds is one session's live policy, shared.
#[derive(Clone)]
pub struct PolicyGate {
    approval: SessionApproval,
    timeout: Option<Duration>,
}

impl PolicyGate {
    /// Gates a session on `approval`, reading it live.
    pub fn new(approval: SessionApproval) -> Self {
        Self {
            // No timeout, matching `ApprovalGate`: a person reading a diff
            // should not lose the turn to a stopwatch.
            approval,
            timeout: None,
        }
    }

    /// Gives up on an unanswered request after `timeout`, denying the call.
    ///
    /// The knob [`ApprovalGate`](basis::ApprovalGate) carries, kept because
    /// installing this gate *replaces* the runtime's rather than layering over
    /// it — a host that had bounded its own wait would otherwise lose the
    /// bound without being told.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
            ..self
        }
    }
}

#[async_trait::async_trait]
impl ToolAuthorizer for PolicyGate {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        if !is_consequential(request.preview.side_effect_level) {
            return Ok(ToolAuthorizationDecision::allow());
        }

        Ok(match self.approval.current() {
            ApprovalPolicy::Never => {
                ToolAuthorizationDecision::deny(read_only_refusal(&request.tool_name))
            }
            // Unchanged from `ApprovalGate`, wording included: the reason
            // becomes the description whoever is answering shows a person.
            ApprovalPolicy::Always | ApprovalPolicy::Prompt => {
                ToolAuthorizationDecision::prompt(format!(
                    "{} wants to run and can change state outside this process",
                    request.tool_name
                ))
            }
        })
    }

    fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}

/// Applies a session's policy to each approval request, asking `inner` only
/// when the policy says to ask.
pub struct PolicyApprover<A> {
    approval: SessionApproval,
    inner: A,
}

impl<A> PolicyApprover<A> {
    pub fn new(approval: SessionApproval, inner: A) -> Self {
        Self { approval, inner }
    }
}

#[async_trait::async_trait]
impl<A: Approver> Approver for PolicyApprover<A> {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        match self.approval.current() {
            ApprovalPolicy::Always => ApprovalDecision::Allow.into(),
            // Reached only when something other than [`PolicyGate`] surfaced
            // the call — a source that installed its own authorizer, or a run
            // driven without one. The gate refuses first where it is installed.
            ApprovalPolicy::Never => ApprovalAnswer::new(ApprovalDecision::Deny)
                .because(read_only_refusal(&request.tool_name)),
            ApprovalPolicy::Prompt => self.ask(request).await,
        }
    }
}

impl<A: Approver> PolicyApprover<A> {
    async fn ask(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        if let Some(allow) = self.approval.remembered(&request.tool_name) {
            return if allow {
                ApprovalDecision::Allow.into()
            } else {
                ApprovalAnswer::new(ApprovalDecision::Deny).because(format!(
                    "{} was refused earlier in this session, and that answer still stands",
                    request.tool_name
                ))
            };
        }

        let answer = self.inner.approve(request).await;
        match answer.decision {
            ApprovalDecision::AllowForSession => {
                self.approval.remember(&request.tool_name, true);
                ApprovalAnswer {
                    decision: ApprovalDecision::Allow,
                    ..answer
                }
            }
            ApprovalDecision::DenyForSession => {
                self.approval.remember(&request.tool_name, false);
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
    use basis::ToolSideEffectLevel;
    use mentra::tool::{
        ToolApprovalCategory, ToolAuthorizationOutcome, ToolAuthorizationPreview, ToolCapability,
        ToolDurability, ToolExecutionCategory,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
            side_effect_level: Some(ToolSideEffectLevel::LocalState),
        }
    }

    fn gate(
        initial: ApprovalPolicy,
        answer: ApprovalDecision,
    ) -> (SessionApproval, PolicyApprover<Counting>, Arc<AtomicUsize>) {
        let approval = SessionApproval::new(initial);
        let asked = Arc::new(AtomicUsize::new(0));
        let approver = PolicyApprover::new(
            approval.clone(),
            Counting {
                asked: Arc::clone(&asked),
                answer,
            },
        );
        (approval, approver, asked)
    }

    #[test]
    fn the_tags_round_trip() {
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::Prompt);

        for policy in [
            ApprovalPolicy::Always,
            ApprovalPolicy::Prompt,
            ApprovalPolicy::Never,
        ] {
            let tag = policy.type_tag();
            assert_eq!(ApprovalPolicy::from_type_tag(tag), Some(policy));
            assert_eq!(serde_json::to_value(policy).unwrap(), json!(tag));
            assert_eq!(
                serde_json::from_value::<ApprovalPolicy>(json!(tag)).unwrap(),
                policy
            );
        }
        assert_eq!(ApprovalPolicy::from_type_tag("architect"), None);
    }

    #[tokio::test]
    async fn allow_and_refuse_answer_without_asking() {
        for (mode, expected) in [
            (ApprovalPolicy::Always, ApprovalDecision::Allow),
            (ApprovalPolicy::Never, ApprovalDecision::Deny),
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
        let (_approval, mut approver, _asked) =
            gate(ApprovalPolicy::Never, ApprovalDecision::Allow);

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
        let (_approval, mut approver, asked) =
            gate(ApprovalPolicy::Prompt, ApprovalDecision::Allow);

        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_answer_for_the_session_is_not_asked_twice() {
        let (_approval, mut approver, asked) =
            gate(ApprovalPolicy::Prompt, ApprovalDecision::AllowForSession);

        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1);

        assert_eq!(
            approver.approve(&request("files")).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn changing_mode_forgets_what_was_allowed_for_the_session() {
        let (approval, mut approver, _asked) =
            gate(ApprovalPolicy::Prompt, ApprovalDecision::AllowForSession);

        approver.approve(&request("shell")).await;
        approval.set(ApprovalPolicy::Never);

        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Deny,
            "a stale allow must not survive the mode that replaced it"
        );

        approval.set(ApprovalPolicy::Prompt);
        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Allow,
            "moving back to prompt must ask again rather than reviving a stale rule"
        );
    }

    fn authorization(tool_name: &str, level: ToolSideEffectLevel) -> ToolAuthorizationRequest {
        ToolAuthorizationRequest {
            agent_id: "a1".to_string(),
            agent_name: "test".to_string(),
            model: "m".to_string(),
            history_len: 1,
            tool_call_id: "tc-1".to_string(),
            tool_name: tool_name.to_string(),
            preview: ToolAuthorizationPreview {
                working_directory: std::path::PathBuf::from("/repo"),
                capabilities: vec![ToolCapability::FilesystemWrite],
                side_effect_level: level,
                durability: ToolDurability::Ephemeral,
                execution_category: ToolExecutionCategory::default(),
                approval_category: ToolApprovalCategory::default(),
                raw_input: json!({}),
                structured_input: json!({}),
            },
        }
    }

    async fn gated(
        approval: &SessionApproval,
        level: ToolSideEffectLevel,
    ) -> ToolAuthorizationDecision {
        PolicyGate::new(approval.clone())
            .authorize(&authorization("shell", level))
            .await
            .expect("authorization does not error")
    }

    #[tokio::test]
    async fn a_read_only_session_refuses_at_the_gate_rather_than_surfacing() {
        // The whole point: a `Deny` here is terminal, so no remembered rule is
        // read and the approver is never reached. A `Prompt` would put the
        // call back where a seeded durable allow can answer it.
        let approval = SessionApproval::new(ApprovalPolicy::Never);
        let decision = gated(&approval, ToolSideEffectLevel::Process).await;

        assert_eq!(decision.outcome, ToolAuthorizationOutcome::Deny);
        assert_eq!(
            decision.reason.as_deref(),
            Some(
                "shell changes state outside this process, \
                 and this session is set to refuse that"
            ),
            "and it refuses in the same words the approver would have used"
        );
    }

    #[tokio::test]
    async fn every_other_policy_still_surfaces_the_call() {
        for policy in [ApprovalPolicy::Always, ApprovalPolicy::Prompt] {
            let approval = SessionApproval::new(policy);

            assert_eq!(
                gated(&approval, ToolSideEffectLevel::Process).await.outcome,
                ToolAuthorizationOutcome::Prompt,
                "{policy:?} decides per turn, and a remembered rule may answer first"
            );
        }
    }

    #[tokio::test]
    async fn a_read_is_allowed_whatever_the_policy_is() {
        // The corollary basis documents: prompting for reads trains people to
        // approve without reading, so not even read-only surfaces one.
        for policy in [
            ApprovalPolicy::Always,
            ApprovalPolicy::Prompt,
            ApprovalPolicy::Never,
        ] {
            let approval = SessionApproval::new(policy);

            assert_eq!(
                gated(&approval, ToolSideEffectLevel::None).await.outcome,
                ToolAuthorizationOutcome::Allow,
                "{policy:?} has nothing to ask about for a read"
            );
        }
    }

    #[tokio::test]
    async fn the_gate_follows_the_session_it_was_installed_on() {
        // Installed once, before the first turn, and never replaced: a gate
        // that cached the policy would keep answering for a mode the client
        // has already moved off.
        let approval = SessionApproval::new(ApprovalPolicy::Prompt);
        let gate = PolicyGate::new(approval.clone());
        let request = authorization("shell", ToolSideEffectLevel::Process);

        assert_eq!(
            gate.authorize(&request).await.expect("no error").outcome,
            ToolAuthorizationOutcome::Prompt
        );

        approval.set(ApprovalPolicy::Never);
        assert_eq!(
            gate.authorize(&request).await.expect("no error").outcome,
            ToolAuthorizationOutcome::Deny,
            "the same gate must read the mode the session is on now"
        );

        approval.set(ApprovalPolicy::Always);
        assert_eq!(
            gate.authorize(&request).await.expect("no error").outcome,
            ToolAuthorizationOutcome::Prompt,
            "and must stop refusing when the session moves back"
        );
    }

    #[test]
    fn a_gate_waits_as_long_as_it_takes_unless_told_otherwise() {
        let approval = SessionApproval::new(ApprovalPolicy::Prompt);

        assert_eq!(PolicyGate::new(approval.clone()).timeout(), None);
        assert_eq!(
            PolicyGate::new(approval)
                .with_timeout(Duration::from_secs(60))
                .timeout(),
            Some(Duration::from_secs(60))
        );
    }

    #[tokio::test]
    async fn a_refusal_for_the_session_is_also_remembered() {
        let (_approval, mut approver, asked) =
            gate(ApprovalPolicy::Prompt, ApprovalDecision::DenyForSession);

        assert_eq!(
            approver.approve(&request("shell")).await.decision,
            ApprovalDecision::Deny
        );
        let repeated = approver.approve(&request("shell")).await;
        assert_eq!(repeated.decision, ApprovalDecision::Deny);
        assert_eq!(
            repeated.reason.as_deref(),
            Some("shell was refused earlier in this session, and that answer still stands")
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }
}
