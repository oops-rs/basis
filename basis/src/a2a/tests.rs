use super::*;

fn config(global: Option<PathBuf>) -> A2aConfig {
    A2aConfig {
        workspace_file: PathBuf::from(DEFAULT_WORKSPACE_A2A_FILE),
        global_dir: global,
        supplied: Vec::new(),
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

fn one_agent(name: &str, url: &str) -> String {
    format!(r#"{{"agents":{{"{name}":{{"url":"{url}"}}}}}}"#)
}

fn urls(agents: &[A2aAgent]) -> Vec<(&str, &str)> {
    agents
        .iter()
        .map(|agent| (agent.name(), agent.config().url.as_str()))
        .collect()
}

#[test]
fn nothing_on_disk_means_no_agents() {
    let tmp = tempfile::tempdir().expect("tempdir");

    assert!(
        discover(tmp.path(), &config(None))
            .expect("no file is not an error")
            .is_empty()
    );
}

#[test]
fn the_workspace_file_lives_under_dot_basis() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join(".basis/a2a.json"),
        &one_agent("nous", "https://nous.example"),
    );

    let found = discover(tmp.path(), &config(None)).expect("discovery succeeds");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].scope, ContextScope::Workspace);
    assert_eq!(found[0].agents[0].name(), "nous");
}

#[test]
fn a_supplied_agent_outranks_the_workspace_file_which_outranks_the_global_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_A2A_FILE),
        r#"{"agents":{"nous":{"url":"https://workspace.example"},"local":{"url":"https://local.example"}}}"#,
    );
    write(
        &global.join(DEFAULT_GLOBAL_A2A_FILE),
        r#"{"agents":{"nous":{"url":"https://global.example"},"local":{"url":"https://global.example"},"shared":{"url":"https://shared.example"}}}"#,
    );

    let config = config(Some(global)).with_supplied(vec![
        A2aAgentConfig::new("nous", "https://host.example").into(),
    ]);
    let layered = agents(tmp.path(), &config).expect("layering succeeds");

    assert_eq!(
        urls(&layered),
        [
            ("nous", "https://host.example"),
            ("local", "https://local.example"),
            ("shared", "https://shared.example"),
        ],
        "one name is one agent, and the most specific source wins it whole"
    );
}

#[test]
fn there_is_no_parent_walk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_A2A_FILE),
        &one_agent("parent", "https://parent.example"),
    );
    let child = tmp.path().join("crates/child");
    std::fs::create_dir_all(&child).expect("create child");

    assert!(
        agents(&child, &config(None))
            .expect("discovery succeeds")
            .is_empty(),
        "a file in a directory nobody pointed basis at names nobody's agent"
    );
}

#[test]
fn supplied_only_reads_no_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    write(
        &tmp.path().join(DEFAULT_WORKSPACE_A2A_FILE),
        &one_agent("file", "https://file.example"),
    );
    write(
        &global.join(DEFAULT_GLOBAL_A2A_FILE),
        &one_agent("global", "https://global.example"),
    );

    let config = config(Some(global))
        .with_supplied(vec![
            A2aAgentConfig::new("host", "https://host.example").into(),
        ])
        .supplied_only();

    assert_eq!(
        urls(&agents(tmp.path(), &config).expect("succeeds")),
        [("host", "https://host.example")]
    );
}

#[test]
fn the_same_file_reached_twice_is_read_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("global");
    write(
        &global.join(DEFAULT_GLOBAL_A2A_FILE),
        &one_agent("nous", "https://nous.example"),
    );

    let found = discover(
        &global,
        &A2aConfig {
            workspace_file: PathBuf::from(DEFAULT_GLOBAL_A2A_FILE),
            global_dir: Some(global.clone()),
            supplied: Vec::new(),
        },
    )
    .expect("discovery succeeds");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].scope, ContextScope::Workspace);
}

#[test]
fn a_malformed_file_is_an_error_not_a_silent_skip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(&tmp.path().join(DEFAULT_WORKSPACE_A2A_FILE), "{not json");

    let error = discover(tmp.path(), &config(None)).expect_err("malformed is an error");

    assert!(matches!(error, A2aError::Parse { .. }), "{error}");
}

#[test]
fn a_supplied_agent_mentra_would_refuse_fails_the_load_without_echoing_it() {
    // A host's list has no door of its own, so `supplied` is where the check
    // lands — the whole of mentra's, not just the name rule.
    let tmp = tempfile::tempdir().expect("tempdir");

    for (agent, needle) in [
        (A2aAgentConfig::new("evil__foo", "https://x.example"), "__"),
        (
            A2aAgentConfig::new("nous", "http://remote.example/?k=sk-live")
                .with_bearer_token("sk-live"),
            "plaintext",
        ),
    ] {
        let error = agents(tmp.path(), &config(None).with_supplied(vec![agent.into()]))
            .expect_err("refused before any agent is dialed");

        let rendered = error.to_string();
        assert!(matches!(error, A2aError::Invalid { .. }), "{rendered}");
        assert!(rendered.contains(needle), "{rendered}");
        assert!(!rendered.contains("sk-live"), "{rendered}");
    }
}

#[test]
fn a_host_may_grant_itself_plaintext_passage_in_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let agent = A2aAgentConfig::new("lab", "http://lab.internal")
        .with_bearer_token("t")
        .allowing_plaintext_credentials();

    assert_eq!(
        agents(tmp.path(), &config(None).with_supplied(vec![agent.into()]))
            .expect("the override is the host's to give")
            .len(),
        1
    );
}

#[test]
fn a_configured_agent_is_not_printed_by_whatever_holds_it() {
    let config = A2aConfig::default().with_supplied(vec![
        A2aAgentConfig::new("nous", "https://nous.example/card.json?key=sk-live-secret")
            .with_bearer_token("sk-live-secret")
            .into(),
    ]);

    let rendered = format!("{config:?}");
    assert!(!rendered.contains("sk-live-secret"), "{rendered}");
    assert!(rendered.contains("nous.example"), "{rendered}");
}

#[test]
fn defaults_configure_no_agents_of_their_own() {
    let config = A2aConfig::default();

    assert_eq!(config.workspace_file, PathBuf::from(".basis/a2a.json"));
    assert!(config.supplied.is_empty());
}
