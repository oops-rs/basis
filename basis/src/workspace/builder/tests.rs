//! What opening a workspace settles, and what it must not.
//!
//! Split out of `builder.rs` only for its size. The tests for the knobs
//! ADR-0018 moved to the runtime — history, credential redaction, interceptor
//! ordering, the compatible provider — moved with them, to
//! `runtime/builder/tests.rs`.

use mentra::{
    ContentBlock,
    test::{MockRuntime, MockToolCall},
};

use crate::context::{ContextDocument, ContextScope};

use super::*;

/// The agent config a workspace that said nothing produces, with the snapshot
/// directory pinned so no test can name a real one.
fn defaults(workspace: &Path, context: &WorkspaceContext) -> mentra::agent::AgentConfig {
    agent_config(
        workspace,
        context,
        Compaction::default(),
        PathBuf::from("/transcripts"),
    )
}

#[test]
fn context_becomes_the_system_prompt_and_the_workspace_is_scoped() {
    let context = WorkspaceContext::from_documents(vec![ContextDocument {
        path: PathBuf::from("/repo/AGENTS.md"),
        scope: ContextScope::Workspace,
        content: "house rules".to_string(),
    }]);

    let agent = defaults(Path::new("/repo"), &context);

    assert!(
        agent
            .system
            .expect("a system prompt")
            .contains("house rules")
    );
    assert_eq!(agent.workspace.base_dir, PathBuf::from("/repo"));
}

#[test]
fn an_empty_workspace_context_leaves_the_system_prompt_unset() {
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(agent.system, None);
}

#[test]
fn the_two_doors_spawn_replaces_leave_the_roster() {
    // ADR-0016. Left alongside `spawn` they would restore what it removed:
    // three names arriving at one approval gate, and three rule namespaces,
    // for a question an operator asks once.
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    for replaced in ["shell", "background_run", "task"] {
        assert!(
            !agent.tool_profile.allows(replaced),
            "{replaced} is still offered to the model"
        );
    }
    assert!(
        agent.tool_profile.allows(crate::tools::SPAWN),
        "the door that replaces them has to be open"
    );
}

#[test]
fn hiding_is_a_roster_fact_and_not_a_capability_one() {
    // What lets `spawn` still reach the command executor underneath: nothing
    // here is an allow-list, so the tools stay registered on the runtime and
    // only stop being *offered*. A profile that named an allowed set instead
    // would take the capability away with the listing.
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(
        agent.tool_profile.allowed_tools, None,
        "an allow-list here would silently drop every tool nobody thought to name"
    );
    assert!(agent.tool_profile.allows("files"));
}

#[test]
fn commands_are_available_unless_the_caller_says_otherwise() {
    // ADR-0013: the first `basis "run the tests"` has to work.
    assert!(WorkspaceBuilder::new("/repo").shell.is_granted());
}

#[test]
fn a_fresh_builder_carries_a_private_default_runtime_and_no_model_override() {
    // The sugar's shape: `Workspace::open(path)` must behave as it always
    // has, so the default source is a default private recipe — and the model
    // is *unsaid*, deferring to that runtime's policy.
    let builder = WorkspaceBuilder::new("/repo");

    assert!(matches!(builder.runtime, RuntimeSource::Private(_)));
    assert_eq!(builder.model, None);
}

#[test]
fn builders_return_new_values() {
    let base = WorkspaceBuilder::new("/repo");
    let derived = base.with_model(ModelSelector::Id("pinned".to_string()));

    assert!(matches!(derived.model, Some(ModelSelector::Id(ref id)) if id == "pinned"));
    assert_eq!(
        WorkspaceBuilder::new("/repo").model,
        None,
        "a fresh builder defers to the runtime's policy"
    );
}

#[tokio::test]
async fn a_shared_runtime_is_the_one_the_workspace_borrows() {
    let runtime = Arc::new(
        crate::runtime::Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds offline"),
    );

    let builder = WorkspaceBuilder::new("/repo").with_runtime(Arc::clone(&runtime));

    match &builder.runtime {
        RuntimeSource::Shared(held) => assert!(
            Arc::ptr_eq(held, &runtime),
            "borrowing must not clone the substrate"
        ),
        RuntimeSource::Private(_) => panic!("with_runtime must switch the source"),
    }
}

