//! `${VAR}` substitution inside a `.mcp.json` entry.
//!
//! Every agent that reads an MCP file expands these, and the reason is that a
//! server's credential belongs in the environment rather than in a file people
//! commit. A reader that passed `${GITHUB_TOKEN}` through verbatim would hand
//! the server a password spelled `${GITHUB_TOKEN}` and leave the operator
//! reading a handshake failure to find out why — so basis expands, and treats an
//! unset variable with no default as an error rather than as an empty string.
//!
//! The lookup is a parameter rather than a call to [`std::env::var`] so the
//! rules below are testable without mutating the process environment.
//!
//! # Nothing here repeats what it read
//!
//! The values this walks are the ones most likely to be a credential, so an
//! error names the *variable* it could not resolve and never the text it was
//! resolving. `${` with no `}` is reported as exactly that, without quoting
//! the string it was found in — a half-expanded token is still a token.

/// Expands `${NAME}` and `${NAME:-fallback}` in `raw`.
///
/// `:-` follows the shell: the fallback applies when the variable is unset
/// *or* empty. A bare `$` is literal, because nothing in this format uses it.
///
/// The error is a reason fragment reading after "has a `<field>` value that";
/// the caller knows which file, which server, and which field it belongs to.
pub(super) fn expand(raw: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut expanded = String::with_capacity(raw.len());
    let mut rest = raw;

    while let Some(open) = rest.find("${") {
        expanded.push_str(&rest[..open]);

        let placeholder = &rest[open + 2..];
        let Some(close) = placeholder.find('}') else {
            return Err("leaves a `${` placeholder unterminated".to_string());
        };

        let (name, fallback) = match placeholder[..close].split_once(":-") {
            Some((name, fallback)) => (name.trim(), Some(fallback)),
            None => (placeholder[..close].trim(), None),
        };

        let resolved = match lookup(name) {
            // An empty value is a real value unless a fallback was offered,
            // which is exactly what `:-` means in a shell.
            Some(value) if !value.is_empty() || fallback.is_none() => Some(value),
            _ => fallback.map(str::to_string),
        };

        match resolved {
            Some(value) => expanded.push_str(&value),
            None if name.is_empty() => {
                return Err("contains an empty `${}` placeholder".to_string());
            }
            None => {
                return Err(format!(
                    "refers to `{name}`, which is not set in the environment and has no default"
                ));
            }
        }

        rest = &placeholder[close + 1..];
    }

    expanded.push_str(rest);
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();

        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn text_without_a_placeholder_is_unchanged() {
        let expanded = expand("npx", &env(&[])).expect("no placeholder to resolve");

        assert_eq!(expanded, "npx");
    }

    #[test]
    fn a_set_variable_is_substituted() {
        let expanded = expand("Bearer ${TOKEN}", &env(&[("TOKEN", "abc")])).expect("TOKEN is set");

        assert_eq!(expanded, "Bearer abc");
    }

    #[test]
    fn several_placeholders_are_all_substituted() {
        let expanded = expand(
            "${HOST}:${PORT}/mcp",
            &env(&[("HOST", "localhost"), ("PORT", "8080")]),
        )
        .expect("both are set");

        assert_eq!(expanded, "localhost:8080/mcp");
    }

    #[test]
    fn an_unset_variable_is_an_error_not_an_empty_string() {
        let error = expand("${TOKEN}", &env(&[])).expect_err("TOKEN is unset");

        assert!(error.contains("TOKEN"), "the name must be named: {error}");
    }

    #[test]
    fn a_fallback_covers_an_unset_variable() {
        let expanded = expand("${LEVEL:-info}", &env(&[])).expect("the fallback applies");

        assert_eq!(expanded, "info");
    }

    #[test]
    fn a_fallback_also_covers_an_empty_variable() {
        let expanded =
            expand("${LEVEL:-info}", &env(&[("LEVEL", "")])).expect("the fallback applies");

        assert_eq!(
            expanded, "info",
            "`:-` means unset or empty, as it does in a shell"
        );
    }

    #[test]
    fn an_empty_variable_without_a_fallback_expands_to_nothing() {
        let expanded =
            expand("[${LEVEL}]", &env(&[("LEVEL", "")])).expect("an empty value is a value");

        assert_eq!(expanded, "[]");
    }

    #[test]
    fn an_unterminated_placeholder_is_an_error() {
        let error = expand("${TOKEN", &env(&[("TOKEN", "abc")])).expect_err("no closing brace");

        assert!(error.contains("unterminated"), "{error}");
    }

    #[test]
    fn no_error_ever_repeats_the_text_it_was_expanding() {
        // `.mcp.json` is gitignored in most projects precisely because these
        // values are credentials. An error may name a variable; it may never
        // quote what it read.
        for broken in ["sk-live-secret${", "${}", "${MISSING}sk-live-secret"] {
            let error = expand(broken, &env(&[])).expect_err("each of these fails");

            assert!(
                !error.contains("sk-live-secret"),
                "the value leaked into {error:?}"
            );
        }
    }

    #[test]
    fn a_bare_dollar_is_literal() {
        let expanded = expand("$HOME/bin", &env(&[("HOME", "/root")])).expect("nothing to expand");

        assert_eq!(
            expanded, "$HOME/bin",
            "only the braced form is a placeholder"
        );
    }
}
