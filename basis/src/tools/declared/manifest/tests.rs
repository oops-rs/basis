//! What a manifest may say, and what basis refuses to read.
//!
//! Portable: nothing here spawns a process. The subprocess half lives in
//! [`super::super::tool`]'s tests and in `basis/tests/declared_tools.rs`.

use super::*;
use serde_json::json;

fn config(global: Option<PathBuf>) -> ToolsConfig {
    ToolsConfig {
        workspace_file: PathBuf::from(DEFAULT_WORKSPACE_TOOLS_FILE),
        global_dir: global,
        supplied: Vec::new(),
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

fn one_tool(name: &str, program: &str) -> String {
    format!(
        r#"{{"schema": 1, "tools": {{
            "{name}": {{
                "description": "does the thing",
                "input_schema": {{"type": "object", "properties": {{}}}},
                "command": ["{program}"]
            }}
        }}}}"#
    )
}

fn supplied(name: &str, program: &str) -> DeclaredToolSpec {
    DeclaredToolSpec {
        name: name.to_string(),
        description: "does the thing".to_string(),
        input_schema: json!({"type": "object"}),
        command: vec![program.to_string()],
        cwd: None,
        env: Vec::new(),
        timeout_ms: None,
        side_effect: SideEffect::Process,
    }
}

/// Parses `body` against an environment that holds nothing.
fn parsed(body: &str) -> Result<Vec<DeclaredToolSpec>, DeclaredToolError> {
    parse(Path::new("/repo/.basis/tools.json"), body, &|_| None)
}

/// The one declared tool `body` holds, or the reason it was refused.
fn only(body: &str) -> DeclaredToolSpec {
    let mut tools = parsed(body).expect("the manifest parses");
    assert_eq!(tools.len(), 1);
    tools.pop().expect("one tool")
}

#[test]
fn nothing_on_disk_means_no_tools() {
    let tmp = tempfile::tempdir().expect("tempdir");

    assert!(
        load(tmp.path(), &config(None))
            .expect("no file is not an error")
            .is_empty()
    );
}

#[test]
fn a_workspace_manifest_is_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(DEFAULT_WORKSPACE_TOOLS_FILE);
    write(&path, &one_tool("jenkins_job", "./ci/jenkins"));

    let found = discover(tmp.path(), &config(None)).expect("parses");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, path);
    assert_eq!(found[0].scope, ContextScope::Workspace);
    assert_eq!(found[0].tools[0].name, "jenkins_job");
}

#[test]
fn the_workspace_manifest_shadows_the_global_one() {
    // Names are the identity: two declarations under one name are one tool with
    // a precedence question, never two tools.
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_TOOLS_FILE),
        &one_tool("jenkins_job", "./from-the-workspace"),
    );
    write(
        &global.join(DEFAULT_GLOBAL_TOOLS_FILE),
        &one_tool("jenkins_job", "./from-the-global-file"),
    );

    let tools = load(tmp.path(), &config(Some(global))).expect("parses");

    assert_eq!(tools.len(), 1, "one name is one tool");
    assert_eq!(tools[0].command, vec!["./from-the-workspace".to_string()]);
}

#[test]
fn a_global_tool_survives_alongside_a_workspace_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_TOOLS_FILE),
        &one_tool("local_tool", "./a"),
    );
    write(
        &global.join(DEFAULT_GLOBAL_TOOLS_FILE),
        &one_tool("shared_tool", "./b"),
    );

    let tools = load(tmp.path(), &config(Some(global))).expect("parses");

    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, vec!["local_tool", "shared_tool"]);
}

#[test]
fn supplied_tools_shadow_files_and_preserve_source_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_TOOLS_FILE),
        r#"{"schema":1,"tools":{
            "local_only":{"description":"d","input_schema":{"type":"object"},"command":["/local"]},
            "shared":{"description":"d","input_schema":{"type":"object"},"command":["/workspace"]}
        }}"#,
    );
    write(
        &global.join(DEFAULT_GLOBAL_TOOLS_FILE),
        r#"{"schema":1,"tools":{
            "global_only":{"description":"d","input_schema":{"type":"object"},"command":["/global-only"]},
            "shared":{"description":"d","input_schema":{"type":"object"},"command":["/global"]}
        }}"#,
    );
    let config = config(Some(global)).with_supplied(vec![
        supplied("host_first", "/first"),
        supplied("shared", "/supplied"),
        supplied("host_first", "/duplicate"),
    ]);

    let tools = load(tmp.path(), &config).expect("all sources are valid");

    assert_eq!(
        tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.command[0].as_str()))
            .collect::<Vec<_>>(),
        [
            ("host_first", "/first"),
            ("shared", "/supplied"),
            ("local_only", "/local"),
            ("global_only", "/global-only"),
        ],
        "first name wins while supplied, workspace, and global source order survives"
    );
}

#[test]
fn the_same_file_reached_twice_is_read_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join(DEFAULT_GLOBAL_TOOLS_FILE),
        &one_tool("shared_tool", "./b"),
    );

    let found = discover(
        tmp.path(),
        &ToolsConfig {
            workspace_file: PathBuf::from(DEFAULT_GLOBAL_TOOLS_FILE),
            global_dir: Some(tmp.path().to_path_buf()),
            supplied: Vec::new(),
        },
    )
    .expect("parses");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].scope, ContextScope::Workspace);
}

