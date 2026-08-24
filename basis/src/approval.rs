//! Asking before the agent does something consequential.
//!
//! Two pieces, and only two. [`ApprovalGate`] is the tool authorizer basis
//! installs on every runtime; it answers one question — *is this call worth
//! asking about* — and puts every call where the answer is yes to whoever is
//! answering. [`Approver`] is whoever that is, and it is the only thing that
//! decides.
//!
//! There was a third piece until ADR-0010: an `ApprovalPolicy` enum the core
//! interpreted, whose three values were three trait impls in disguise. Two of
//! them ship here — [`AllowAll`] and [`DenyAll`] — and the third, asking a
//! person, lives where the terminal is: `basis-acp` supplies an approver that
//! asks the client, and the binary one that asks at a TTY (ADR-0011). What the
//! enum could never express, the trait can: allow edits but deny the network,
//! ask over Slack with a timeout, escalate after the third refusal.
//!
//! The first of those is the one this module has to make *writable* rather than
//! merely describable, and it is written on [`Approver`]. It reads
//! [`ApprovalRequest::side_effect_level`] and names no tool, which is the whole
//! point: a policy spelled as a list of tool names is a policy that silently
//! stops covering the next MCP server a workspace connects.
//!
//! Nothing installs an approver by default, and that is deliberate: with no
//! approver the run gets [`AllowAll`], which is what a headless run needs.
//! Anything stricter is one argument to
//! [`run_with_approver`](crate::run::run_with_approver).

use std::time::Duration;

use async_trait::async_trait;
use mentra::{
    error::RuntimeError,
    tool::{ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer},
};
use serde_json::Value;

/// How far outside this process a call reaches: nothing, this machine's state,
/// another process, or the world.
///
/// mentra's, deliberately, and re-exported here under the rule written on
/// [`CancellationToken`](crate::CancellationToken) — every mentra type basis's
/// surface makes a caller *name*, basis re-exports. Both
/// [`is_consequential`] and [`ApprovalRequest::side_effect_level`] ask an
/// approver to name it, and without this line writing the policy those exist
/// for would mean adding mentra to the host's own manifest, pinned to whatever
/// version basis happens to resolve.
pub use mentra::tool::ToolSideEffectLevel;

/// What the agent wants to do, as put to an [`Approver`].
///
/// Deliberately not `#[non_exhaustive]`, though hosts read it far more often
/// than they build one. The struct has no constructor and no builder, so
/// sealing it would make an `ApprovalRequest` *unconstructable* outside this
/// crate — and every host testing its own approver builds one, as `basis-acp`
/// and `basis-cli` both do. Sealing would trade a compile error that names the
/// new field, on the day a field is added, for a permanent one with no way past
/// it.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Why approval is being asked for.
    pub description: String,
    /// The tool's input, parsed when it is JSON.
    pub input: Value,
    /// How far outside this process the call reaches, when basis knows.
    ///
    /// This is what lets a policy be written about *what a call does* rather
    /// than about which tools happen to be installed —
    /// [`LocalState`](ToolSideEffectLevel::LocalState) for an edit to this
    /// checkout, [`External`](ToolSideEffectLevel::External) for an MCP server
    /// or a declared tool that leaves the machine. See [`Approver`] for the
    /// worked example.
    ///
    /// **`None` means unknown, never harmless.** Read-only calls do not reach
    /// an approver at all ([`is_consequential`]), so nothing that arrives here
    /// is a read; a `None` is only ever basis failing to recover a fact it
    /// could not carry. The fail-closed reading is to treat it as
    /// [`External`](ToolSideEffectLevel::External) — judge it by the most it
    /// could be doing, the same rule the rest of this module runs on.
    ///
    /// It is an `Option` because the level rides mentra's own event: since
    /// [mentra#21](https://github.com/oops-rs/mentra/issues/21), a
    /// `PermissionRequested` carries the call's classification and basis reads
    /// the level straight off it. Every request a live session raises carries
    /// one; `None` is only an event replayed from a stream recorded before
    /// the field existed.
    pub side_effect_level: Option<ToolSideEffectLevel>,
}