#[test]
fn a_builder_holding_a_credentialed_recipe_does_not_print_it() {
    // WorkspaceBuilder's Debug delegates to the recipe's, which redacts; this
    // pins that the delegation actually happens.
    let printed = format!(
        "{:?}",
        WorkspaceBuilder::new("/repo").with_runtime_builder(
            crate::runtime::RuntimeBuilder::default().with_api_key("sk-secret-value")
        )
    );

    assert!(!printed.contains("sk-secret-value"), "{printed}");
    assert!(printed.contains("redacted"), "{printed}");
}

#[test]
fn an_unconfigured_workspace_keeps_every_tool_result() {
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(
        agent.compaction.keep_recent_tool_results,
        usize::MAX,
        "mentra's off switch for micro-compaction is what basis defaults to"
    );
    assert_eq!(agent.compaction.auto_compact_threshold_tokens, Some(50_000));
}

#[test]
fn what_a_host_says_about_compaction_is_what_the_agent_carries() {
    let compaction = Compaction::default()
        .with_keep_recent_tool_results(Some(2))
        .with_auto_threshold_tokens(Some(400_000))
        .with_preserve_recent_user_tokens(1_000);

    let agent = agent_config(
        Path::new("/repo"),
        &WorkspaceContext::default(),
        compaction,
        PathBuf::from("/transcripts"),
    );

    assert_eq!(agent.compaction.keep_recent_tool_results, 2);
    assert_eq!(
        agent.compaction.auto_compact_threshold_tokens,
        Some(400_000)
    );
    assert_eq!(agent.compaction.preserve_recent_user_tokens, 1_000);
}

#[test]
fn with_compaction_returns_a_new_builder() {
    let base = WorkspaceBuilder::new("/repo");
    let derived =
        base.with_compaction(Compaction::default().with_keep_recent_tool_results(Some(1)));

    assert_eq!(
        derived.compaction,
        Compaction::default().with_keep_recent_tool_results(Some(1))
    );
    assert_eq!(
        WorkspaceBuilder::new("/repo").compaction,
        Compaction::default(),
        "a fresh builder keeps everything"
    );
}

/// A workspace holding five files a model would read before it edits anything,
/// each one long enough to be worth eliding — mentra blanks a tool result over
/// 100 bytes — and each carrying a marker no other file has.
fn workspace_of_five_files() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");

    for index in 1..=5 {
        std::fs::write(
            dir.path().join(format!("file{index}.txt")),
            format!("marker-{index} {}", "x".repeat(200)),
        )
        .expect("write file");
    }

    dir
}

/// One tool-calling turn per file, then the sentence the model ends on.
fn reading_all_five(workspace: &Path) -> MockRuntime {
    let mut builder = MockRuntime::builder().model("mock-model", "openai");

    for index in 1..=5 {
        builder = builder.tool_calls(vec![
            MockToolCall::new(
                "files",
                serde_json::json!({
                    "operations": [{
                        "op": "read",
                        "path": workspace.join(format!("file{index}.txt")),
                    }],
                }),
            )
            .with_id(format!("read-{index}")),
        ]);
    }

    builder
        .text("read them all")
        .build()
        .expect("the mock runtime builds")
}

#[tokio::test]
async fn every_tool_result_the_model_read_is_still_in_front_of_it() {
    // The defect this pins. mentra's `keep_recent_tool_results` defaults to 3,
    // so from the fourth tool call on, every older result is replaced by
    // `[Previous: used files]` on the way to the provider — silently, with no
    // event, at any context size, on any model. A coding agent that reads five
    // files and then edits one would be editing from a transcript where the
    // first two are gone. basis keeps them all unless a host asks for elision
    // by number.
    let workspace = workspace_of_five_files();
    let mock = reading_all_five(workspace.path());
    let mut session = mock
        .runtime()
        .create_session_with_config(
            "compaction",
            mock.model(),
            defaults(workspace.path(), &WorkspaceContext::default()),
        )
        .expect("session");

    session
        .append_turn(vec![ContentBlock::Text {
            text: "read every file".to_string(),
        }])
        .await
        .expect("the scripted turn runs");

    let requests = mock.recorded_requests().await;
    let last = requests.last().expect("the model was asked something");
    let results: Vec<String> = last
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.to_display_string()),
            _ => None,
        })
        .collect();

    assert_eq!(results.len(), 5, "five reads, five results");
    for (index, result) in results.iter().enumerate() {
        assert!(
            result.contains(&format!("marker-{}", index + 1)),
            "the model can no longer see what it read: {result}"
        );
    }
}
