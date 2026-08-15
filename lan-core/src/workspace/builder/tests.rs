//! What opening a workspace settles, and what it must not.
//!
//! Split out of `builder.rs` only for its size. The tests for the knobs
//! ADR-0018 moved to the runtime — history, credential redaction, interceptor
//! ordering, the compatible provider — moved with them, to
//! `runtime/builder/tests.rs`.

use crate::context::{ContextDocument, ContextScope};

use super::*;

#[test]
fn context_becomes_the_system_prompt_and_the_workspace_is_scoped() {
    let context = WorkspaceContext::from_documents(vec![ContextDocument {
        path: PathBuf::from("/repo/AGENTS.md"),
        scope: ContextScope::Workspace,
        content: "house rules".to_string(),
    }]);

    let agent = agent_config(Path::new("/repo"), &context);

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
    let agent = agent_config(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(agent.system, None);
}

#[test]
fn the_two_doors_spawn_replaces_leave_the_roster() {
    // ADR-0016. Left alongside `spawn` they would restore what it removed:
    // three names arriving at one approval gate, and three rule namespaces,
    // for a question an operator asks once.
    let agent = agent_config(Path::new("/repo"), &WorkspaceContext::default());

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
    let agent = agent_config(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(
        agent.tool_profile.allowed_tools, None,
        "an allow-list here would silently drop every tool nobody thought to name"
    );
    assert!(agent.tool_profile.allows("files"));
}

#[test]
fn commands_are_available_unless_the_caller_says_otherwise() {
    // ADR-0013: the first `lan "run the tests"` has to work.
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