/// What an [`Approver`] decided.
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

/// How an [`Approver`] answered: the decision, and — when it refused — why.
///
/// The reason is not decoration. A denial reaches the model as that tool
/// call's result, so the wording is the only thing telling it what to do
/// next: a model told merely that something was denied tries the write
/// again, and one told this run does not allow writes stops and reports.
/// An answer that leaves it unset still denies; the model just reads
/// mentra's standing "denied by session approver" instead.
///
/// Allowing needs no reason, because an allowed call explains itself by
/// happening.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApprovalAnswer {
    pub decision: ApprovalDecision,
    pub reason: Option<String>,
}

impl ApprovalAnswer {
    /// An answer that says only what it decided.
    pub fn new(decision: ApprovalDecision) -> Self {
        Self {
            decision,
            reason: None,
        }
    }

    /// The same answer, carrying the words the model will read.
    pub fn because(self, reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..self
        }
    }
}

impl From<ApprovalDecision> for ApprovalAnswer {
    fn from(decision: ApprovalDecision) -> Self {
        Self::new(decision)
    }
}

/// Answers approval requests. The seam a host plugs its own judgment into.
///
/// Called from the event-forwarding task while the turn is blocked inside
/// mentra waiting, so an implementation must answer rather than defer to
/// something that only happens after the run.
///
/// Async because answering genuinely takes time and the caller is an async
/// task: an ACP approver awaits a round trip to the client, and a terminal one
/// waits on a person. A synchronous signature would force both to block a
/// runtime worker thread — which tokio rejects outright for the ACP case. The
/// attribute to spell an impl with is re-exported at the crate root —
/// [`async_trait`](crate::async_trait) — so it costs no manifest line of the
/// host's own.
///
/// # Fail closed
///
/// **An approver that cannot answer denies.** No terminal to ask at, an answer
/// that never came, a channel whose other end is gone: none of those is
/// consent, and the only calls that reach an approver are the ones that change
/// something outside this process.
///
/// The worked example is the binary's `TerminalApprover`. Asked when stdin is
/// not a terminal — an unattended `basis spawn --approve prompt`, a cron job — it
/// denies without printing a question nobody would read, so the run fails
/// visibly instead of quietly granting whatever came up. `basis-acp`'s client
/// approver applies the same rule to a failed round trip, a cancelled request,
/// an answer it cannot parse, and its own thirty-minute timeout.
///
/// Each of those denials should say which one it was, on the
/// [`reason`](ApprovalAnswer::reason) of its answer. Failing closed silently
/// leaves the model to guess, and it guesses that retrying will work.
///
/// [`ApprovalDecision`]'s own default is [`Deny`](ApprovalDecision::Deny) for
/// the same reason, and so is mentra's when an authorizer times out: silence is
/// never a yes.
///
/// # Allow edits, deny the network
///
/// The policy this module's own documentation has always named as the reason
/// the seam is a trait, written out. Nothing in it names a tool — every call is
/// judged by how far it reaches — so a workspace that connects a new MCP server
/// tomorrow, or ships a `.basis/tools.json` declaring a program, is covered by
/// the rule that was already there.
///
/// ```
/// use basis::{
///     ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, ToolSideEffectLevel,
///     async_trait,
/// };
///
/// struct EditsButNotTheNetwork;
///
/// #[async_trait]
/// impl Approver for EditsButNotTheNetwork {
///     async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
///         match request.side_effect_level {
///             // Changes this machine's state and nothing past it: the file
///             // tools, and a delegation to a subagent.
///             Some(ToolSideEffectLevel::LocalState) => ApprovalDecision::Allow.into(),
///
///             // Everything else. `Process` is a command, which can reach the
///             // network by running `curl`; `External` says so outright; and
///             // `None` is a level basis could not recover, which is judged by
///             // the most it could be rather than the least. `ToolSideEffectLevel::None`
///             // never arrives — a read is not put to an approver at all.
///             _ => ApprovalAnswer::new(ApprovalDecision::Deny)
///                 .because("this run may change this checkout and nothing beyond it"),
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait Approver: Send + 'static {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer;
}

