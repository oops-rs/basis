use super::*;

const SECRET: &str = "sk-live-do-not-print-me";

fn nothing_set(_: &str) -> Option<String> {
    None
}

fn parse_text(text: &str) -> Result<Vec<A2aAgent>, A2aError> {
    parse_with(Path::new("/repo/.basis/a2a.json"), text, &nothing_set)
}

fn with_secret(name: &str) -> Option<String> {
    (name == "TOKEN").then(|| SECRET.to_string())
}

#[test]
fn the_documented_shape_becomes_an_agent() {
    let agents = parse_with(
        Path::new("/repo/.basis/a2a.json"),
        r#"{"agents":{"nous":{"url":"https://nous.example.com","headers":{"Authorization":"Bearer ${TOKEN}"}}}}"#,
        &with_secret,
    )
    .expect("a well-formed file");

    assert_eq!(agents.len(), 1);
    let config = agents[0].config();
    assert_eq!(config.name, "nous", "the map key becomes the name");
    assert_eq!(config.url, "https://nous.example.com");
    assert_eq!(
        config
            .headers
            .get("Authorization")
            .map(mentra::mcp::SecretString::expose_secret),
        Some(format!("Bearer {SECRET}").as_str()),
        "header values expand"
    );
    assert!(
        !config.allow_plaintext_credentials,
        "a file can never grant the override"
    );
}

#[test]
fn the_url_expands_too() {
    let agents = parse_with(
        Path::new("/repo/.basis/a2a.json"),
        r#"{"agents":{"nous":{"url":"https://${HOST}/card.json"}}}"#,
        &|name| (name == "HOST").then(|| "nous.example.com".to_string()),
    )
    .expect("the host is set");

    assert_eq!(agents[0].config().url, "https://nous.example.com/card.json");
}

