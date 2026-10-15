//! What `spawn` decides before anything runs.
//!
//! Everything here is reachable without a runtime, which is the point of
//! parsing at the boundary and shaping the preview from the parsed value: the
//! rules that decide what an operator is asked, and what a stored rule matches,
//! are functions and can be pinned as functions. The half that needs a driven
//! turn — approval ordering, `--no-shell`, delegation accounting — is in
//! `tests/spawn.rs`.

use super::*;
use parse::INPUT_FIELD;

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
    preview_with_child(input, None)
}

fn preview_with_child(input: &str, child: Option<Value>) -> ToolAuthorizationPreview {
    let spawn = parsed(input);
    let tool = SpawnTool::new();
    preview(
        &spawn,
        PathBuf::from("/repo"),
        &tool.descriptor(),
        &call(input),
        child,
        None,
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
fn an_at_prefix_names_where_the_command_runs() {
    // ADR-0021: one more fact on the same act, read by the same reader.
    let spawn = parsed("!@mac xcodebuild -list");

    assert_eq!(spawn.mode(), Mode::Command);
    assert_eq!(spawn.body(), "xcodebuild -list");
    assert_eq!(spawn.target(), Some("mac"));
}

#[test]
fn a_command_with_no_at_runs_where_basis_runs() {
    // The untargeted call is untouched by the new dimension, which is what
    // makes `target` additive rather than a second thing to decide.
    assert_eq!(parsed("!cargo test -q").target(), None);
    assert_eq!(parsed("summarise the diff").target(), None);
}

#[test]
fn a_target_name_may_hold_letters_digits_underscores_and_dashes() {
    for name in ["mac", "build-box", "ios_17", "M1"] {
        let spawn = parsed(&format!("!@{name} echo hi"));
        assert_eq!(spawn.target(), Some(name), "{name} is a usable name");
        assert_eq!(spawn.body(), "echo hi");
    }
}

#[test]
fn a_bad_target_spelling_is_taught_the_working_one() {
    // Each of these reaches the model as `Tool execution denied: …`, so each
    // has to leave it able to write a call that works.
    for (input, taught) in [
        ("!@", "`!@<target> <command>`"),
        ("!@ mac ls", "`!@<target> <command>`"),
        ("!@mac", "write the command after it"),
        ("!@mac   ", "write the command after it"),
        ("!@ma.c ls", "letters, digits"),
        ("!@/mac ls", "letters, digits"),
        ("!@mac/ ls", "letters, digits"),
        ("!@ma//c ls", "letters, digits"),
        ("!@\"mac\" ls", "letters, digits"),
    ] {
        let error = refusal(input);
        assert!(error.contains(taught), "{input} -> {error}");
    }
}

#[test]
fn local_is_the_wire_word_for_here_and_never_a_target() {
    // Reserved in the parser as well as on the builder, so a parsed call can
    // never carry it: `"target":"local"` in a serialized preview means exactly
    // one thing, and a rule written against it cannot be fooled by a model
    // that spelled the reserved word out.
    let error = refusal("!@local ls");

    assert!(error.contains(LOCAL_TARGET), "{error}");
    assert!(error.contains("`!cargo test`"), "{error}");
}

#[test]
fn the_escape_still_wins_over_the_routing_prefix() {
    // `!!` is read before `@`, so a prompt that begins with `!@` is still a
    // prompt, and exactly one `!` is spent on the escape — the delegated task
    // keeps the `!@x` it meant to say. Adding a routing prefix must not
    // quietly reinterpret an escape that already had a meaning.
    let spawn = parsed("!!@x is a shell idiom worth explaining");

    assert_eq!(spawn.mode(), Mode::Agent);
    assert_eq!(spawn.body(), "!@x is a shell idiom worth explaining");
    assert_eq!(spawn.target(), None);
}

#[test]
fn a_delegation_with_no_at_is_a_subagent_here() {
    assert_eq!(parsed("find every TODO").target(), None);
    assert_eq!(parsed("!!@mac still a prompt").target(), None);
}

#[test]
fn an_at_names_the_remote_agent_a_task_is_for() {
    // ADR-0029: the same dimension a command has, on a delegation.
    let spawn = parsed("@reviewer look at this diff");

    assert_eq!(spawn.mode(), Mode::Agent);
    assert_eq!(spawn.target(), Some("reviewer"));
    assert_eq!(spawn.body(), "look at this diff");
    assert!(!spawn.is_remote_wait());
}

#[test]
fn an_agent_named_alone_waits_on_what_it_is_still_doing() {
    // The one call whose body may be empty: it sends nothing new.
    let spawn = parsed("  @reviewer  ");

    assert_eq!(spawn.target(), Some("reviewer"));
    assert_eq!(spawn.body(), "");
    assert!(spawn.is_remote_wait());
}

#[test]
fn a_doubled_at_is_a_task_that_begins_with_one() {
    let spawn = parsed("@@channel is how the team pings everyone");

    assert_eq!(spawn.mode(), Mode::Agent);
    assert_eq!(spawn.target(), None);
    assert_eq!(spawn.body(), "@channel is how the team pings everyone");
}

#[test]
fn a_bad_agent_spelling_is_taught_the_working_one_and_the_escape() {
    for (input, taught) in [
        ("@", "`@<agent> <task>`"),
        ("@ reviewer look", "`@<agent> <task>`"),
        ("@src/main.rs explain this file", "letters, digits"),
        ("@\"reviewer\" look", "letters, digits"),
    ] {
        let error = refusal(input);
        assert!(error.contains(taught), "{input}: {error}");
        assert!(error.contains("`@@`"), "{input} names the escape: {error}");
    }
}

#[test]
fn a_name_may_be_namespaced_with_one_slash_between_parts() {
    // ADR-0037 §8: `team/reviewer` is a registry's handle, for commands and
    // agents alike — one charset for one `target` field.
    assert_eq!(
        parsed("@team/reviewer look at this").target(),
        Some("team/reviewer")
    );
    assert_eq!(parsed("!@team/box cargo test").target(), Some("team/box"));
    assert_eq!(parsed("@a/b/c deep").target(), Some("a/b/c"));
    for input in ["@/reviewer look", "@reviewer/ look", "@team//reviewer look"] {
        assert!(refusal(input).contains("letters, digits"), "{input}");
    }
}

#[test]
fn a_namespaced_name_nobody_resolves_is_never_a_teammate() {
    let (registry, _row) = ledger("agent-1");
    let tool = SpawnTool::new().with_agents(registry);

    let error = decided(&tool, "agent-1", call("@team/reviewer look")).expect_err("not a teammate");
    assert!(error.contains("namespaced"), "{error}");
    assert!(error.contains("`@reviewer`"), "{error}");

    let error = decided(
        &tool,
        "agent-1",
        call_reaching("@team/reviewer look", "local"),
    )
    .expect_err("not a teammate");
    assert!(error.contains("namespaced"), "{error}");
}

#[test]
fn local_is_not_an_agent_name_either() {
    let error = refusal("@local do it");

    assert!(error.contains(LOCAL_TARGET), "{error}");
}

#[test]
fn a_remote_delegation_is_not_a_command_target_question() {
    // Command targets and remote agents are two registries: a runtime whose
    // only command target is `mac` must not route `@mac …` to it.
    let routed = SpawnTool::with_targets(["mac".to_string()]);

    assert_eq!(
        routed.authorize_target(&parsed("@mac look at this")),
        Ok(())
    );
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
fn lexical_mode_classification_covers_unreadable_empty_escaped_and_command_inputs() {
    assert_eq!(classify_spawn_input(&json!({})), None);
    assert_eq!(
        classify_spawn_input(&json!({(INPUT_FIELD): "   "})),
        Some(Mode::Agent)
    );
    assert_eq!(
        classify_spawn_input(&json!({(INPUT_FIELD): "!!urgent"})),
        Some(Mode::Agent)
    );
    assert_eq!(
        classify_spawn_input(&json!({(INPUT_FIELD): "!cargo test"})),
        Some(Mode::Command)
    );
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
        json!({ "mode": "command", "body": "cargo test -q", "cwd": "/repo", "target": "local" })
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
        json!({ "mode": "agent", "body": "!literally bang", "cwd": "/repo", "target": "local" }),
        "an escaped prompt reaches the approver as a prompt, escape already spent"
    );
}

#[test]
fn a_child_the_policy_shaped_is_in_front_of_the_approver() {
    // D4's additive key: with an override in hand, the preview says what the
    // child will be, and the four established keys keep their exact spellings
    // and values around it.
    let child = ChildSpec::inherit()
        .with_model(mentra::ModelInfo::new("cheap-model", "openai"))
        .preview_value();
    let preview = preview_with_child("triage: is this real?", child);

    assert_eq!(
        preview.structured_input,
        json!({
            "mode": "agent",
            "body": "triage: is this real?",
            "child": { "model": { "id": "cheap-model", "provider": "openai" } },
            "cwd": "/repo",
            "target": "local",
        })
    );
}

#[test]
fn an_inherited_child_leaves_the_preview_exactly_as_it_was() {
    // The other half of additive: inherit — every delegation on a policy-free
    // runtime — must present the four-key shape byte for byte, because
    // `child` sorts between `body` and `cwd` and an always-present key would
    // move under every rule already written.
    assert_eq!(
        preview_with_child("summarise the diff", ChildSpec::inherit().preview_value())
            .structured_input,
        json!({ "mode": "agent", "body": "summarise the diff", "cwd": "/repo", "target": "local" })
    );
}

#[test]
fn an_untargeted_call_says_local_rather_than_saying_nothing() {
    // ADR-0021: *here* is a value an operator writes rules about, so it has a
    // spelling. A glob against a JSON `null` is a spelling nobody notices
    // missing, and an absent key is one a pattern cannot say anything about.
    for input in ["!cargo test -q", "summarise the diff"] {
        assert_eq!(
            preview_of(input).structured_input["target"],
            json!(LOCAL_TARGET),
            "{input}"
        );
    }
}

#[test]
fn a_targeted_command_puts_its_destination_in_front_of_the_approver() {
    // The routing decision is data: the approver renders it and a remembered
    // rule can glob it, on the machinery ADR-0016 already tiered.
    let preview = preview_of("!@mac xcodebuild -list");

    assert_eq!(
        preview.structured_input,
        json!({
            "mode": "command",
            "body": "xcodebuild -list",
            "cwd": "/repo",
            "target": "mac",
        })
    );
    assert_eq!(
        preview.side_effect_level,
        ToolSideEffectLevel::Process,
        "a targeted command is still a command, so every command guard applies"
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
fn the_routing_prefix_is_taught_only_when_there_is_somewhere_to_route() {
    // ADR-0021: a model must not be told about a door that is not there. The
    // best case for mentioning an unregistered prefix is a wasted call; the
    // worse case is a model that reads the refusal as an invitation to guess.
    let bare = SpawnTool::new().descriptor();
    let bare_description = bare.provider.description.clone().expect("described");
    assert!(!bare_description.contains("!@"), "{bare_description}");
    assert!(
        !bare.provider.input_schema["properties"][INPUT_FIELD]["description"]
            .as_str()
            .expect("a described field")
            .contains("!@")
    );

    let routed = SpawnTool::with_targets(["mac".to_string(), "builder".to_string()]).descriptor();
    let routed_description = routed.provider.description.clone().expect("described");
    for taught in ["!@<target> <command>", "`mac`", "`builder`"] {
        assert!(routed_description.contains(taught), "{routed_description}");
    }
    assert!(
        routed.provider.input_schema["properties"][INPUT_FIELD]["description"]
            .as_str()
            .expect("a described field")
            .contains("!@<target> <command>")
    );
}

#[test]
fn the_taught_target_names_are_sorted_and_deduplicated() {
    // One string on every build: a description that reordered itself between
    // processes would change the prompt cache and the model's reading of it
    // for no reason anyone chose.
    let descriptor =
        SpawnTool::with_targets(["mac".to_string(), "builder".to_string(), "mac".to_string()])
            .descriptor();
    let description = descriptor.provider.description.expect("described");

    assert!(
        description.contains("`builder`, `mac`"),
        "sorted and deduplicated: {description}"
    );
}

#[test]
fn a_target_nothing_registered_is_refused_and_the_refusal_names_the_set() {
    // Refused in the preview, so it never becomes a question for a person, and
    // the words have to leave the model able to write a call that works.
    let routed = SpawnTool::with_targets(["mac".to_string(), "builder".to_string()]);
    let refused = routed
        .authorize_target(&parsed("!@linux uname -a"))
        .expect_err("linux is not registered");

    assert!(refused.contains("`linux`"), "{refused}");
    assert!(refused.contains("`builder`, `mac`"), "{refused}");
    assert!(refused.contains("runs where basis is running"), "{refused}");

    assert_eq!(
        routed.authorize_target(&parsed("!@mac xcodebuild -list")),
        Ok(()),
        "a registered name routes"
    );
    assert_eq!(
        routed.authorize_target(&parsed("!cargo test")),
        Ok(()),
        "and an untargeted command is not a routing question at all"
    );
}

#[test]
fn a_runtime_with_no_targets_says_so_rather_than_listing_nothing() {
    // The empty set is the ordinary case, and "the registered targets are "
    // followed by nothing is a sentence that teaches a model to try again.
    let refused = SpawnTool::new()
        .authorize_target(&parsed("!@mac xcodebuild -list"))
        .expect_err("nothing is registered");

    assert!(
        refused.contains("no command targets registered"),
        "{refused}"
    );
    assert!(refused.contains("`!@mac`"), "{refused}");
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

    let _second = ledger.entered("grandchild", DEFAULT_DELEGATION_DEPTH);
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
fn with_targets_and_depth_moves_the_floor() {
    // Decision D9: the floor is `RuntimeBuilder::with_delegation_depth`'s
    // value now, threaded in here — not the constant it defaulted to.
    let ledger = depth::Depth::new(1);

    assert_eq!(ledger.authorize_delegation("root"), Ok(0));

    let _entered = ledger.entered("child", 1);
    let refused = ledger
        .authorize_delegation("child")
        .expect_err("the narrower floor holds at 1, not 2");
    assert_eq!(
        refused,
        "this work is already 1 levels of delegation deep and spawn goes no deeper than 1; \
         do it here rather than handing it on"
    );
}

#[test]
fn a_command_is_never_refused_for_being_deep() {
    // Depth bounds *nesting*. An agent at the floor is still allowed to do the
    // work itself, and running a command is exactly that.
    let tool = SpawnTool::new();
    let _entered = tool.depth.entered("deep", DEFAULT_DELEGATION_DEPTH);

    assert!(tool.depth.authorize_delegation("deep").is_err());
    assert_eq!(parsed("!cargo test").mode(), Mode::Command);
}

// ADR-0037: `@<name>` is resolved, or a teammate; `reach` says which is
// allowed; the preview says which was decided.

fn call_reaching(input: &str, reach: &str) -> Value {
    json!({ INPUT_FIELD: input, parse::REACH_FIELD: reach })
}

#[test]
fn reach_is_a_typed_field_beside_the_string() {
    assert_eq!(parsed("@reviewer look").reach(), None, "unsaid is unsaid");
    for (said, reach) in [
        ("auto", Reach::Auto),
        ("remote", Reach::Remote),
        ("local", Reach::Local),
    ] {
        let spawn = parse(&call_reaching("@reviewer look", said)).expect("parses");
        assert_eq!(spawn.reach(), Some(reach));
        assert_eq!(spawn.target(), Some("reviewer"));
    }
    let spawn = parse(&call_reaching("!cargo test", "local")).expect("parses");
    assert_eq!(spawn.mode(), Mode::Command, "a command ignores the field");
}

#[test]
fn a_bad_reach_is_taught_the_three_values() {
    let error = parse(&call_reaching("@reviewer look", "anywhere")).expect_err("refused");
    assert!(error.contains("`auto`, `remote` or `local`"), "{error}");
    let error = parse(&json!({ INPUT_FIELD: "@reviewer look", parse::REACH_FIELD: 3 }))
        .expect_err("refused");
    assert!(error.contains("is a string"), "{error}");
}

/// A ledger with one minted agent on it, held for the test's life.
fn ledger(
    agent_id: &str,
) -> (
    Arc<crate::runtime::agents::AgentRegistry>,
    crate::runtime::agents::AgentRow,
) {
    let registry = Arc::new(crate::runtime::agents::AgentRegistry::default());
    let row =
        registry.record_for_workspace(agent_id, crate::runtime::agents::AgentTools::default());
    (registry, row)
}

fn decided(tool: &SpawnTool, agent_id: &str, input: Value) -> Result<SpawnDecision, String> {
    let spawn = parse(&input).expect("parses");
    tool.decide(&spawn, agent_id, || Ok(PathBuf::from("/repo")))
}

#[test]
fn a_name_nobody_resolves_is_a_teammate_when_the_caller_keeps_any() {
    let (registry, _row) = ledger("agent-1");
    let tool = SpawnTool::new().with_agents(registry);

    let decision = decided(&tool, "agent-1", call("@reviewer look at this")).expect("decided");
    let SpawnDecision::Teammate { name, existing, .. } = decision else {
        panic!("a new teammate");
    };
    assert_eq!(name, "reviewer");
    assert!(existing.is_none(), "none made yet");
}

#[test]
fn a_runtime_with_no_ledger_keeps_no_teammates_and_says_so() {
    let error = decided(&SpawnTool::new(), "agent-1", call("@reviewer look")).expect_err("refused");

    assert!(error.contains("keeps no teammates"), "{error}");
    assert!(error.contains("`@@`"), "{error}");
}

#[test]
fn reach_remote_refuses_a_name_no_resolver_knows() {
    let (registry, _row) = ledger("agent-1");
    let tool = SpawnTool::new().with_agents(registry);

    let error = decided(&tool, "agent-1", call_reaching("@reviewer look", "remote"))
        .expect_err("no resolver knows it");

    assert!(error.contains("no remote agents"), "{error}");
}

#[test]
fn a_fixed_reach_policy_refuses_the_other_values_in_the_preview() {
    let (registry, _row) = ledger("agent-1");
    let tool = SpawnTool::new()
        .with_agents(registry)
        .with_reach_policy(ReachPolicy::fixed(Reach::Remote));

    let error = decided(&tool, "agent-1", call_reaching("@reviewer look", "local"))
        .expect_err("not allowed");
    assert!(error.contains("`reach: local` is not allowed"), "{error}");

    let error = decided(&tool, "agent-1", call("@reviewer look")).expect_err("remote only");
    assert!(
        error.contains("no remote agents"),
        "silence means remote under this policy: {error}"
    );
}

#[test]
fn a_teammate_named_alone_is_told_the_task_form() {
    let (registry, _row) = ledger("agent-1");
    let tool = SpawnTool::new().with_agents(registry);

    let error = decided(&tool, "agent-1", call("@reviewer")).expect_err("nothing to do");

    assert!(error.contains("names nobody yet"), "{error}");
    assert!(error.contains("`@reviewer <task>`"), "{error}");
}

#[test]
fn the_policy_shapes_a_teammate_by_its_name_when_it_is_made() {
    let (registry, _row) = ledger("agent-1");
    let tool =
        SpawnTool::new()
            .with_agents(registry)
            .with_child_policy(|child: &ChildContext<'_>| match child.name() {
                Some("triage") => ChildSpec::inherit().with_system("Answer yes or no."),
                _ => ChildSpec::inherit(),
            });

    let SpawnDecision::Teammate { spec, .. } =
        decided(&tool, "agent-1", call("@triage is this real?")).expect("decided")
    else {
        panic!("a teammate");
    };
    assert!(!spec.is_inherit(), "the policy saw the name");

    let SpawnDecision::Teammate { spec, .. } =
        decided(&tool, "agent-1", call("@reviewer is this real?")).expect("decided")
    else {
        panic!("a teammate");
    };
    assert!(spec.is_inherit());
}

#[test]
fn a_named_delegation_tells_the_approver_what_it_was_decided_to_reach() {
    let spawn = parsed("@reviewer look at this");
    let tool = SpawnTool::new();
    let descriptor = tool.descriptor();

    let local = preview(
        &spawn,
        PathBuf::from("/repo"),
        &descriptor,
        &call("@reviewer look at this"),
        None,
        Some(Named {
            reach: Reach::Local,
            agent_id: Some("agent-7".to_string()),
        }),
    );
    assert_eq!(
        local.structured_input,
        json!({
            "mode": "agent",
            "body": "look at this",
            "cwd": "/repo",
            "target": "reviewer",
            "reach": "local",
            "agent_id": "agent-7",
        })
    );
    assert_eq!(local.side_effect_level, ToolSideEffectLevel::LocalState);

    let remote = preview(
        &spawn,
        PathBuf::from("/repo"),
        &descriptor,
        &call("@reviewer look at this"),
        None,
        Some(Named {
            reach: Reach::Remote,
            agent_id: None,
        }),
    );
    assert_eq!(remote.structured_input["reach"], "remote");
    assert!(
        remote.structured_input.get("agent_id").is_none(),
        "not known yet"
    );
    assert_eq!(remote.side_effect_level, ToolSideEffectLevel::External);
}

#[test]
fn a_copy_that_keeps_teammates_teaches_them_and_the_plain_tool_does_not() {
    let (registry, _row) = ledger("agent-1");
    let plain = SpawnTool::new().with_agents(Arc::clone(&registry));
    let plain_text = plain.descriptor().provider.description.expect("described");
    assert!(
        !plain_text.contains("teammate"),
        "the runtime's own instance belongs to nobody: {plain_text}"
    );

    let owned = plain.for_agent("agent-1", Arc::new(|| {}));
    let descriptor = owned.descriptor();
    let text = descriptor.provider.description.clone().expect("described");
    assert!(text.contains("`@<name> <task>`"), "{text}");
    assert!(text.contains("`@reviewer look at this diff`"), "{text}");
    assert!(!text.contains("Your teammates:"), "none made yet: {text}");
    let schema = descriptor.provider.input_schema;
    assert_eq!(
        schema["properties"][parse::REACH_FIELD]["enum"],
        json!(["auto", "remote", "local"])
    );
    assert_eq!(schema["required"], json!([INPUT_FIELD]), "still optional");

    let fixed = owned.with_reach_policy(ReachPolicy::fixed(Reach::Local));
    let descriptor = fixed.descriptor();
    assert!(
        descriptor.provider.input_schema["properties"]
            .get(parse::REACH_FIELD)
            .is_none(),
        "no choice, no field"
    );
    assert!(
        !descriptor
            .provider
            .description
            .expect("described")
            .contains("`reach`"),
        "no choice, nothing taught about it"
    );
}

// ── ADR-0040: a delegation's parameters ──────────────────────────────────

fn with_metadata(input: &str, metadata: Value) -> Value {
    json!({ INPUT_FIELD: input, "metadata": metadata })
}

#[test]
fn parameters_ride_with_a_task_for_a_named_agent() {
    let spawn = parse(&with_metadata(
        "@wenext-android where is login?",
        json!({"project": "lama", "deep": true}),
    ))
    .expect("parsed");
    assert_eq!(spawn.body(), "where is login?");
    assert_eq!(
        spawn.metadata().map(|m| Value::Object(m.clone())),
        Some(json!({"project": "lama", "deep": true}))
    );
    // Absent, null and empty all mean none.
    for metadata in [Value::Null, json!({})] {
        let spawn = parse(&with_metadata("@nous hi", metadata)).expect("parsed");
        assert_eq!(spawn.metadata(), None);
    }
    assert_eq!(parsed("@nous hi").metadata(), None);
}

#[test]
fn parameters_anywhere_but_a_remote_task_are_refused() {
    for input in ["!cargo test", "!@mac ls", "summarise the diff", "@nous"] {
        let refused = parse(&with_metadata(input, json!({"project": "lama"})))
            .expect_err("nowhere to put them");
        assert!(refused.contains("`@<agent> <task>`"), "{input}: {refused}");
    }
    let refused = parse(&with_metadata("@nous hi", json!("lama"))).expect_err("not an object");
    assert!(refused.contains("JSON object"), "{refused}");
}

#[test]
fn the_approver_sees_the_parameters() {
    let spawn = parse(&with_metadata(
        "@reviewer look at this",
        json!({"project": "lama"}),
    ))
    .expect("parsed");
    let tool = SpawnTool::new();
    let remote = preview(
        &spawn,
        PathBuf::from("/repo"),
        &tool.descriptor(),
        &call("@reviewer look at this"),
        None,
        Some(Named {
            reach: Reach::Remote,
            agent_id: None,
        }),
    );
    assert_eq!(
        remote.structured_input["metadata"],
        json!({"project": "lama"})
    );
    let plain = preview_of("@reviewer look at this");
    assert!(plain.structured_input.get("metadata").is_none(), "additive");
}

// ── ADR-0041: a delegation's files ───────────────────────────────────────

fn with_files(input: &str, files: Value) -> Value {
    json!({ INPUT_FIELD: input, "files": files })
}

#[test]
fn files_ride_with_a_task_for_any_agent() {
    for input in ["@reviewer what is wrong here?", "summarise the log"] {
        let spawn = parse(&with_files(
            input,
            json!(["screens/login.png", "build.log"]),
        ))
        .expect("parsed");
        assert_eq!(spawn.files(), ["screens/login.png", "build.log"], "{input}");
    }
    for files in [Value::Null, json!([])] {
        assert!(
            parse(&with_files("@nous hi", files))
                .expect("parsed")
                .files()
                .is_empty()
        );
    }
    assert!(parsed("@nous hi").files().is_empty());
}

#[test]
fn files_with_a_command_or_a_wait_or_in_the_wrong_shape_are_refused() {
    for input in ["!cargo test", "!@mac ls", "@nous"] {
        let refused = parse(&with_files(input, json!(["a.png"]))).expect_err("nowhere to put them");
        assert!(refused.contains("goes with a task"), "{input}: {refused}");
    }
    for files in [json!("a.png"), json!([7]), json!([" "])] {
        let refused = parse(&with_files("@nous hi", files)).expect_err("not a list of paths");
        assert!(refused.contains("list of workspace paths"), "{refused}");
    }
}

#[test]
fn the_approver_sees_the_files() {
    let spawn = parse(&with_files("@reviewer look at this", json!(["shot.png"]))).expect("parsed");
    let tool = SpawnTool::new();
    let remote = preview(
        &spawn,
        PathBuf::from("/repo"),
        &tool.descriptor(),
        &call("@reviewer look at this"),
        None,
        Some(Named {
            reach: Reach::Remote,
            agent_id: None,
        }),
    );
    assert_eq!(remote.structured_input["files"], json!(["shot.png"]));
    let plain = preview_of("@reviewer look at this");
    assert!(plain.structured_input.get("files").is_none(), "additive");
}
