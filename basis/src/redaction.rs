//! Shared debug redaction for environment-shaped secrets and for the URLs
//! the two remote bridges — MCP and A2A — are configured with.

use std::collections::BTreeMap;

/// Keeps variable names while replacing every value with one fixed marker.
pub(crate) fn redacted_env<K>(names: impl IntoIterator<Item = K>) -> BTreeMap<K, &'static str>
where
    K: Ord,
{
    names.into_iter().map(|name| (name, "<redacted>")).collect()
}

/// A URL fit to print: scheme, host and path survive; userinfo and the query
/// string do not.
///
/// String surgery rather than a URL parser, deliberately: this must never
/// fail, because `Debug` is exactly where a malformed configuration goes to
/// be looked at, and a parse error here would trade one leak for another
/// (printing the raw string) or hide the field entirely.
#[cfg(any(feature = "mcp", feature = "a2a"))]
pub(crate) fn redacted_url(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, _)) => (base, "?[redacted]"),
        None => (url, ""),
    };

    // Userinfo sits between the scheme separator and the first `@` of the
    // authority, which ends at the first `/` after `://`.
    let base = match base.split_once("://") {
        Some((scheme, rest)) => {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            match rest[..authority_end].rfind('@') {
                Some(at) => format!("{scheme}://[redacted]@{}", &rest[at + 1..]),
                None => base.to_string(),
            }
        }
        None => base.to_string(),
    };

    format!("{base}{query}")
}

#[cfg(all(test, any(feature = "mcp", feature = "a2a")))]
mod tests {
    use super::*;

    #[test]
    fn a_redacted_url_keeps_the_address_and_drops_the_secrets() {
        assert_eq!(
            redacted_url("https://user:pass@example.com/mcp?key=sk-live"),
            "https://[redacted]@example.com/mcp?[redacted]"
        );
        assert_eq!(
            redacted_url("https://example.com/mcp"),
            "https://example.com/mcp",
            "an innocent URL passes through recognizably"
        );
        assert_eq!(
            redacted_url("not a url?token=x"),
            "not a url?[redacted]",
            "surgery must survive what a parser would refuse"
        );
    }
}
