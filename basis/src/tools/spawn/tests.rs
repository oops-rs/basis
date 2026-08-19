//! What `spawn` decides before anything runs.
//!
//! Everything here is reachable without a runtime, which is the point of
//! parsing at the boundary and shaping the preview from the parsed value: the
//! rules that decide what an operator is asked, and what a stored rule matches,
//! are functions and can be pinned as functions. The half that needs a driven
//! turn — approval ordering, `--no-shell`, delegation accounting — is in
//! `tests/spawn.rs`.

use super::*;

use std::path::PathBuf;

fn call(input: &str) -> Value {
    json!({ INPUT_FIELD: input })
}

fn parsed(input: &str) -> Spawn {
    parse(&call(input)).expect("this input parses")
}

fn refusal(input: &str) -> String {
    parse(&call(input)).expect_err("this input does not parse")
}

fn preview_of(input: &str) -> ToolAuthorizationPreview {
    let spawn = parsed(input);
    let tool = SpawnTool::new();
    preview(
        &spawn,
        PathBuf::from("/repo"),
        &tool.descriptor(),
        &call(input),
    )
}

#[test]
fn a_leading_bang_means_run_this() {
    let spawn = parsed("!cargo test -q");

    assert_eq!(spawn.mode(), Mode::Command);
    assert_eq!(spawn.body(), "cargo test -q");
}

#[test]
fn anything_else_is_a_task_for_a_subagent() {
    let spawn = parsed("find every TODO under src/");

    assert_eq!(spawn.mode(), Mode::Agent);
    assert_eq!(spawn.body(), "find every TODO under src/");
}

#[test]
fn a_doubled_bang_delegates_a_task_that_starts_with_one() {
    // The escape ADR-0016 owes a model that wants to say "!important, …".
    // Exactly one `!` is consumed, so the prompt keeps whatever it began with.
    let spawn = parsed("!!important: summarise the diff");

    assert_eq!(spawn.mode(), Mode::Agent);
    assert_eq!(spawn.body(), "!important: summarise the diff");

    let doubled = parsed("!!!still a prompt");
    assert_eq!(doubled.mode(), Mode::Agent);
    assert_eq!(doubled.body(), "!!still a prompt");
}

#[test]
fn the_string_is_trimmed_once_and_never_read_again() {
    // A stray leading newline must not be the difference between a command and
    // a prompt, and the body every later reader sees is the trimmed one — so
    // there is no second normalization for a consumer to disagree with.
    assert_eq!(parsed("  \n !ls -la  ").mode(), Mode::Command);
    assert_eq!(parsed("  \n !ls -la  ").body(), "ls -la");
    assert_eq!(parsed("\tread the README\n").body(), "read the README");
}

#[test]
fn an_empty_body_says_what_to_write_instead() {
    // These strings reach the model as the call's result, so each has to leave
    // it able to write a call that works.
    assert!(refusal("!").contains("!cargo test"), "{}", refusal("!"));
    assert!(refusal("!   ").contains("!cargo test"));
    assert!(refusal("").contains("delegate"), "{}", refusal(""));
    assert!(refusal("   ").contains("delegate"));
}

#[test]
fn a_call_with_no_string_in_it_is_told_which_field_to_fill() {
    for input in [json!({}), json!({ "input": 7 }), json!({ "command": "ls" })] {
        let error = parse(&input).expect_err("only a string input parses");
        assert!(error.contains(INPUT_FIELD), "{error}");
    }
}

#[test]
fn the_mode_spelling_is_the_one_rules_are_written_against() {
    // An operator's stored `RuleKey { tool_name: "spawn", pattern }` globs
    // against the serialized structured input, so renaming either of these
    // silently stops every rule already written from matching.
    assert_eq!(Mode::Command.as_str(), "command");
    assert_eq!(Mode::Agent.as_str(), "agent");
}

#[test]
fn a_command_presents_as_a_process_and_a_delegation_as_local_state() {
    // The two levels `shell` and `task` declared, now decided per call rather
    // than per name — and neither is `None`, so `ApprovalGate` can never wave
    // command mode through under the reads-are-never-asked rule.
    assert_eq!(
        preview_of("!rm -rf /").side_effect_level,
        ToolSideEffectLevel::Process
    );
    assert_eq!(
        preview_of("summarise the diff").side_effect_level,
        ToolSideEffectLevel::LocalState
    );

    assert!(crate::approval::is_consequential(
        preview_of("!rm -rf /").side_effect_level
    ));
    assert!(crate::approval::is_consequential(
        preview_of("summarise the diff").side_effect_level
    ));
}

