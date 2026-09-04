//! An approver judging by side-effect level rather than by tool name.

use std::path::Path;

use async_trait::async_trait;
use basis::{
    ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, CollectingSink,
    ToolSideEffectLevel,
    approval::ApprovalGate,
    run::prepare_with_session,
    tools::declared::{DeclaredTool, DeclaredToolSpec, SideEffect},
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Runtime, RuntimePolicy, runtime::VolatileRuntimeStore,
};
use serde_json::json;

use super::{NOT_STUCK, ScriptedProvider, context, session, tool_failed, tool_result};

/// A tool that leaves the machine, which is what an MCP server or a
/// `.basis/tools.json` entry declaring `"side_effect": "external"` looks like
/// to the gate.
///
/// The program does not exist, deliberately: nothing here should ever reach it,
/// and if the denial stopped working the tool would fail with a spawn error
/// rather than with the approver's own words — which is what the tests below
/// tell apart.
fn external_tool(workspace: &Path) -> DeclaredTool {
    DeclaredTool::new(
        DeclaredToolSpec {
            name: "publish".to_string(),
            description: "posts the result somewhere off this machine".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
            command: vec![
                workspace
                    .join("no-such-program")
                    .to_string_lossy()
                    .into_owned(),
            ],
            cwd: None,
            env: Vec::new(),
            timeout_ms: None,
            side_effect: SideEffect::External,
        },
        workspace,
    )
}

/// A turn that edits the checkout and then tries to leave the machine: one
/// `LocalState` call and one `External` one, with a read in front of both.
fn runtime_editing_then_publishing(workspace: &Path) -> (Runtime, ModelInfo) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let provider = ScriptedProvider::new(
        model.clone(),
        vec![
            vec![
                ContentBlock::ToolUse {
                    id: "call-0".to_string(),
                    name: "files".to_string(),
                    input: json!({
                        "operations": [
                            { "op": "create", "path": "made.txt", "content": "hi" }
                        ]
                    }),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "publish".to_string(),
                    input: json!({}),
                },
            ],
            vec![ContentBlock::text("done")],
        ],
    );

    let gate = ApprovalGate::new();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        .with_tool(external_tool(workspace))
        .with_tool_authorizer(gate)
        .build()
        .expect("runtime builds");

    (runtime, model)
}

#[tokio::test]
async fn an_approver_can_allow_edits_and_deny_the_network_without_naming_a_tool() {
    // The policy `basis::approval`'s own module doc has always named as the
    // reason the seam is a trait, driven through a real run. What makes it
    // worth a test is the *without naming a tool* half: an approver written as
    // a list of tool names silently stops covering the next MCP server a
    // workspace connects or the next program a repository declares, and until
    // `ApprovalRequest` carried the level there was no other way to write it.
    struct EditsButNotTheNetwork;

    #[async_trait]
    impl Approver for EditsButNotTheNetwork {
        async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
            match request.side_effect_level {
                Some(ToolSideEffectLevel::LocalState) => ApprovalDecision::Allow.into(),
                // Including `None`: a level basis could not recover is judged
                // by the most the call could be doing, never the least.
                _ => ApprovalAnswer::new(ApprovalDecision::Deny)
                    .because("this run may change this checkout and nothing beyond it"),
            }
        }
    }

    let workspace = tempfile::tempdir().expect("tempdir");
    let (runtime, model) = runtime_editing_then_publishing(workspace.path());
    let session = session(&runtime, workspace.path(), model);

    let mut prepared = prepare_with_session(
        session,
        workspace.path(),
        "make a file",
        &context(),
        "openai",
        "scripted-model",
    )
    .expect("prepared");

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(CollectingSink::new(), EditsButNotTheNetwork),
    )
    .await
    .expect("the run must not hang waiting on an unanswered approval")
    .expect("the run completes");

    let events = report.sink.into_events();

    assert_eq!(
        tool_failed(&events, "files"),
        Some(false),
        "an edit to this checkout is what the policy allows"
    );
    assert!(
        workspace.path().join("made.txt").exists(),
        "and an allowed edit must actually happen"
    );
    assert_eq!(
        tool_failed(&events, "publish"),
        Some(true),
        "and a call that leaves the machine is what it refuses"
    );
    assert_eq!(
        tool_result(&events, "publish").as_deref(),
        Some(
            "Tool execution denied: this run may change this checkout \
             and nothing beyond it"
        ),
        "refused by the approver, not by a program that failed to start"
    );
}
