//! Asking before the agent does something consequential.
//!
//! Until now lan installed no tool authorizer at all, so mentra's session
//! authorizer took `inner = None` and allowed every call unconditionally —
//! which also meant the `permission_requested` events in lan's stream could
//! never fire, and that anything answering them would have hung, because
//! nothing resolved them.
//!
//! This module closes both halves: a policy that decides which calls need
//! asking, and an [`Approver`] that answers. ACP (P2) supplies an approver
//! that asks the client; the terminal one asks a person.

mod terminal;

use std::time::Duration;

use async_trait::async_trait;
use mentra::{
    error::RuntimeError,
    tool::{
        ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer, ToolSideEffectLevel,
    },
};
use serde_json::Value;

pub use terminal::TerminalApprover;

/// When the agent must ask before acting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalPolicy {
    /// Never ask.
    ///
    /// The default, because it is what a headless run needs and what the
    /// container already makes safe — there the kernel bounds the damage, so a
    /// prompt would be ceremony. On an unconfined host with commands granted,
    /// prefer [`Self::Prompt`].
    #[default]
    Always,
    /// Ask before anything that changes state outside this process.
    Prompt,
    /// Refuse anything that changes state outside this process.
    ///
    /// Useful for a genuinely read-only run: the agent can inspect a
    /// workspace and report, and cannot touch it.
    Never,
}

/// What the agent wants to do, as put to an [`Approver`].
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Why approval is being asked for.
    pub description: String,
    /// The tool's input, parsed when it is JSON.
    pub input: Value,
}

/// How an [`Approver`] answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalDecision {
    Allow,
    /// The default: when in doubt, do not.
    #[default]
    Deny,
    /// Allow, and stop asking about this tool for the rest of the session.
    AllowForSession,
    /// Deny, and stop asking about this tool for the rest of the session.
    DenyForSession,
}

/// Answers approval requests.
///
/// Called from the event-forwarding task while the turn is blocked waiting, so
/// an implementation must answer rather than wait for something that only
/// happens after the run.
pub trait Approver: Send + 'static {
    fn approve(&mut self, request: &ApprovalRequest) -> ApprovalDecision;
}

/// Approves everything. What a confined or headless run wants.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl Approver for AllowAll {
    fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

/// Refuses everything, with the reason surfacing to the model as a tool error.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

impl Approver for DenyAll {
    fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

/// Whether a call changes anything outside this process.
///
/// Read-only calls are never worth asking about — prompting for them trains
/// people to approve without reading, which is worse than not asking.
pub fn is_consequential(level: ToolSideEffectLevel) -> bool {
    !matches!(level, ToolSideEffectLevel::None)
}

/// Applies an [`ApprovalPolicy`] to each tool call.
///
/// Installed on the runtime so that `Prompt` reaches mentra's session
/// authorizer, which is what emits `PermissionRequested` and waits.
#[derive(Debug, Clone, Copy)]
pub struct PolicyAuthorizer {
    policy: ApprovalPolicy,
    timeout: Option<Duration>,
}

impl PolicyAuthorizer {
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self {
            policy,
            // No timeout by default: a person reading a diff should not lose
            // the turn to a stopwatch. A host that needs one sets it.
            timeout: None,
        }
    }

    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
            ..self
        }
    }
}

#[async_trait]
impl ToolAuthorizer for PolicyAuthorizer {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        if !is_consequential(request.preview.side_effect_level) {
            return Ok(ToolAuthorizationDecision::allow());
        }

        Ok(match self.policy {
            ApprovalPolicy::Always => ToolAuthorizationDecision::allow(),
            ApprovalPolicy::Prompt => ToolAuthorizationDecision::prompt(format!(
                "{} wants to run and can change state outside this process",
                request.tool_name
            )),
            ApprovalPolicy::Never => ToolAuthorizationDecision::deny(format!(
                "{} changes state outside this process, which this run does not allow",
                request.tool_name
            )),
        })
    }

    fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mentra::tool::{
        ToolApprovalCategory, ToolAuthorizationOutcome, ToolAuthorizationPreview, ToolCapability,
        ToolDurability, ToolExecutionCategory,
    };
    use serde_json::json;
    use std::path::PathBuf;

    fn request(name: &str, level: ToolSideEffectLevel) -> ToolAuthorizationRequest {
        ToolAuthorizationRequest {
            agent_id: "a1".to_string(),
            agent_name: "test".to_string(),
            model: "m".to_string(),
            history_len: 1,
            tool_call_id: "tc-1".to_string(),
            tool_name: name.to_string(),
            preview: ToolAuthorizationPreview {
                working_directory: PathBuf::from("/repo"),
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

    async fn outcome(
        policy: ApprovalPolicy,
        level: ToolSideEffectLevel,
    ) -> ToolAuthorizationOutcome {
        PolicyAuthorizer::new(policy)
            .authorize(&request("shell", level))
            .await
            .expect("authorization does not error")
            .outcome
    }

    #[test]
    fn only_side_effects_are_consequential() {
        assert!(!is_consequential(ToolSideEffectLevel::None));
        assert!(is_consequential(ToolSideEffectLevel::LocalState));
        assert!(is_consequential(ToolSideEffectLevel::Process));
        assert!(is_consequential(ToolSideEffectLevel::External));
    }

    #[tokio::test]
    async fn a_read_only_call_is_never_worth_asking_about() {
        for policy in [
            ApprovalPolicy::Always,
            ApprovalPolicy::Prompt,
            ApprovalPolicy::Never,
        ] {
            assert_eq!(
                outcome(policy, ToolSideEffectLevel::None).await,
                ToolAuthorizationOutcome::Allow,
                "prompting for reads trains people to approve without reading"
            );
        }
    }

    #[tokio::test]
    async fn the_policy_decides_consequential_calls() {
        assert_eq!(
            outcome(ApprovalPolicy::Always, ToolSideEffectLevel::Process).await,
            ToolAuthorizationOutcome::Allow
        );
        assert_eq!(
            outcome(ApprovalPolicy::Prompt, ToolSideEffectLevel::Process).await,
            ToolAuthorizationOutcome::Prompt
        );
        assert_eq!(
            outcome(ApprovalPolicy::Never, ToolSideEffectLevel::Process).await,
            ToolAuthorizationOutcome::Deny
        );
    }

    #[tokio::test]
    async fn a_denial_says_why() {
        let decision = PolicyAuthorizer::new(ApprovalPolicy::Never)
            .authorize(&request("files", ToolSideEffectLevel::LocalState))
            .await
            .expect("no error");

        let reason = decision.reason.expect("a denial must explain itself");
        assert!(reason.contains("files"));
    }

    #[test]
    fn always_is_the_default_policy() {
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::Always);
    }

    #[test]
    fn the_trivial_approvers_answer_as_named() {
        let request = ApprovalRequest {
            request_id: "r".to_string(),
            tool_call_id: "t".to_string(),
            tool_name: "shell".to_string(),
            description: "d".to_string(),
            input: json!({}),
        };

        assert_eq!(AllowAll.approve(&request), ApprovalDecision::Allow);
        assert_eq!(DenyAll.approve(&request), ApprovalDecision::Deny);
    }
}
