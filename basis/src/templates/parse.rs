//! Splitting a template file into frontmatter and prompt.
//!
//! The delimiter rules are markdown frontmatter's, not basis's: a leading `---`
//! line opens, the next bare `---` line closes, and everything after it is the
//! prompt. A file that opens frontmatter and never closes it is an error, not a
//! file whose prompt happens to start with `---`.

use std::path::Path;

use serde::Deserialize;

use super::TemplateError;
use crate::frontmatter;

/// The keys basis reads. Unknown keys are left alone — a template written for a
/// newer basis, or for another harness, should still load here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Frontmatter {
    pub description: Option<String>,
    /// `argument-hint` is the spelling the convention uses. The underscore
    /// form is accepted too, because YAML tempts people into it and a key basis
    /// merely ignored would look exactly like a hint that never displayed.
    #[serde(rename = "argument-hint", alias = "argument_hint")]
    pub argument_hint: Option<String>,
}

/// Splits `raw` into its frontmatter and the prompt below it.
///
/// A file with no frontmatter is not an error here — it is all prompt. Whether
/// that is *usable* is the caller's question, since it is the caller that needs
/// a description.
pub fn split(path: &Path, raw: &str) -> Result<(Frontmatter, String), TemplateError> {
    // The delimiter scanning — BOM, CRLF, the unterminated case — is shared
    // with the other frontmatter convention; see [`crate::frontmatter`].
    let scanned = frontmatter::scan(raw).map_err(|frontmatter::Unterminated| {
        TemplateError::InvalidFrontmatter {
            path: path.to_path_buf(),
            message: "missing closing frontmatter delimiter".to_string(),
        }
    })?;

    let meta = match scanned.frontmatter {
        None => Frontmatter::default(),
        Some(block) => parse(path, block)?,
    };

    Ok((meta, scanned.body.to_string()))
}

fn parse(path: &Path, frontmatter: &str) -> Result<Frontmatter, TemplateError> {
    // Empty frontmatter deserializes to YAML null, which is not a mapping and
    // so is not a `Frontmatter`. `---\n---\n` is a person declaring nothing,
    // not a person making a mistake.
    if frontmatter.trim().is_empty() {
        return Ok(Frontmatter::default());
    }

    serde_yaml_ng::from_str(frontmatter).map_err(|error| TemplateError::InvalidFrontmatter {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("/templates/example.md")
    }

    #[test]
    fn a_file_without_frontmatter_is_all_body() {
        let (meta, body) = split(&path(), "just a prompt\n").expect("split succeeds");

        assert_eq!(meta.description, None);
        assert_eq!(body, "just a prompt\n");
    }

    #[test]
    fn frontmatter_is_read_and_stripped() {
        let raw = "---\ndescription: Do a thing\nargument-hint: <file>\n---\nthe prompt\n";

        let (meta, body) = split(&path(), raw).expect("split succeeds");

        assert_eq!(meta.description.as_deref(), Some("Do a thing"));
        assert_eq!(meta.argument_hint.as_deref(), Some("<file>"));
        assert_eq!(body, "the prompt\n");
    }

    #[test]
    fn crlf_line_endings_are_understood() {
        let raw = "---\r\ndescription: Windows\r\n---\r\nthe prompt\r\n";

        let (meta, body) = split(&path(), raw).expect("split succeeds");

        assert_eq!(meta.description.as_deref(), Some("Windows"));
        assert_eq!(body, "the prompt\r\n");
    }

    #[test]
    fn a_leading_byte_order_mark_does_not_hide_the_frontmatter() {
        let raw = "\u{feff}---\ndescription: Encoded\n---\nbody\n";

        let (meta, _) = split(&path(), raw).expect("split succeeds");

        assert_eq!(meta.description.as_deref(), Some("Encoded"));
    }

    #[test]
    fn the_underscore_spelling_of_the_hint_is_accepted() {
        let raw = "---\ndescription: d\nargument_hint: <path>\n---\nbody\n";

        let (meta, _) = split(&path(), raw).expect("split succeeds");

        assert_eq!(meta.argument_hint.as_deref(), Some("<path>"));
    }

    #[test]
    fn unknown_keys_are_ignored_rather_than_rejected() {
        let raw = "---\ndescription: d\nfrom-the-future: yes\n---\nbody\n";

        let (meta, _) = split(&path(), raw).expect("split succeeds");

        assert_eq!(meta.description.as_deref(), Some("d"));
    }

    #[test]
    fn empty_frontmatter_is_a_declaration_of_nothing() {
        let (meta, body) = split(&path(), "---\n---\nbody\n").expect("split succeeds");

        assert_eq!(meta.description, None);
        assert_eq!(body, "body\n");
    }

    #[test]
    fn a_closing_delimiter_at_end_of_file_leaves_an_empty_body() {
        let (meta, body) = split(&path(), "---\ndescription: d\n---").expect("split succeeds");

        assert_eq!(meta.description.as_deref(), Some("d"));
        assert_eq!(body, "");
    }

    #[test]
    fn unterminated_frontmatter_is_an_error() {
        let error = split(&path(), "---\ndescription: d\nthe prompt\n").expect_err("rejected");

        assert!(matches!(error, TemplateError::InvalidFrontmatter { .. }));
        assert!(error.to_string().contains("closing"));
    }

    #[test]
    fn malformed_yaml_is_an_error_naming_the_file() {
        let raw = "---\ndescription: [unclosed\n---\nbody\n";

        let error = split(&path(), raw).expect_err("rejected");

        assert!(matches!(error, TemplateError::InvalidFrontmatter { .. }));
        assert!(error.to_string().contains("example.md"));
    }

    #[test]
    fn a_scalar_where_a_mapping_belongs_is_an_error() {
        // Not a mapping at all — silently treating this as "no frontmatter"
        // would hide a real mistake.
        let error = split(&path(), "---\njust a string\n---\nbody\n").expect_err("rejected");

        assert!(matches!(error, TemplateError::InvalidFrontmatter { .. }));
    }

    #[test]
    fn a_colon_inside_a_description_survives() {
        let raw = "---\ndescription: \"fix: the thing\"\n---\nbody\n";

        let (meta, _) = split(&path(), raw).expect("split succeeds");

        assert_eq!(meta.description.as_deref(), Some("fix: the thing"));
    }
}