/// Forwards to the approver inside.
///
/// Lets a caller hold an approver it chose at runtime — one of several, or one
/// a feature flag picked — and still pass it to anything taking
/// `impl Approver`. The binary is exactly that caller: `--approve` names one of
/// three, and without this each arm would have to duplicate the whole run.
#[async_trait]
impl<A: Approver + ?Sized> Approver for Box<A> {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        (**self).approve(request).await
    }
}

/// Approves everything. What a confined or headless run wants, and what a run
/// given no approver of its own gets.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

#[async_trait]
impl Approver for AllowAll {
    async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
        ApprovalDecision::Allow.into()
    }
}

/// Refuses everything, so the agent can inspect a workspace and report on it
/// and cannot touch it. Each refusal reaches the model as a tool error, which
/// is how it learns to stop trying.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

#[async_trait]
impl Approver for DenyAll {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        ApprovalAnswer::new(ApprovalDecision::Deny).because(format!(
            "{} changes state outside this process, which this run does not allow",
            request.tool_name
        ))
    }
}

/// Whether a call changes anything outside this process.
///
/// Read-only calls are never worth asking about — prompting for them trains
/// people to approve without reading, which is worse than not asking.
pub fn is_consequential(level: ToolSideEffectLevel) -> bool {
    !matches!(level, ToolSideEffectLevel::None)
}

/// Puts every consequential call to the [`Approver`], and lets the rest
/// through.
///
/// This is the runtime half of approval, installed as mentra's
/// `ToolAuthorizer`. It carries no policy: since ADR-0010 there is nothing left
/// for one to say, because the approver decides. What it still owns is the
/// filter — [`is_consequential`] — and the choice to *surface* rather than
/// answer, which is what turns a call into a `PermissionRequested` event and
/// blocks the turn until someone resolves it.
///
/// Installed even by a run that approves everything, and that is the point. An
/// authorizer is fixed when the runtime is built and mentra never hands it
/// back; without one it allows every call unconditionally and no permission
/// request can ever be raised. Surfacing unconditionally is what lets the
/// answer be chosen per turn — or changed mid-session, which is how an ACP
/// client's mode picker works at all.
#[derive(Debug, Default, Clone)]
pub struct ApprovalGate {
    timeout: Option<Duration>,
}

impl ApprovalGate {
    pub fn new() -> Self {
        Self {
            // No timeout by default: a person reading a diff should not lose
            // the turn to a stopwatch. A host that needs one sets it.
            timeout: None,
        }
    }

    /// Gives up on an unanswered request after `timeout`, denying the call.
    ///
    /// mentra applies this to the whole wait, so it bounds an approver that
    /// never answers as well as one that answers slowly — the fail-closed rule
    /// of [`Approver`], enforced from outside for approvers that forget it.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
        }
    }
}

#[async_trait]
impl ToolAuthorizer for ApprovalGate {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        if !is_consequential(request.preview.side_effect_level) {
            return Ok(ToolAuthorizationDecision::allow());
        }

        // Nothing to relay: mentra puts the classification on the
        // `PermissionRequested` event itself (mentra#21), and the forwarder
        // reads the level straight off that.
        //
        // The reason becomes the description the approver shows, so it says
        // what is being asked rather than that something is.
        Ok(ToolAuthorizationDecision::prompt(format!(
            "{} wants to run and can change state outside this process",
            request.tool_name
        )))
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