#[test]
fn a_directory_where_the_manifest_should_be_is_ignored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join(DEFAULT_WORKSPACE_TOOLS_FILE))
        .expect("create a directory with the file's name");

    assert!(
        discover(tmp.path(), &config(None))
            .expect("parses")
            .is_empty()
    );
}

#[test]
fn a_malformed_manifest_is_an_error_not_an_empty_list() {
    // The alternative is a run whose model is missing a capability its
    // instructions assume, discovered only where it needed it.
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join(DEFAULT_WORKSPACE_TOOLS_FILE), "{not json");

    let error = load(tmp.path(), &config(None)).expect_err("rejected");

    assert!(matches!(error, DeclaredToolError::Parse { .. }), "{error}");
}

#[test]
fn a_future_schema_is_refused_by_name() {
    let error = parsed(r#"{"schema": 99, "tools": {}}"#).expect_err("rejected");

    assert!(matches!(
        error,
        DeclaredToolError::UnsupportedSchema { schema: 99, .. }
    ));
    assert!(error.to_string().contains("99"));
}

#[test]
fn a_manifest_with_no_schema_is_refused() {
    let error = parsed(r#"{"tools": {}}"#).expect_err("rejected");

    assert!(
        matches!(error, DeclaredToolError::NoSchema { .. }),
        "{error}"
    );
}

#[test]
fn a_misspelled_tools_key_is_refused_rather_than_read_as_empty() {
    // `deny_unknown_fields` catches it, so it arrives as a location in the file
    // rather than as a workspace whose tools quietly never appear.
    let error = parsed(r#"{"schema": 1, "tool": {}}"#).expect_err("rejected");

    assert!(matches!(error, DeclaredToolError::Parse { .. }), "{error}");
}

#[test]
fn a_misspelled_field_is_refused_rather_than_silently_defaulted() {
    let error = parsed(
        r#"{"schema": 1, "tools": {"t": {
            "description": "d",
            "input_schema": {"type": "object"},
            "command": ["./x"],
            "timout_ms": 10
        }}}"#,
    )
    .expect_err("rejected");

    assert!(
        matches!(error, DeclaredToolError::Parse { .. }),
        "a tool running on a timeout nobody chose is worse than a refused file: {error}"
    );
}

#[test]
fn every_field_a_tool_cannot_do_without_is_named_when_it_is_missing() {
    let cases = [
        (
            r#"{"input_schema": {"type": "object"}, "command": ["./x"]}"#,
            "description",
        ),
        (
            r#"{"description": "d", "command": ["./x"]}"#,
            "input_schema",
        ),
        (
            r#"{"description": "d", "input_schema": {"type": "object"}}"#,
            "command",
        ),
        (
            r#"{"description": "d", "input_schema": {"type": "object"}, "command": []}"#,
            "command",
        ),
    ];

    for (entry, field) in cases {
        let error = parsed(&format!(r#"{{"schema": 1, "tools": {{"t": {entry}}}}}"#))
            .expect_err("rejected");

        assert!(
            error.to_string().contains(field),
            "{field} was not named in {error}"
        );
    }
}

#[test]
fn a_name_a_provider_would_refuse_is_refused_here_instead() {
    // Otherwise the rejection arrives as an opaque request failure on the first
    // turn, naming neither the file nor the tool.
    for name in ["", "has a space", "emoji-🙂", &"x".repeat(65)] {
        let body = format!(
            r#"{{"schema": 1, "tools": {{"{name}": {{
                "description": "d",
                "input_schema": {{"type": "object"}},
                "command": ["./x"]
            }}}}}}"#
        );

        let error = parsed(&body).expect_err("rejected");
        assert!(
            matches!(error, DeclaredToolError::Invalid { .. }),
            "{name:?} was accepted: {error}"
        );
    }
}

#[test]
fn a_workspace_cannot_declare_a_tool_into_mcps_namespace() {
    let error = parsed(&one_tool("mcp__fs__read", "./x")).expect_err("rejected");

    assert!(
        matches!(error, DeclaredToolError::Invalid { .. }),
        "mentra parses that prefix to find a bridged server: {error}"
    );
}

#[test]
fn a_schema_that_is_not_an_object_is_refused() {
    for schema in [r#""string""#, "[]", r#"{"type": "array"}"#] {
        let body = format!(
            r#"{{"schema": 1, "tools": {{"t": {{
                "description": "d",
                "input_schema": {schema},
                "command": ["./x"]
            }}}}}}"#
        );

        let error = parsed(&body).expect_err("rejected");
        assert!(
            matches!(error, DeclaredToolError::Invalid { .. }),
            "{schema} was accepted: {error}"
        );
    }
}

