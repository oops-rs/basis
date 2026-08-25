//! Splitting a markdown file into YAML frontmatter and body.
//!
//! The delimiter rules are markdown frontmatter's, not basis's: a leading
//! `---` line opens, the next bare `---` line closes, and everything after it
//! is the body. Two conventions read this shape — prompt templates
//! ([`crate::templates`]) and memory files ([`crate::memory`]) — and the
//! scanner lives here once so the two cannot drift on delimiters, byte-order
//! marks, or line endings. What the frontmatter *means* stays with each
//! caller: the keys, the required fields, and the error naming the file are
//! per-convention questions this module does not answer.
//!
//! mentra parses the same shape for `SKILL.md`, privately; a public upstream
//! frontmatter function is a pending mentra ask, and this module is what it
//! would replace.

/// A file split at its frontmatter delimiters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scanned<'a> {
    /// The raw YAML between the delimiters, or `None` when the file never
    /// opened frontmatter and is all body.
    pub frontmatter: Option<&'a str>,
    pub body: &'a str,
}

/// A file that opened frontmatter and never closed it — an error, not a file
/// whose body happens to start with `---`. Carries no path: the caller is the
/// one holding a file name to blame, and blames it in its own error type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unterminated;

/// Splits `raw` into its frontmatter and the body below it.
///
/// A file with no frontmatter is not an error here — it is all body. Whether
/// that is *usable* is the caller's question.
pub(crate) fn scan(raw: &str) -> Result<Scanned<'_>, Unterminated> {
    // A byte-order mark would keep the `---` from starting the file, turning
    // frontmatter into prose and the failure into a confusing one.
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);

    let Some(opening) = raw
        .strip_prefix("---\r\n")
        .or_else(|| raw.strip_prefix("---\n"))
    else {
        return Ok(Scanned {
            frontmatter: None,
            body: raw,
        });
    };

    let mut cursor = 0usize;
    for segment in opening.split_inclusive('\n') {
        if segment.trim_end_matches(['\n', '\r']) == "---" {
            return Ok(Scanned {
                frontmatter: Some(&opening[..cursor]),
                body: &opening[cursor + segment.len()..],
            });
        }
        cursor += segment.len();
    }

    // The closing delimiter can also be the last line, with no newline after
    // it; then the body is empty rather than missing.
    if opening[cursor..].trim_end_matches('\r') == "---" {
        return Ok(Scanned {
            frontmatter: Some(&opening[..cursor]),
            body: "",
        });
    }

    Err(Unterminated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_frontmatter_is_all_body() {
        let scanned = scan("just prose\n").expect("scans");

        assert_eq!(scanned.frontmatter, None);
        assert_eq!(scanned.body, "just prose\n");
    }

    #[test]
    fn frontmatter_is_separated_from_the_body() {
        let scanned = scan("---\nkey: value\n---\nbody\n").expect("scans");

        assert_eq!(scanned.frontmatter, Some("key: value\n"));
        assert_eq!(scanned.body, "body\n");
    }

    #[test]
    fn crlf_line_endings_are_understood() {
        let scanned = scan("---\r\nkey: value\r\n---\r\nbody\r\n").expect("scans");

        assert_eq!(scanned.frontmatter, Some("key: value\r\n"));
        assert_eq!(scanned.body, "body\r\n");
    }

    #[test]
    fn a_leading_byte_order_mark_does_not_hide_the_frontmatter() {
        let scanned = scan("\u{feff}---\nkey: value\n---\nbody\n").expect("scans");

        assert_eq!(scanned.frontmatter, Some("key: value\n"));
    }

    #[test]
    fn a_closing_delimiter_at_end_of_file_leaves_an_empty_body() {
        let scanned = scan("---\nkey: value\n---").expect("scans");

        assert_eq!(scanned.frontmatter, Some("key: value\n"));
        assert_eq!(scanned.body, "");
    }

    #[test]
    fn unterminated_frontmatter_is_an_error() {
        assert_eq!(scan("---\nkey: value\nbody\n"), Err(Unterminated));
    }

    #[test]
    fn empty_frontmatter_scans_as_an_empty_block_not_as_none() {
        // `---\n---\n` is a person declaring nothing; whether nothing is
        // enough is the caller's ruling, so the block has to come back
        // distinguishable from a file that never opened one.
        let scanned = scan("---\n---\nbody\n").expect("scans");

        assert_eq!(scanned.frontmatter, Some(""));
        assert_eq!(scanned.body, "body\n");
    }
}
