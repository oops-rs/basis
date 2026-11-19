//! Exercise the registered tool through a provider turn and the real dispatcher.
use std::sync::{Arc, Mutex};

use basis::{
    AllowAll, CollectingSink, DenyAll, ModelInfo, RunOutcome, RunProfile, RunSpec, Runtime,
    ToolRoster, Workspace,
};
use mentra::{BuiltinProvider, ContentBlock, tool::ToolResultContent};
use serde_json::{Value, json};

use super::{NOT_STUCK, Recording, ScriptedProvider, asked_about};

fn script(source: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::ToolUse {
        id: "outer".into(),
        name: "code".into(),
        input: json!({"source":source}),
    }]
}

fn builder(root: &std::path::Path, source: &str) -> basis::WorkspaceBuilder {
    builder_with_turns(root, vec![script(source)])
}

fn builder_with_turns(
    root: &std::path::Path,
    turns: Vec<Vec<ContentBlock>>,
) -> basis::WorkspaceBuilder {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let runtime = Runtime::builder()
        .with_provider_instance(ScriptedProvider::new(model.clone(), turns))
        .with_ephemeral_history();
    Workspace::builder(root)
        .without_discovery()
        .with_runtime_builder(runtime)
        .with_resolved_model(model)
}

fn with_tools(names: &[&str]) -> RunSpec {
    RunSpec::new("run the script").with_profile(
        RunProfile::new()
            .with_code_mode()
            .with_tool_roster(ToolRoster::only(names.iter().copied())),
    )
}

fn assert_public_result(run: &basis::PreparedRun, expected: Value) {
    let mut results = run
        .session()
        .history()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => Some((tool_use_id.as_str(), content, *is_error)),
            _ => None,
        });
    assert_eq!(
        results.next(),
        Some(("outer", &ToolResultContent::Structured(expected), false))
    );
    assert!(
        results.next().is_none(),
        "only the selected result belongs in provider history"
    );
}

#[tokio::test]
async fn computed_write_is_approved_and_only_the_selected_result_enters_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    std::fs::write(path.join("input"), "PRIVATE-INTERMEDIATE destination.txt").unwrap();
    let source = r#"
        const text = await tools.read({path: "input"});
        const path = text.trim().split(" ").pop();
        await tools.write({path, content: "updated"});
        return { path, hidden: typeof tools.spawn };
    "#;
    let workspace = builder(path, source).open().await.unwrap();
    let mut prepared = workspace
        .prepare(with_tools(&["code", "read", "write"]))
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(
            CollectingSink::new(),
            Recording {
                inner: AllowAll,
                seen: seen.clone(),
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert_eq!(
        std::fs::read_to_string(path.join("destination.txt")).unwrap(),
        "updated"
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(asked_about(&seen), ["write"]);
        assert!(seen[0].input.to_string().contains("destination.txt"));
    }
    let history = prepared.session().history();
    assert_public_result(
        &prepared,
        json!({"path":"destination.txt","hidden":"undefined"}),
    );
    assert!(
        !serde_json::to_string(history)
            .unwrap()
            .contains("PRIVATE-INTERMEDIATE")
    );
    let assistant_rounds = |run: &basis::PreparedRun| {
        run.session()
            .history()
            .iter()
            .filter(|message| message.role == mentra::Role::Assistant)
            .count()
    };
    assert_eq!(assistant_rounds(&prepared), 2);

    // Direct calls perform the same work in separate model rounds.
    let direct = tempfile::tempdir().unwrap();
    std::fs::copy(path.join("input"), direct.path().join("input")).unwrap();
    let direct_workspace = builder_with_turns(
        direct.path(),
        vec![
            vec![ContentBlock::ToolUse {
                id: "read-input".into(),
                name: "read".into(),
                input: json!({"path":"input"}),
            }],
            vec![ContentBlock::ToolUse {
                id: "write-output".into(),
                name: "write".into(),
                input: json!({"path":"destination.txt","content":"updated"}),
            }],
        ],
    )
    .open()
    .await
    .unwrap();
    let mut direct_run = direct_workspace.prepare("run the workflow").unwrap();
    direct_run
        .execute_with_approver(CollectingSink::new(), AllowAll)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(direct.path().join("destination.txt")).unwrap(),
        b"updated"
    );
    assert_eq!(assistant_rounds(&direct_run), 3);
    assert!(
        serde_json::to_string(direct_run.session().history())
            .unwrap()
            .contains("PRIVATE-INTERMEDIATE")
    );
}

