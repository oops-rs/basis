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

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use basis::approval::{ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver};

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
            ApprovalPolicy::Never => ApprovalAnswer::new(ApprovalDecision::Deny).because(format!(
                "{} changes state outside this process, and this session is set to refuse that",
                request.tool_name
            )),
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