#[test]
fn an_empty_agents_object_is_allowed() {
    assert!(
        parse_text(r#"{"agents": {}}"#)
            .expect("explicitly empty is a choice")
            .is_empty()
    );
}

#[test]
fn a_missing_agents_key_is_an_error() {
    let error = parse_text(r#"{}"#).expect_err("nothing named is a typo, not a choice");

    assert!(matches!(error, A2aError::NoAgents { .. }), "{error}");
}

#[test]
fn unknown_keys_are_refused_at_every_level() {
    // This file is basis's own: a misspelled key is a typo, and a misspelled
    // `headers` is an agent called without its credential.
    for text in [
        r#"{"agnets": {}}"#,
        r#"{"agents": {}, "$schema": "x"}"#,
        r#"{"agents": {"nous": {"url": "https://x", "header": {}}}}"#,
    ] {
        let error = parse_text(text).expect_err("an unknown key is refused");
        assert!(matches!(error, A2aError::Parse { .. }), "{text}: {error}");
        assert!(
            error.to_string().contains("unknown key"),
            "the kind of problem is named: {error}"
        );
    }
}

#[test]
fn an_entry_without_a_url_names_the_field() {
    let error = parse_text(r#"{"agents":{"nous":{"headers":{}}}}"#).expect_err("nothing to reach");

    let rendered = error.to_string();
    assert!(matches!(error, A2aError::Invalid { .. }), "{rendered}");
    assert!(
        rendered.contains("nous") && rendered.contains("`url`"),
        "{rendered}"
    );
    assert!(rendered.contains("/repo/.basis/a2a.json"), "{rendered}");
}

#[test]
fn names_follow_the_mcp_server_name_rule() {
    for name in ["", "evil__foo", "evil_"] {
        let error = parse_text(&format!(
            r#"{{"agents":{{"{name}":{{"url":"https://x.example"}}}}}}"#
        ))
        .expect_err("mentra's tool-name rule refuses it");
        assert!(
            matches!(error, A2aError::Invalid { .. }),
            "{name:?}: {error}"
        );
    }

    assert!(parse_text(r#"{"agents":{"a-b_c":{"url":"https://x.example"}}}"#).is_ok());
}

#[test]
fn plaintext_credentials_to_a_remote_host_fail_at_parse() {
    let error = parse_with(
        Path::new("/repo/.basis/a2a.json"),
        r#"{"agents":{"nous":{"url":"http://example.com/?k=${TOKEN}","headers":{"Authorization":"Bearer ${TOKEN}"}}}}"#,
        &with_secret,
    )
    .expect_err("a committed file cannot grant plaintext passage");

    let rendered = error.to_string();
    assert!(matches!(error, A2aError::Invalid { .. }), "{rendered}");
    assert!(rendered.contains("plaintext"), "{rendered}");
    assert!(
        !rendered.contains("example.com") && !rendered.contains(SECRET),
        "nothing read from the file returns in an error: {rendered}"
    );
}

#[test]
fn loopback_plaintext_is_mentras_own_carve_out() {
    let agents = parse_text(
        r#"{"agents":{"local":{"url":"http://127.0.0.1:8080","headers":{"Authorization":"Bearer t"}}}}"#,
    )
    .expect("a local agent needs no ceremony");

    assert_eq!(agents[0].name(), "local");
}

#[test]
fn a_header_without_credentials_may_travel_plaintext() {
    // mentra refuses plaintext only when there is a header to protect.
    assert!(parse_text(r#"{"agents":{"open":{"url":"http://agents.example.com"}}}"#).is_ok());
}

#[test]
fn a_url_mentra_would_refuse_fails_at_parse() {
    for url in [
        "not a url",
        "ftp://x.example",
        "https://user:pass@x.example",
    ] {
        let error = parse_text(&format!(r#"{{"agents":{{"nous":{{"url":"{url}"}}}}}}"#))
            .expect_err("validation happens at the boundary, not mid-handshake");
        assert!(matches!(error, A2aError::Invalid { .. }), "{url}: {error}");
        assert!(!error.to_string().contains("pass"), "{error}");
    }
}

#[test]
fn the_version_header_is_mentras_to_set() {
    let error = parse_text(
        r#"{"agents":{"nous":{"url":"https://x.example","headers":{"A2A-Version":"0.3"}}}}"#,
    )
    .expect_err("the client speaks one version and says so itself");

    assert!(error.to_string().contains("A2A-Version"), "{error}");
}

#[test]
fn an_unset_placeholder_names_the_agent_and_the_field() {
    let error = parse_text(
        r#"{"agents":{"nous":{"url":"https://x.example","headers":{"Authorization":"Bearer ${NOUS_TOKEN}"}}}}"#,
    )
    .expect_err("an unset variable is an error");

    let rendered = error.to_string();
    assert!(rendered.contains("nous"), "{rendered}");
    assert!(rendered.contains("headers.Authorization"), "{rendered}");
    assert!(rendered.contains("NOUS_TOKEN"), "{rendered}");
}

#[test]
fn no_error_repeats_a_value_from_the_file() {
    let broken = [
        // An unterminated placeholder inside a credential.
        format!(
            r#"{{"agents":{{"a":{{"url":"https://x.example","headers":{{"Authorization":"{SECRET}${{"}}}}}}}}"#
        ),
        // A header value HTTP cannot carry.
        format!(
            r#"{{"agents":{{"b":{{"url":"https://x.example","headers":{{"Authorization":"{SECRET}\n"}}}}}}}}"#
        ),
        // serde's own message would quote this one.
        format!(r#"{{"agents":{{"c":{{"url":"https://x.example","headers":"{SECRET}"}}}}}}"#),
        format!(r#"{{"agents":{{"d":{{"url":["{SECRET}"]}}}}}}"#),
    ];

    for text in broken {
        let error = parse_text(&text).expect_err("each of these fails");
        assert!(
            !error.to_string().contains(SECRET),
            "a value leaked into: {error}"
        );
    }
}

#[test]
fn an_agents_debug_does_not_print_an_expanded_credential() {
    let agents = parse_with(
        Path::new("/repo/.basis/a2a.json"),
        r#"{"agents":{"nous":{"url":"https://nous.example.com/card.json?key=${TOKEN}","headers":{"Authorization":"${TOKEN}"}}}}"#,
        &with_secret,
    )
    .expect("the token is set");

    let rendered = format!("{:?}", agents[0]);
    assert!(
        !rendered.contains(SECRET),
        "an expanded credential reached Debug: {rendered}"
    );
    assert!(
        rendered.contains("https://nous.example.com/card.json")
            && rendered.contains("Authorization"),
        "the redaction must not hide where the agent is or which header was set: {rendered}"
    );
}

#[test]
fn agents_come_back_in_a_stable_order() {
    let agents = parse_text(
        r#"{"agents":{"zeta":{"url":"https://z.example"},"alpha":{"url":"https://a.example"}}}"#,
    )
    .expect("a well-formed file");

    let names: Vec<&str> = agents.iter().map(A2aAgent::name).collect();
    assert_eq!(
        names,
        ["alpha", "zeta"],
        "connect order must not depend on hashing"
    );
}
