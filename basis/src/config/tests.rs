//! What `.basis/config.json` may say, who may say it, and which file wins.

use super::*;

/// An environment fixed by the test rather than by the shell that started it,
/// for [`crate::provider`]'s reason: the variables this module expands are
/// exactly the ones a person working on basis is likely to have exported.
fn exporting(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let vars: Vec<(String, String)> = vars
        .iter()
        .map(|(var, value)| ((*var).to_string(), (*value).to_string()))
        .collect();

    move |name: &str| {
        vars.iter()
            .find(|(var, _)| var == name)
            .map(|(_, value)| value.clone())
    }
}

fn nothing_exported() -> impl Fn(&str) -> Option<String> {
    exporting(&[])
}

fn write(path: &std::path::Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

/// The workspace file, in a fresh temp directory.
fn workspace_file(root: &std::path::Path, body: &str) {
    write(&root.join(DEFAULT_WORKSPACE_CONFIG_FILE), body);
}

fn global_file(dir: &std::path::Path, body: &str) {
    write(&dir.join(DEFAULT_GLOBAL_CONFIG_FILE), body);
}

fn discover(
    root: &std::path::Path,
    global: Option<&std::path::Path>,
) -> Result<Config, ConfigError> {
    Config::discover_with(root, global, &nothing_exported())
}

#[test]
fn nothing_on_disk_says_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let config = discover(tmp.path(), None).expect("a missing file is not an error");

    assert!(config.is_empty());
    assert!(config.files.is_empty());
}

#[test]
fn a_workspace_file_pins_the_model_and_names_itself() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(
        tmp.path(),
        r#"{"schema": 1, "model": "claude-sonnet-4-5-20250929"}"#,
    );

    let config = discover(tmp.path(), None).expect("a well-formed file");

    let model = config.model.as_ref().expect("the file set a model");
    assert_eq!(model.value, "claude-sonnet-4-5-20250929");
    assert_eq!(model.scope, ContextScope::Workspace);
    assert_eq!(model.path, tmp.path().join(DEFAULT_WORKSPACE_CONFIG_FILE));
    assert_eq!(
        config.model_selector(),
        Some(ModelSelector::Id("claude-sonnet-4-5-20250929".to_string()))
    );
    assert_eq!(config.files.len(), 1, "the file that decided is reported");
    assert_eq!(config.files[0].scope, "workspace");
}

#[test]
fn every_key_is_optional() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 1}"#);

    let config = discover(tmp.path(), None).expect("an empty file is a choice");

    assert!(
        config.is_empty(),
        "a file that says nothing settles nothing"
    );
    assert_eq!(config.files.len(), 1, "it was still read");
}

#[test]
fn the_workspace_file_outranks_the_global_one_key_by_key() {
    // The point of layering per key rather than per file: a repository that
    // pins a model has not thereby unsaid its owner's preferred effort.
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    workspace_file(tmp.path(), r#"{"schema": 1, "model": "workspace-model"}"#);
    global_file(
        &global,
        r#"{"schema": 1, "model": "global-model", "effort": "high", "provider": "openai"}"#,
    );

    let config = discover(tmp.path(), Some(&global)).expect("both files parse");

    let model = config.model.expect("a model");
    assert_eq!(model.value, "workspace-model");
    assert_eq!(model.scope, ContextScope::Workspace);

    let effort = config.effort.expect("the global effort survives");
    assert_eq!(effort.value, Effort::High);
    assert_eq!(effort.scope, ContextScope::Global);
    assert_eq!(effort.path, global.join(DEFAULT_GLOBAL_CONFIG_FILE));

    let provider = config.provider.expect("the global provider survives");
    assert_eq!(provider.value, BuiltinProvider::OpenAI);
    assert_eq!(provider.scope, ContextScope::Global);

    assert_eq!(config.files.len(), 2, "both files are reported");
    assert_eq!(
        config
            .files
            .iter()
            .map(|file| file.scope.as_str())
            .collect::<Vec<_>>(),
        vec!["workspace", "global"],
        "most specific first"
    );
}

#[test]
fn a_base_url_in_the_workspace_file_is_refused_and_the_file_is_named() {
    // The whole asymmetry of this format: a committed file may not point the
    // model's traffic — and the key riding on it — at a host nobody chose.
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(
        tmp.path(),
        r#"{"schema": 1, "base_url": "https://gateway.example.com/v1"}"#,
    );

    let error = discover(tmp.path(), None).expect_err("refused, not ignored");

    assert!(
        matches!(error, ConfigError::WorkspaceBaseUrl { .. }),
        "{error}"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains(DEFAULT_WORKSPACE_CONFIG_FILE),
        "{rendered}"
    );
    assert!(rendered.contains("base_url"), "{rendered}");
}

#[test]
fn a_base_url_in_the_global_file_is_honored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    global_file(
        &global,
        r#"{"schema": 1, "base_url": "https://gateway.example.com/v1"}"#,
    );

    let config = discover(tmp.path(), Some(&global)).expect("the user's own file may say it");

    let base_url = config.base_url.expect("a base URL");
    assert_eq!(
        base_url.value, "https://gateway.example.com/",
        "normalized where a typo can still name this file"
    );
    assert_eq!(base_url.scope, ContextScope::Global);
}

#[test]
fn a_workspace_file_may_still_choose_a_provider() {
    // Safe where `base_url` is not: it selects a preset endpoint and an
    // environment variable, and both of those are the user's own.
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 1, "provider": "anthropic"}"#);

    let config = discover(tmp.path(), None).expect("a well-formed file");

    assert_eq!(
        config.provider.expect("a provider").value,
        BuiltinProvider::Anthropic
    );
}