    async fn outcome(level: ToolSideEffectLevel) -> ToolAuthorizationOutcome {
        ApprovalGate::new()
            .authorize(&request("shell", level))
            .await
            .expect("authorization does not error")
            .outcome
    }

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            request_id: "r".to_string(),
            tool_call_id: "t".to_string(),
            tool_name: "shell".to_string(),
            description: "d".to_string(),
            input: json!({}),
            side_effect_level: Some(ToolSideEffectLevel::Process),
        }
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
        assert_eq!(
            outcome(ToolSideEffectLevel::None).await,
            ToolAuthorizationOutcome::Allow,
            "prompting for reads trains people to approve without reading"
        );
    }

    #[tokio::test]
    async fn every_other_call_is_put_to_the_approver() {
        for level in [
            ToolSideEffectLevel::LocalState,
            ToolSideEffectLevel::Process,
            ToolSideEffectLevel::External,
        ] {
            assert_eq!(
                outcome(level).await,
                ToolAuthorizationOutcome::Prompt,
                "{level:?} changes something outside this process"
            );
        }
    }

    #[tokio::test]
    async fn the_request_says_which_tool_wants_to_run() {
        // This text is what an approver shows a person, so a request that
        // named nothing would be a prompt nobody can answer.
        let decision = ApprovalGate::new()
            .authorize(&request("files", ToolSideEffectLevel::LocalState))
            .await
            .expect("no error");

        let reason = decision.reason.expect("a prompt must say what it is about");
        assert!(reason.contains("files"), "{reason}");
    }

    #[test]
    fn a_gate_waits_as_long_as_it_takes_unless_told_otherwise() {
        assert_eq!(ApprovalGate::new().timeout(), None);
        assert_eq!(
            ApprovalGate::new()
                .with_timeout(Duration::from_secs(60))
                .timeout(),
            Some(Duration::from_secs(60))
        );
    }

    #[tokio::test]
    async fn the_trivial_approvers_answer_as_named() {
        let request = approval_request();

        assert_eq!(
            AllowAll.approve(&request).await.decision,
            ApprovalDecision::Allow
        );
        assert_eq!(
            DenyAll.approve(&request).await.decision,
            ApprovalDecision::Deny
        );
    }

    #[tokio::test]
    async fn a_blanket_refusal_tells_the_model_why_it_was_refused() {
        // Without this the model reads "denied" and tries the write again;
        // with it, it learns the run itself is the reason and stops.
        let reason = DenyAll
            .approve(&approval_request())
            .await
            .reason
            .expect("a refusal the model can act on must explain itself");

        assert_eq!(
            reason,
            "shell changes state outside this process, which this run does not allow"
        );
    }

    #[tokio::test]
    async fn a_boxed_approver_answers_exactly_as_the_one_inside() {
        // What the binary relies on to choose between three approvers without
        // writing the run out three times.
        let mut chosen: Box<dyn Approver> = Box::new(DenyAll);
        let answer = chosen.approve(&approval_request()).await;

        assert_eq!(answer.decision, ApprovalDecision::Deny);
        assert!(
            answer.reason.is_some(),
            "the reason must survive the indirection too"
        );
    }

    #[test]
    fn an_unanswered_request_is_a_refusal() {
        // The fail-closed rule, in the one place every approver inherits it:
        // whatever a decision defaults to is what silence means.
        assert_eq!(ApprovalDecision::default(), ApprovalDecision::Deny);
        assert_eq!(
            ApprovalAnswer::default(),
            ApprovalAnswer::new(ApprovalDecision::Deny)
        );
    }

    #[test]
    fn a_reason_rides_along_without_changing_the_decision() {
        let answer = ApprovalAnswer::from(ApprovalDecision::DenyForSession).because("no writes");

        assert_eq!(answer.decision, ApprovalDecision::DenyForSession);
        assert_eq!(answer.reason.as_deref(), Some("no writes"));
    }
}
