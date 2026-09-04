//! A durable remembered rule against a session authorizer that refuses it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use basis::{
    AllowAll, CollectingSink, Event,
    approval::{
        RuntimeError, ToolAuthorizationDecision, ToolAuthorizationRequest, ToolAuthorizer,
        is_consequential,
    },
    run::prepare_with_session,
};
use mentra::session::{PermissionRuleScope, RememberedRule, RuleKey};

use super::{
    NOT_STUCK, Recording, asked_about, context, runtime_writing_a_file, session, tool_failed,
};

/// Refuses every consequential call outright, the way a host with a posture
/// that must not be answerable by a remembered rule writes one.
struct RefusingGate;

#[async_trait]
impl ToolAuthorizer for RefusingGate {
    async fn authorize(
        &self,
        request: &ToolAuthorizationRequest,
    ) -> Result<ToolAuthorizationDecision, RuntimeError> {
        if !is_consequential(request.preview.side_effect_level) {
            return Ok(ToolAuthorizationDecision::allow());
        }

        Ok(ToolAuthorizationDecision::deny(format!(
            "{} changes state outside this process, which this session refuses",
            request.tool_name
        )))
    }
}

#[tokio::test]
async fn a_session_authorizers_refusal_outranks_a_rule_seeded_before_it() {
    // What `PreparedRun::with_tool_authorizer` exists for. A durable rule
    // resolves the runtime gate's `Prompt` ahead of the approver, so a posture
    // written on the approver can be pre-empted by an allow someone seeded
    // through the permission handle. An authorizer's own `Deny` is terminal:
    // mentra returns it unchanged, reads no rule, and raises no request.
    let workspace = tempfile::tempdir().expect("tempdir");
    let (runtime, model) = runtime_writing_a_file(workspace.path());
    let session = session(&runtime, workspace.path(), model);

    let prepared = prepare_with_session(
        session,
        workspace.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    // Seeded first and durably, so the rule genuinely predates the posture —
    // the seam `basis/examples/reviewed_shell.rs` teaches, at a scope nothing
    // clears.
    prepared
        .session()
        .permission_handle()
        .remember_rule(RememberedRule {
            key: RuleKey {
                tool_name: "files".to_string(),
                pattern: None,
            },
            allow: true,
            scope: PermissionRuleScope::Global,
            reason: None,
        })
        .expect("the rule is remembered");

    let mut prepared = prepared.with_tool_authorizer(RefusingGate);
    let seen = Arc::new(Mutex::new(Vec::new()));

    // `AllowAll`, so nothing an *approver* did can be mistaken for the
    // refusal: whatever reaches one is allowed.
    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(
            CollectingSink::new(),
            Recording {
                inner: AllowAll,
                seen: Arc::clone(&seen),
            },
        ),
    )
    .await
    .expect("the run must not hang")
    .expect("the run completes");

    let events = report.sink.into_events();
    let asked = seen.lock().expect("not poisoned").clone();

    assert!(
        !workspace.path().join("made.txt").exists(),
        "a seeded durable allow must not survive an authorizer that refuses"
    );
    assert_eq!(tool_failed(&events, "files"), Some(true));
    assert!(
        asked.is_empty(),
        "a terminal refusal is never put to the approver: {:?}",
        asked_about(&asked)
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "and never surfaced as a request, which is what proves no rule was read"
    );
    assert_eq!(
        tool_failed(&events, "check_background"),
        Some(false),
        "while a read still runs, because the gate answers `Allow` for one"
    );
}