#[test]
fn there_is_no_api_key_and_saying_one_is_an_error() {
    // A credential is the environment's. `deny_unknown_fields` is what makes
    // the absence enforceable rather than documented.
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 1, "api_key": "sk-live-nope"}"#);

    let error = discover(tmp.path(), None).expect_err("there is no such key");

    assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
    assert!(
        !error.to_string().contains("sk-live-nope"),
        "no error repeats a value it read: {error}"
    );
}

#[test]
fn a_misspelled_key_is_an_error_not_a_silent_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 1, "modle": "gpt-5"}"#);

    let error = discover(tmp.path(), None).expect_err("a typo is caught");

    assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
}

#[test]
fn a_malformed_file_is_an_error_naming_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), "{not json");

    let error = discover(tmp.path(), None).expect_err("malformed is an error");

    match error {
        ConfigError::Parse { ref path, .. } => {
            assert_eq!(path, &tmp.path().join(DEFAULT_WORKSPACE_CONFIG_FILE));
        }
        other => panic!("expected a parse error, got {other}"),
    }
}

#[test]
fn a_file_that_states_no_schema_is_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"model": "gpt-5"}"#);

    let error = discover(tmp.path(), None).expect_err("basis will not guess a schema");

    assert!(matches!(error, ConfigError::NoSchema { .. }), "{error}");
}

#[test]
fn a_schema_from_the_future_is_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 99, "model": "gpt-5"}"#);

    let error = discover(tmp.path(), None).expect_err("an unknown schema is refused");

    assert!(
        matches!(error, ConfigError::UnsupportedSchema { schema: 99, .. }),
        "{error}"
    );
}

#[test]
fn environment_placeholders_are_expanded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(
        tmp.path(),
        r#"{"schema": 1, "model": "${TEAM_MODEL}", "provider": "${TEAM_PROVIDER}"}"#,
    );

    let config = Config::discover_with(
        tmp.path(),
        None,
        &exporting(&[("TEAM_MODEL", "gpt-5"), ("TEAM_PROVIDER", "openai")]),
    )
    .expect("both are set");

    assert_eq!(config.model.expect("a model").value, "gpt-5");
    assert_eq!(
        config.provider.expect("a provider").value,
        BuiltinProvider::OpenAI
    );
}

#[test]
fn an_unset_placeholder_names_the_key_and_the_variable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 1, "model": "${TEAM_MODEL}"}"#);

    let error = discover(tmp.path(), None).expect_err("an unset variable is an error");

    let rendered = error.to_string();
    assert!(rendered.contains("model"), "{rendered}");
    assert!(rendered.contains("TEAM_MODEL"), "{rendered}");
}

#[test]
fn a_placeholder_may_carry_a_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(
        tmp.path(),
        r#"{"schema": 1, "model": "${TEAM_MODEL:-gpt-5}"}"#,
    );

    let config = discover(tmp.path(), None).expect("the fallback answers");

    assert_eq!(config.model.expect("a model").value, "gpt-5");
}

#[test]
fn the_five_effort_spellings_are_the_flags_own() {
    for (text, expected) in [
        ("low", Effort::Low),
        ("medium", Effort::Medium),
        ("high", Effort::High),
        ("xhigh", Effort::XHigh),
        ("max", Effort::Max),
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        workspace_file(
            tmp.path(),
            &format!(r#"{{"schema": 1, "effort": "{text}"}}"#),
        );

        let config = discover(tmp.path(), None).expect("a known spelling");

        assert_eq!(config.effort.expect("an effort").value, expected);
    }
}

#[test]
fn an_unknown_effort_is_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 1, "effort": "hardest"}"#);

    let error = discover(tmp.path(), None).expect_err("only five words are efforts");

    assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
}

#[test]
fn an_unknown_provider_names_the_key_and_the_alternatives() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 1, "provider": "hal9000"}"#);

    let error = discover(tmp.path(), None).expect_err("rejected");

    let rendered = error.to_string();
    assert!(rendered.contains("provider"), "{rendered}");
    assert!(rendered.contains("anthropic"), "{rendered}");
}

#[test]
fn an_empty_model_is_an_error_rather_than_a_request_for_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    workspace_file(tmp.path(), r#"{"schema": 1, "model": "   "}"#);

    let error = discover(tmp.path(), None).expect_err("rejected");

    assert!(
        matches!(error, ConfigError::Invalid { key: "model", .. }),
        "{error}"
    );
}

#[test]
fn a_directory_where_the_file_should_be_is_ignored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join(DEFAULT_WORKSPACE_CONFIG_FILE))
        .expect("create a directory with the file's name");

    let config = discover(tmp.path(), None).expect("discovery succeeds");

    assert!(config.is_empty());
}

#[test]
fn the_same_file_reached_twice_is_read_once() {
    // A global directory that *is* the workspace's `.basis` would otherwise
    // layer one file against itself and report two.
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join(".basis");
    workspace_file(tmp.path(), r#"{"schema": 1, "model": "gpt-5"}"#);

    let config = discover(tmp.path(), Some(&global)).expect("discovery succeeds");

    assert_eq!(config.files.len(), 1);
    assert_eq!(config.files[0].scope, "workspace");
}

#[test]
fn an_empty_config_is_the_off_switch() {
    let config = Config::default();

    assert!(config.is_empty());
    assert_eq!(config.model_selector(), None);
}
