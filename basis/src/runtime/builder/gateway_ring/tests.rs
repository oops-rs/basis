//! Where a member's key comes from, and what an operator is shown about it.

use super::*;

fn exporting(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |name| {
        vars.iter()
            .find(|(var, _)| *var == name)
            .map(|(_, value)| (*value).to_string())
    }
}

#[test]
fn a_key_named_by_variable_is_read_at_build_and_trimmed() {
    let member = GatewayMember::new("http://x/").with_api_key_env("LOCAL_KEY");

    let key = member
        .resolve_key(0, &exporting(&[("LOCAL_KEY", " sk-local\n")]))
        .expect("set");

    assert_eq!(key.as_deref(), Some("sk-local"));
}

#[test]
fn a_named_variable_that_is_unset_or_blank_names_the_member_and_the_variable() {
    let member = GatewayMember::new("http://x/").with_api_key_env("LOCAL_KEY");

    for lookup in [exporting(&[]), exporting(&[("LOCAL_KEY", "  ")])] {
        let error = member.resolve_key(1, &lookup).expect_err("refused");

        assert!(matches!(
            &error,
            provider::ProviderError::GatewayCredentialMissing { member: 1, origin }
                if origin == "$LOCAL_KEY"
        ));
    }
}

#[test]
fn a_key_file_is_read_trimmed_and_an_empty_or_missing_one_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("key");
    std::fs::write(&path, "sk-file\n").expect("write");
    let nothing = exporting(&[]);

    let key = GatewayMember::new("http://x/")
        .with_api_key_file(&path)
        .resolve_key(0, &nothing)
        .expect("readable");
    assert_eq!(key.as_deref(), Some("sk-file"));

    std::fs::write(&path, "\n").expect("write");
    assert!(matches!(
        GatewayMember::new("http://x/")
            .with_api_key_file(&path)
            .resolve_key(0, &nothing),
        Err(provider::ProviderError::GatewayCredentialMissing { member: 0, .. })
    ));

    let error = GatewayMember::new("http://x/")
        .with_api_key_file(dir.path().join("absent"))
        .resolve_key(2, &nothing)
        .expect_err("unreadable");
    assert!(matches!(
        error,
        provider::ProviderError::GatewayCredentialUnreadable { member: 2, .. }
    ));
}

#[test]
fn a_member_prints_where_its_key_lives_and_never_the_key() {
    let printed = [
        GatewayMember::new("http://x/").with_api_key("sk-secret"),
        GatewayMember::new("http://x/").with_api_key_env("LOCAL_KEY"),
        GatewayMember::new("http://x/").with_api_key_file("/run/secrets/key"),
    ]
    .map(|member| format!("{member:?}"));

    assert!(printed[0].contains("<redacted>") && !printed[0].contains("sk-secret"));
    assert!(printed[1].contains("$LOCAL_KEY"));
    assert!(printed[2].contains("/run/secrets/key"));
}
