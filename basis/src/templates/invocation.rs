//! Reading `/name args` off the front of a prompt.
//!
//! The other half of the `/command` convention: [`discovery`](super::discovery)
//! says which names exist, and this says when a line is asking for one. It
//! lives here rather than in either surface because both of them ask — the
//! shell rewrites a `/name` before spawning, and the ACP server has to
//! recognize its own built-ins in a prompt a client typed — and a convention
//! two crates read differently is two conventions.
//!
//! # The rule, and why it is total
//!
//! The first token, and nothing else. A template's name never contains `/` —
//! nesting is namespacing and joins with
//! [`NAMESPACE_SEPARATOR`](super::NAMESPACE_SEPARATOR) — so a first token with
//! a second slash is a path rather than a command, and
//! `"/usr/bin/x crashes on startup"` is a bug report that passes through
//! untouched. That is what lets the rule be applied to every prompt without an
//! escape for the common case.
//!
//! What to do about a name that fits the shape and matches nothing is the
//! caller's, not this function's: a shell can refuse and list what exists,
//! where a protocol server may have nothing better to do than send the line on.

/// The characters a template name can hold: what a filename below the
/// templates root can hold, plus the `:` that joins its directories.
/// Deliberately excludes `/`, which is what makes a path pass through.
fn is_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '.' | '-')
}

/// The command name and arguments a prompt opens with, if it opens with one.
///
/// The arguments are trimmed and may be empty — a command invoked with nothing
/// is still an invocation.
pub fn invocation(prompt: &str) -> Option<(&str, &str)> {
    let rest = prompt.strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (name, arguments) = rest.split_at(end);

    (!name.is_empty() && name.chars().all(is_name_char)).then_some((name, arguments.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_token_is_read_as_a_name() {
        assert_eq!(
            invocation("/fix auth login.rs"),
            Some(("fix", "auth login.rs"))
        );
        assert_eq!(invocation("/fix"), Some(("fix", "")));
        assert_eq!(
            invocation("/review\nthe diff below"),
            Some(("review", "the diff below")),
            "a multi-line prompt still opens with one token"
        );
        assert_eq!(invocation("fix /review"), None);
        assert_eq!(invocation("/"), None);
    }

    #[test]
    fn a_namespaced_name_is_one_token() {
        assert_eq!(
            invocation("/git:commit the parser fix"),
            Some(("git:commit", "the parser fix"))
        );
    }

    /// A template name never contains `/`, so a first token that does is a
    /// path. This is the whole reason the rule can be applied to every prompt
    /// without an escape for the common case.
    #[test]
    fn a_path_is_a_path_and_passes_straight_through() {
        for prose in [
            "/usr/bin/x crashes on startup",
            "/ is the root directory",
            "//comment syntax",
            "look at /etc/hosts",
        ] {
            assert_eq!(invocation(prose), None, "{prose} is prose");
        }
    }
}
