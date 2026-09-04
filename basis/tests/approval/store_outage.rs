//! A "…for this session" answer against a store that has gone read-only.
//!
//! mentra 0.26 made resolving a remembered answer fallible: the rule was
//! persisted to the live store *before* the oneshot was answered, so a store
//! failure could leave an "…for this session" answer downgraded to a plain
//! denial with a notice explaining why. mentra 0.27 removes the failure mode
//! instead of basis having to recover from it: `AllowForSession` and
//! `DenyForSession` now remember into `PermissionRuleScope::Process`
//! (mentra#53), a rung owned by the live session alone and never written to
//! the runtime store — so a "…for this session" answer now survives exactly
//! the outage that used to downgrade it.

use std::path::Path;

use async_trait::async_trait;
use basis::{
    AllowAll, ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, CollectingSink, Event,
    PreparedRun, approval::ApprovalGate, run::prepare_with_session,
};
use mentra::{BuiltinProvider, ContentBlock, ModelInfo, Runtime, RuntimePolicy};
use serde_json::json;

use super::{NOT_STUCK, ScriptedProvider, context, session};

/// Restores the store root's permissions on drop, so a panicking assertion —
/// or a timeout — cannot leave a read-only tempdir behind that `TempDir::drop`
/// silently fails to remove.
struct RestorePermissions<'a>(&'a Path);

impl Drop for RestorePermissions<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// Warms a store-backed session, then makes its store root unwritable —
/// shared setup for the two "survives a store outage" tests below. Returns
/// `None` (skip the test) when this process writes through `0o555` anyway
/// (root), since every assertion downstream would test nothing.
async fn store_outage_fixture<'a>(
    workspace: &Path,
    store_dir: &'a Path,
    turn: Vec<ContentBlock>,
) -> Option<(PreparedRun, RestorePermissions<'a>)> {
    use std::os::unix::fs::PermissionsExt;

    use mentra::runtime::FileRuntimeStore;

    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            // A plain first turn, so every store file the turn machinery
            // touches — the agent's rows, `runs.jsonl` — exists before the
            // root goes read-only below.
            vec![ContentBlock::text("warmed")],
            turn,
            vec![ContentBlock::text("done")],
        ],
    );
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(FileRuntimeStore::new(store_dir))
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool_authorizer(ApprovalGate::new())
        .build()
        .expect("runtime builds");
    let session = session(&runtime, workspace, model);

    let mut prepared = prepare_with_session(
        session,
        workspace,
        "warm the store",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");
    prepared
        .execute_with_approver(CollectingSink::new(), AllowAll)
        .await
        .expect("the warming turn runs");

    // A durable remembered rule would rewrite `rules.json` atomically — a
    // fresh temp file in the store root — so a read-only root is exactly a
    // store that can still read its rules (there are none) and cannot record
    // a new durable one. Process-scoped remembering never reaches this
    // directory at all, which is the whole point of both tests below.
    std::fs::set_permissions(store_dir, std::fs::Permissions::from_mode(0o555))
        .expect("make the store root read-only");
    let restore = RestorePermissions(store_dir);

    // Mode bits do not stop root. Probe the premise instead of trusting the
    // effective uid: if this process can still create a file in the root,
    // every assertion downstream would test nothing.
    if std::fs::write(store_dir.join(".probe"), b"").is_ok() {
        eprintln!("skipping: this process writes through 0o555 (running as root?)");
        return None;
    }

    Some((prepared, restore))
}

/// Neither store-outage test below should ever see one of these: a
/// process-scoped remember never touches the read-only root, so there is
/// nothing to downgrade and nothing to explain. Shared so a change to the
/// wording or the check cannot drift between the two tests unnoticed.
fn assert_no_store_failure_notice(events: &[Event]) {
    use basis::event::NoticeSeverity;

    let notices: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            Event::Notice {
                severity: NoticeSeverity::Warning,
                message,
            } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        !notices
            .iter()
            .any(|message| message.contains("could not be")),
        "no store-failure notice belongs on this stream: {notices:?}"
    );
}

#[tokio::test]
async fn a_for_session_denial_survives_a_store_outage() {
    struct RefuseWithReason;

    #[async_trait]
    impl Approver for RefuseWithReason {
        async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
            ApprovalAnswer::new(ApprovalDecision::DenyForSession)
                .because("writes are refused at this desk")
        }
    }

    let workspace = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let Some((mut prepared, _restore)) = store_outage_fixture(
        workspace.path(),
        store_dir.path(),
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "files".to_string(),
            input: json!({
                "operations": [
                    { "op": "create", "path": "deny-me.txt", "content": "hi" }
                ]
            }),
        }],
    )
    .await
    else {
        return;
    };

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.send_with_options(
            "make the file",
            CollectingSink::new(),
            RefuseWithReason,
            basis::TurnOptions::default(),
        ),
    )
    .await
    .expect("a store outage must not hang the turn")
    .expect("the run completes");

    let events = report.sink.into_events();
    let results: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCompleted {
                tool_name, summary, ..
            } if tool_name == "files" => Some(summary.clone()),
            _ => None,
        })
        .collect();
    assert!(
        results
            .iter()
            .any(|result| result.ends_with("writes are refused at this desk")),
        "the refusal keeps the person's own reason, not a store error dressed \
         as one: {results:?}"
    );
    assert!(
        !workspace.path().join("deny-me.txt").exists(),
        "a refused write must not reach disk"
    );
    assert_no_store_failure_notice(&events);
}

#[tokio::test]
async fn a_for_session_approval_survives_a_store_outage() {
    struct AllowEverything;

    #[async_trait]
    impl Approver for AllowEverything {
        async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
            ApprovalDecision::AllowForSession.into()
        }
    }

    let workspace = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let Some((mut prepared, _restore)) = store_outage_fixture(
        workspace.path(),
        store_dir.path(),
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "files".to_string(),
            input: json!({
                "operations": [
                    { "op": "create", "path": "allow-me.txt", "content": "hi" }
                ]
            }),
        }],
    )
    .await
    else {
        return;
    };

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.send_with_options(
            "make the file",
            CollectingSink::new(),
            AllowEverything,
            basis::TurnOptions::default(),
        ),
    )
    .await
    .expect("a store outage must not hang the turn")
    .expect("the run completes");

    assert!(
        workspace.path().join("allow-me.txt").exists(),
        "the approval must actually run: remembering it for the session never \
         touched the read-only store root"
    );
    assert_no_store_failure_notice(&report.sink.into_events());
}
