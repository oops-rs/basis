//! Resuming a conversation: what it carries forward, and what it does not.

use super::{offline, offline_runtime, write};

/// basis's documented duration for a "…for this session" answer basis's own
/// approval flow gives *now*: it lives with the live session and dies at the
/// next attach — automatically, with nothing basis states to make it true.
/// mentra 0.27's `PermissionRuleScope::Process` (mentra#53) is what that flow
/// remembers into — a rung owned by one live `SessionPermissionHandle`,
/// never written to the runtime store — so a resumed session (a fresh
/// handle, even for the same stable agent id) starts with an empty rung on
/// its own. See `a_resuming_a_legacy_session_scope_row_still_clears_it` below
/// for the different, migration-only question of a row an *older* basis
/// binary left behind in the durable `Session` scope.
#[tokio::test]
async fn a_resumed_conversation_forgets_its_for_this_session_answers() {
    use mentra::session::{PermissionRuleScope, RememberedRule, RuleKey};

    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store = tempfile::tempdir().expect("tempdir");

    let opened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let prepared = opened.prepare("go").expect("mints");
    let agent_id = prepared.agent_id().to_string();
    prepared
        .session()
        .permission_handle()
        .remember_rule(RememberedRule {
            key: RuleKey {
                tool_name: "spawn".to_string(),
                pattern: None,
            },
            allow: false,
            scope: PermissionRuleScope::Process,
            reason: Some("refused for the rest of this session".to_string()),
        })
        .expect("remembers");
    assert_eq!(
        prepared
            .session()
            .permission_handle()
            .remembered_rules()
            .expect("reads the live handle")
            .len(),
        1,
        "the answer must hold for the live session that gave it"
    );
    drop(prepared);
    drop(opened);

    let reopened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let resumed = reopened.resume(&agent_id, "again").expect("resumes");

    assert_eq!(
        resumed
            .session()
            .permission_handle()
            .remembered_rules()
            .expect("reads the live handle"),
        Vec::new(),
        "a for-this-session answer must die at the next attach, not replay forever — and \
         it never touched disk to begin with"
    );
}

/// The migration case the test above does not cover: a row a *pre-0.12*
/// basis binary remembered into the durable `Session` scope, before this
/// workspace's approval flow existed to remember into `Process` instead.
///
/// mentra 0.27 still loads and matches `Session`-scope rows exactly as it
/// always did — only `Process` rows are excluded from
/// `load_applicable_rules` — so a row written under an older basis binary
/// sits in `rules.json` today and would be replayed forever against a
/// session that never gave it, unless something clears it.
/// `Runtime::resume_minted` still does, on purpose, for exactly this legacy
/// row and no other: see its doc comment for why that is a one-way migration
/// cleanup rather than this workspace's live contract.
#[tokio::test]
async fn a_resuming_a_legacy_session_scope_row_still_clears_it() {
    use mentra::session::{PermissionRuleScope, RememberedRule, RuleKey};

    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store = tempfile::tempdir().expect("tempdir");

    let opened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let prepared = opened.prepare("go").expect("mints");
    let agent_id = prepared.agent_id().to_string();
    // What a pre-0.12 basis binary remembered a "for this session" answer
    // as: the durable `Session` scope, written straight through mentra's own
    // handle rather than through basis's (now `Process`-scoped) approval
    // flow, standing in for the row a real upgrade would find already on
    // disk.
    prepared
        .session()
        .permission_handle()
        .remember_rule(RememberedRule {
            key: RuleKey {
                tool_name: "spawn".to_string(),
                pattern: None,
            },
            allow: false,
            scope: PermissionRuleScope::Session,
            reason: Some("refused for the rest of this session".to_string()),
        })
        .expect("remembers");
    drop(prepared);
    drop(opened);

    let reopened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let resumed = reopened.resume(&agent_id, "again").expect("resumes");

    assert_eq!(
        resumed
            .session()
            .permission_handle()
            .remembered_rules()
            .expect("reads the live backend"),
        Vec::new(),
        "a legacy Session-scope row must still die at the next attach, or it \
         replays forever against a session that never gave it"
    );
}

/// `offline` resolves its model by explicit id, which mentra never asks a
/// listing for (`Runtime::resolve_model`) — so this workspace's context
/// window is unknown before *and* after a resume. What this pins is that
/// `resume`'s own reapplication of the resolved model — the fix for a
/// resumed agent otherwise losing a *known* window mentra does not persist —
/// does not corrupt the model a resumed conversation reports, in the one case
/// that exercises the same code path without a known window to lose.
#[tokio::test]
async fn resuming_on_the_same_model_reports_the_same_model_and_an_honest_unknown_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store = tempfile::tempdir().expect("tempdir");

    let opened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let prepared = opened.prepare("go").expect("mints");
    assert_eq!(
        prepared.context_window(),
        None,
        "an id-selected model was never listed, on any provider"
    );
    assert!(
        prepared.estimated_context_tokens() > 0,
        "the estimate still counts the system prompt AGENTS.md rendered, \
         even with an empty history"
    );
    let agent_id = prepared.agent_id().to_string();
    drop(prepared);
    drop(opened);

    let reopened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let resumed = reopened.resume(&agent_id, "again").expect("resumes");

    assert_eq!(resumed.context_window(), None);
    assert_eq!(
        resumed.context().model,
        "test-model",
        "reapplying the resolved model on resume must not rename it"
    );
}