#[tokio::test]
async fn child_denial_is_catchable_without_writing_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = builder(
        dir.path(),
        r#"
        try { await tools.write({path: "denied", content: "x"}); }
        catch (err) { return String(err).includes("write"); }
        return false;
    "#,
    )
    .open()
    .await
    .unwrap();
    let mut prepared = workspace.prepare(with_tools(&["code", "write"])).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(
            CollectingSink::new(),
            Recording {
                inner: DenyAll,
                seen: seen.clone(),
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(asked_about(&seen.lock().unwrap()), ["write"]);
    assert!(!dir.path().join("denied").exists());
    assert_public_result(&prepared, json!(true));
}

#[cfg(all(unix, feature = "mcp"))]
#[tokio::test]
async fn declared_stdout_feeds_a_bridged_mcp_tool_in_one_model_call() {
    use basis::{
        McpConfig,
        mcp::{McpServer, McpServerConfig},
        tools::declared::{DeclaredToolSpec, ToolsConfig},
    };
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        const record = JSON.parse(await tools.echo_input({ticket: "0042"}));
        return (await tools.mcp__tickets__lookup({ticket: record.ticket})).split(" ")[1];
    "#;
    let echo = DeclaredToolSpec {
        name: "echo_input".into(),
        description: "Echo the record as JSON text".into(),
        input_schema: json!({
            "type":"object", "properties":{"ticket":{"type":"string"}}, "required":["ticket"]
        }),
        command: vec!["/bin/cat".into()],
        env: vec![],
        cwd: None,
        timeout_ms: None,
        side_effect: Default::default(),
    };
    let tickets = McpServer::Stdio(McpServerConfig {
        name: "tickets".into(),
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            include_str!("fixtures/code-mode-mcp.sh").into(),
        ],
        env: Default::default(),
        cwd: None,
    });
    let workspace = builder(dir.path(), source)
        .with_tools(
            ToolsConfig::default()
                .supplied_only()
                .with_supplied(vec![echo]),
        )
        .with_mcp(
            McpConfig::default()
                .supplied_only()
                .with_supplied(vec![tickets]),
        )
        .open()
        .await
        .unwrap();
    let mut run = workspace
        .prepare(with_tools(&["code", "echo_input", "mcp__tickets__lookup"]))
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    tokio::time::timeout(
        NOT_STUCK,
        run.execute_with_approver(
            CollectingSink::new(),
            Recording {
                inner: AllowAll,
                seen: seen.clone(),
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        asked_about(&seen.lock().unwrap()),
        ["echo_input", "mcp__tickets__lookup"]
    );
    assert_public_result(&run, json!("0042"));
}

#[tokio::test]
async fn a_terminal_native_tool_stops_javascript_before_a_second_effect() {
    use basis::tools::{RuntimeToolDescriptor, ToolContext, ToolDefinition, ToolExecutor};
    use mentra::tool::ToolOutput;

    struct Finish;
    impl ToolDefinition for Finish {
        fn descriptor(&self) -> RuntimeToolDescriptor {
            RuntimeToolDescriptor::builder("finish").terminal().build()
        }
    }
    #[basis::async_trait]
    impl ToolExecutor for Finish {
        async fn execute_mut_output(
            &self,
            _: ToolContext<'_>,
            _: Value,
        ) -> Result<ToolOutput, String> {
            Ok(ToolOutput::structured(json!({"finished":true})).terminating())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = builder(
        dir.path(),
        r#"
        try { await tools.finish({}) } catch (err) {}
        await tools.write({path:"after-terminal",content:"unexpected"});
    "#,
    )
    .with_tool(Finish)
    .open()
    .await
    .unwrap();
    let mut run = workspace
        .prepare(with_tools(&["code", "finish", "write"]))
        .unwrap();
    let _report = tokio::time::timeout(
        NOT_STUCK,
        run.execute_with_approver(CollectingSink::new(), AllowAll),
    )
    .await
    .unwrap();
    assert!(!dir.path().join("after-terminal").exists());
    assert_public_result(&run, json!({"finished":true}));
}

#[tokio::test]
async fn the_outer_call_and_each_child_spend_the_same_tool_budget() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = builder(
        dir.path(),
        r#"
        await tools.write({path:"first", content:"first"});
        try { await tools.write({path:"second", content:"second"}); } catch (_) {}
        await tools.write({path:"third", content:"third"});
    "#,
    )
    .open()
    .await
    .unwrap();
    let mut run = workspace
        .prepare(with_tools(&["code", "write"]).with_tool_budget(2))
        .unwrap();
    let report = tokio::time::timeout(
        NOT_STUCK,
        run.execute_with_approver(CollectingSink::new(), AllowAll),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(report.stopped_by, Some(basis::Bound::ToolBudget));
    assert!(dir.path().join("first").exists());
    assert!(!dir.path().join("second").exists());
    assert!(!dir.path().join("third").exists());
}

#[tokio::test]
async fn a_run_sets_its_own_script_time() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = builder(dir.path(), "while (true) {}").open().await.unwrap();
    let spec = RunSpec::new("run the script").with_profile(
        RunProfile::new()
            .with_code_mode()
            .with_code_script_time(std::time::Duration::from_millis(50))
            .with_tool_roster(ToolRoster::only(["code"])),
    );
    let mut prepared = workspace.prepare(spec).unwrap();
    tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver(CollectingSink::new(), AllowAll),
    )
    .await
    .expect("the run's 50 ms ends the script, not the default 120 s")
    .unwrap();
    let history = serde_json::to_string(prepared.session().history()).unwrap();
    assert!(history.contains("timed out after 50ms"), "{history}");
}