#[test]
fn the_preview_carries_the_parsed_call_and_not_the_string() {
    // The claim ADR-0016 rests on: what the approver renders and what a rule
    // matches is the typed pair, so no consumer downstream re-reads `!`.
    let preview = preview_of("!cargo test -q");

    assert_eq!(
        preview.structured_input,
        json!({ "mode": "command", "body": "cargo test -q", "cwd": "/repo" })
    );
    assert_eq!(
        preview.raw_input,
        json!({ "input": "!cargo test -q" }),
        "the string the model wrote is kept, beside the parse rather than instead of it"
    );
    assert_eq!(preview.working_directory, PathBuf::from("/repo"));

    let delegation = preview_of("!!literally bang");
    assert_eq!(
        delegation.structured_input,
        json!({ "mode": "agent", "body": "!literally bang", "cwd": "/repo" }),
        "an escaped prompt reaches the approver as a prompt, escape already spent"
    );
}

#[test]
fn each_mode_is_categorised_as_the_door_it_replaced() {
    let command = preview_of("!ls");
    assert_eq!(command.approval_category, ToolApprovalCategory::Process);
    assert_eq!(
        command.execution_category,
        ToolExecutionCategory::ExclusiveLocalMutation
    );
    assert_eq!(
        command.capabilities,
        vec![ToolCapability::ProcessExec, ToolCapability::FilesystemWrite]
    );

    let agent = preview_of("read the README");
    assert_eq!(agent.approval_category, ToolApprovalCategory::Delegation);
    assert_eq!(agent.execution_category, ToolExecutionCategory::Delegation);
    assert_eq!(agent.capabilities, vec![ToolCapability::Delegation]);
}

#[test]
fn neither_mode_may_be_batched_with_anything() {
    // A command mutates the workspace and a delegation borrows the agent, so
    // both belong in the exclusive lane. A malformed call falls back to the
    // same lane rather than to the parallel one.
    let tool = SpawnTool::new();

    for input in [call("!ls"), call("read the README"), json!({})] {
        assert!(
            !tool.execution_category(&input).allows_parallel(),
            "{input} must not run in a parallel batch"
        );
    }
}

#[test]
fn the_static_descriptor_states_the_stronger_of_the_two_modes() {
    // Asked in the abstract — with no call in hand — a tool that can run
    // commands must not describe itself as something milder.
    let descriptor = SpawnTool::new().descriptor();

    assert_eq!(descriptor.provider.name, SPAWN);
    assert_eq!(descriptor.side_effect_level, ToolSideEffectLevel::Process);
    assert_eq!(descriptor.approval_category, ToolApprovalCategory::Process);
    assert!(!descriptor.terminal);
}

#[test]
fn the_description_teaches_the_convention_it_is_the_only_source_of() {
    // `!` is discoverable nowhere else: the schema has one untyped string in
    // it, so a description that omitted the prefix would leave the model to
    // guess that a command is even possible.
    let descriptor = SpawnTool::new().descriptor();
    let description = descriptor
        .provider
        .description
        .clone()
        .expect("the model is told what this does");

    for taught in ["!cargo test -q", "!!", "subagent"] {
        assert!(description.contains(taught), "{description}");
    }
}

#[test]
fn the_schema_asks_for_one_string_and_no_decisions() {
    let descriptor = SpawnTool::new().descriptor();
    let schema = descriptor.provider.input_schema;

    assert_eq!(schema["required"], json!([INPUT_FIELD]));
    assert_eq!(schema["properties"][INPUT_FIELD]["type"], "string");
    assert_eq!(
        schema["properties"].as_object().map(serde_json::Map::len),
        Some(1),
        "a second field is a decision on every call"
    );
}

#[test]
fn delegation_stops_at_the_floor_and_says_what_to_do_instead() {
    let ledger = depth::Depth::default();

    assert_eq!(ledger.authorize_delegation("root"), Ok(0));

    let _first = ledger.entered("child", 1);
    assert_eq!(ledger.authorize_delegation("child"), Ok(1));

    let _second = ledger.entered("grandchild", MAX_DEPTH);
    let refused = ledger
        .authorize_delegation("grandchild")
        .expect_err("the floor holds");
    assert_eq!(
        refused,
        "this work is already 2 levels of delegation deep and spawn goes no deeper than 2; \
         do it here rather than handing it on"
    );
}

#[test]
fn a_finished_delegation_leaves_no_trace_in_the_ledger() {
    // The entry lives exactly as long as the run that opened it, so a long
    // session holds one per delegation in flight rather than one per
    // delegation ever made.
    let ledger = depth::Depth::default();

    {
        let _entered = ledger.entered("child", 1);
        assert_eq!(ledger.authorize_delegation("child"), Ok(1));
    }

    assert_eq!(
        ledger.authorize_delegation("child"),
        Ok(0),
        "an id mentra reused would otherwise inherit a depth it never had"
    );
}

#[test]
fn a_command_is_never_refused_for_being_deep() {
    // Depth bounds *nesting*. An agent at the floor is still allowed to do the
    // work itself, and running a command is exactly that.
    let tool = SpawnTool::new();
    let _entered = tool.depth.entered("deep", MAX_DEPTH);

    assert!(tool.depth.authorize_delegation("deep").is_err());
    assert_eq!(parsed("!cargo test").mode(), Mode::Command);
}
