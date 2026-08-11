//! Discovery of the skills directory.
//!
//! Skills are on-demand instructions the model loads by name — description
//! first, body only when it asks. mentra owns the loading (frontmatter
//! parsing, dedup, the `load_skill` tool); lan owns *where to look*, which is
//! the convention half.
//!
//! # One directory, for now
//!
//! `Runtime::register_skill_loader` replaces rather than merges, so
//! registering two directories keeps only the second
//! ([oops-rs/mentra#8](https://github.com/oops-rs/mentra/issues/8)). lan
//! therefore registers exactly one — the most specific that exists — instead
//! of merging directories itself, which would mean reimplementing the
//! frontmatter parsing mentra already does correctly (ADR-0005: no lan-side
//! workarounds for mentra-shaped holes).
//!
//! When that issue lands, [`discover`] becomes "collect all of them in
//! precedence order" and the caller registers the list. The scope ordering
//! here is already the one that change would use.

use std::path::{Path, PathBuf};

use crate::context::ContextScope;

/// Where lan looks inside a workspace, relative to its root.
pub const DEFAULT_WORKSPACE_SKILLS_DIR: &str = ".lan/skills";

/// Where lan looks inside the global config directory.
pub const DEFAULT_GLOBAL_SKILLS_DIR: &str = "skills";

/// How to look for skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsConfig {
    /// Path relative to the workspace root.
    pub workspace_subdir: PathBuf,
    /// The global config directory, if any. `skills/` inside it is used.
    pub global_dir: Option<PathBuf>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            workspace_subdir: PathBuf::from(DEFAULT_WORKSPACE_SKILLS_DIR),
            global_dir: crate::context::ContextConfig::default().global_dir,
        }
    }
}

/// A skills directory that exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsSource {
    pub path: PathBuf,
    pub scope: ContextScope,
}

/// Every skills directory that exists, most specific first.
///
/// Returned in precedence order so the caller can take the first and still be
/// correct once multi-root registration is possible.
pub fn discover(workspace: &Path, config: &SkillsConfig) -> Vec<SkillsSource> {
    let mut sources = Vec::new();

    let workspace_dir = workspace.join(&config.workspace_subdir);
    if workspace_dir.is_dir() {
        sources.push(SkillsSource {
            path: workspace_dir,
            scope: ContextScope::Workspace,
        });
    }

    if let Some(global) = &config.global_dir {
        let global_dir = global.join(DEFAULT_GLOBAL_SKILLS_DIR);
        // A global directory that *is* the workspace one is not a second
        // source; registering it twice would just be noise in the report.
        if global_dir.is_dir()
            && !sources
                .iter()
                .any(|source| crate::paths::same_dir(&source.path, &global_dir))
        {
            sources.push(SkillsSource {
                path: global_dir,
                scope: ContextScope::Global,
            });
        }
    }

    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(global: Option<PathBuf>) -> SkillsConfig {
        SkillsConfig {
            workspace_subdir: PathBuf::from(DEFAULT_WORKSPACE_SKILLS_DIR),
            global_dir: global,
        }
    }

    #[test]
    fn nothing_on_disk_means_no_sources() {
        let tmp = tempfile::tempdir().expect("tempdir");

        assert!(discover(tmp.path(), &config(None)).is_empty());
    }

    #[test]
    fn a_workspace_directory_is_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills = tmp.path().join(DEFAULT_WORKSPACE_SKILLS_DIR);
        std::fs::create_dir_all(&skills).expect("create skills dir");

        let found = discover(tmp.path(), &config(None));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Workspace);
        assert_eq!(found[0].path, skills);
    }

    #[test]
    fn the_workspace_directory_outranks_the_global_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace_skills = tmp.path().join(DEFAULT_WORKSPACE_SKILLS_DIR);
        let global = tmp.path().join("global");
        std::fs::create_dir_all(&workspace_skills).expect("create workspace skills");
        std::fs::create_dir_all(global.join(DEFAULT_GLOBAL_SKILLS_DIR)).expect("create global");

        let found = discover(tmp.path(), &config(Some(global)));

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].scope, ContextScope::Workspace);
        assert_eq!(found[1].scope, ContextScope::Global);
    }

    #[test]
    fn a_global_directory_alone_is_used() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        std::fs::create_dir_all(global.join(DEFAULT_GLOBAL_SKILLS_DIR)).expect("create global");

        let found = discover(tmp.path(), &config(Some(global)));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Global);
    }

    #[test]
    fn a_file_where_the_directory_should_be_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills = tmp.path().join(DEFAULT_WORKSPACE_SKILLS_DIR);
        std::fs::create_dir_all(skills.parent().expect("parent")).expect("create .lan");
        std::fs::write(&skills, "not a directory").expect("write file");

        assert!(discover(tmp.path(), &config(None)).is_empty());
    }

    #[test]
    fn the_same_directory_reached_twice_is_reported_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        let global_skills = global.join(DEFAULT_GLOBAL_SKILLS_DIR);
        std::fs::create_dir_all(&global_skills).expect("create global skills");

        // Point the workspace subdir at the very same place.
        let found = discover(
            &global,
            &SkillsConfig {
                workspace_subdir: PathBuf::from(DEFAULT_GLOBAL_SKILLS_DIR),
                global_dir: Some(global.clone()),
            },
        );

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Workspace);
    }
}
