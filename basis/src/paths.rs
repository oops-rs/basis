//! Small shared helpers for the workspace conventions.
//!
//! `skills`, `templates`, `hooks`, and `mcp` all answer the same shape of
//! question — which directories exist, which of them are really the same
//! directory, which name wins when two roots offer it. Anything more than one
//! of them needs lives here rather than being copied, so the conventions
//! cannot quietly drift apart from each other.

use std::path::{Path, PathBuf};

/// The workspace-relative candidate a config named, or `None` when it named
/// nothing to look for.
///
/// An empty relative path is how a config says *probe no file here* — the same
/// spelling [`ContextConfig::none`](crate::ContextConfig::none) uses for its
/// file name, and what `WorkspaceBuilder::without_discovery` rewrites each
/// convention's workspace path to. Joining it instead would name the workspace
/// directory itself: harmless for a candidate that must be a file, and wrong
/// for one that may be a directory, since the repository root is not a
/// templates root.
pub(crate) fn candidate(workspace: &Path, relative: &Path) -> Option<PathBuf> {
    (!relative.as_os_str().is_empty()).then(|| workspace.join(relative))
}

/// Whether two paths name the same directory, following symlinks when both
/// resolve.
///
/// Falls back to a literal comparison so a path that cannot be canonicalized —
/// one that does not exist yet, or that this process cannot stat — is still
/// compared rather than assumed distinct. Assuming distinct would register the
/// same root twice, which for skills is a duplicate-name error and for
/// templates is a silent self-shadow.
pub(crate) fn same_dir(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn an_empty_relative_path_names_no_candidate() {
        assert_eq!(candidate(Path::new("/repo"), Path::new("")), None);
    }

    #[test]
    fn a_named_relative_path_joins_the_workspace() {
        assert_eq!(
            candidate(Path::new("/repo"), Path::new(".basis/hooks.json")),
            Some(PathBuf::from("/repo/.basis/hooks.json"))
        );
    }

    #[test]
    fn a_directory_is_itself() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert!(same_dir(dir.path(), dir.path()));
    }

    #[test]
    fn two_spellings_of_one_directory_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indirect = dir.path().join("child").join("..");
        std::fs::create_dir(dir.path().join("child")).expect("create child");

        assert!(
            same_dir(dir.path(), &indirect),
            "a path that resolves to the same place is the same place"
        );
    }

    #[test]
    fn different_directories_do_not_match() {
        let left = tempfile::tempdir().expect("tempdir");
        let right = tempfile::tempdir().expect("tempdir");

        assert!(!same_dir(left.path(), right.path()));
    }

    #[test]
    fn unresolvable_paths_fall_back_to_comparing_as_written() {
        let missing = PathBuf::from("/definitely/not/a/real/path");

        assert!(same_dir(&missing, &missing));
        assert!(!same_dir(&missing, &PathBuf::from("/also/not/real")));
    }
}