#[test]
fn a_deadline_that_has_already_passed_is_refused() {
    let body = r#"{"schema": 1, "tools": {"t": {
        "description": "d",
        "input_schema": {"type": "object"},
        "command": ["./x"],
        "timeout_ms": 0
    }}}"#;

    let error = parsed(body).expect_err("rejected");

    assert!(
        matches!(error, DeclaredToolError::Invalid { .. }),
        "{error}"
    );
}

#[test]
fn an_undeclared_side_effect_still_reaches_the_approver() {
    // The fail-closed rule this whole binding turns on: the mildest thing a
    // program can truthfully say about itself is that it is a program.
    let spec = only(&one_tool("t", "./x"));

    assert_eq!(spec.side_effect, SideEffect::Process);
    assert!(crate::approval::is_consequential(spec.side_effect.level()));
}

#[test]
fn neither_side_effect_can_be_waved_through_as_a_read() {
    for side_effect in [SideEffect::Process, SideEffect::External] {
        assert!(
            crate::approval::is_consequential(side_effect.level()),
            "{side_effect:?} skipped the approver"
        );
    }
}

#[test]
fn a_tool_that_leaves_the_machine_can_say_so() {
    let body = r#"{"schema": 1, "tools": {"t": {
        "description": "d",
        "input_schema": {"type": "object"},
        "command": ["./x"],
        "side_effect": "external"
    }}}"#;

    assert_eq!(only(body).side_effect, SideEffect::External);
    assert_eq!(
        only(body).side_effect.level(),
        ToolSideEffectLevel::External
    );
}

#[test]
fn a_credential_reaches_the_program_through_the_environment() {
    // The whole reason `env` and `${VAR}` exist: the token is what the tool
    // needs and the last thing that should be in a file people commit.
    let body = r#"{"schema": 1, "tools": {"t": {
        "description": "d",
        "input_schema": {"type": "object"},
        "command": ["./x", "--host", "${CI_HOST}"],
        "env": {"CI_TOKEN": "${CI_TOKEN}"}
    }}}"#;

    let tools = parse(
        Path::new("/repo/.basis/tools.json"),
        body,
        &|name| match name {
            "CI_HOST" => Some("ci.example".to_string()),
            "CI_TOKEN" => Some("secret-value".to_string()),
            _ => None,
        },
    )
    .expect("both are set");

    assert_eq!(tools[0].command[2], "ci.example");
    assert_eq!(
        tools[0].env,
        vec![("CI_TOKEN".to_string(), "secret-value".to_string())]
    );
}

#[test]
fn an_unset_variable_names_the_field_and_never_the_value() {
    let body = r#"{"schema": 1, "tools": {"t": {
        "description": "d",
        "input_schema": {"type": "object"},
        "command": ["./x"],
        "env": {"CI_TOKEN": "${CI_TOKEN}"}
    }}}"#;

    let error = parsed(body).expect_err("CI_TOKEN is unset");

    let message = error.to_string();
    assert!(message.contains("env.CI_TOKEN"), "{message}");
    assert!(message.contains("`t`"), "the tool must be named: {message}");
}

#[test]
fn a_specs_environment_is_not_printed() {
    // `DeclaredToolSpec` is held by a registered tool and reachable from a
    // host's `{:?}`, and by the time it exists the `${VAR}` is the real value.
    let body = r#"{"schema": 1, "tools": {"t": {
        "description": "d",
        "input_schema": {"type": "object"},
        "command": ["./deploy", "--fast"],
        "env": {"CI_TOKEN": "${CI_TOKEN}"}
    }}}"#;

    let spec = parse(Path::new("/repo/.basis/tools.json"), body, &|_| {
        Some("secret-value".to_string())
    })
    .expect("parses")
    .pop()
    .expect("one tool");

    let printed = format!("{spec:?}");

    assert!(!printed.contains("secret-value"));
    assert!(printed.contains("redacted"));
    assert!(
        printed.contains("CI_TOKEN"),
        "the variable's name is what makes a misconfiguration fixable"
    );
    assert!(
        printed.contains("deploy") && printed.contains("--fast"),
        "the command and its arguments are how a spawn is debugged"
    );
}

#[test]
fn a_tool_runs_at_the_workspace_root_unless_it_says_otherwise() {
    let root = Path::new("/repo");
    let plain = only(&one_tool("t", "./x"));

    assert_eq!(plain.working_directory(root), PathBuf::from("/repo"));

    let nested = only(
        r#"{"schema": 1, "tools": {"t": {
            "description": "d",
            "input_schema": {"type": "object"},
            "command": ["./x"],
            "cwd": "services/ci"
        }}}"#,
    );

    assert_eq!(
        nested.working_directory(root),
        PathBuf::from("/repo/services/ci")
    );
}

#[test]
fn omitted_fields_take_the_documented_defaults() {
    let spec = only(&one_tool("t", "./x"));

    assert_eq!(spec.timeout(), DEFAULT_TOOL_TIMEOUT);
    assert_eq!(spec.side_effect, SideEffect::Process);
    assert_eq!(spec.cwd, None);
    assert!(spec.env.is_empty());
}

#[test]
fn defaults_declare_no_tools_of_their_own() {
    assert_eq!(
        ToolsConfig::default().workspace_file,
        PathBuf::from(".basis/tools.json")
    );
}
