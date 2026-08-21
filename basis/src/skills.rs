//! Discovery of the skills directories.
//!
//! Skills are on-demand instructions the model loads by name — description
//! first, body only when it asks. mentra owns the loading (frontmatter
//! parsing, dedup, the `load_skill` tool); basis owns *where to look*, which is
//! the convention half.
//!
//! # Four roots, one order
//!
//! The whole point of the SKILL.md format is that a skill written once is found
//! by every harness that speaks it, so *where to look* is not basis's to invent:
//! `.agents/skills/` and `~/.agents/skills/` are the directories pi and Claude
//! Code already read, and a repository that carries one is making a statement
//! basis has no reason to ignore. `.basis/skills/` and the global config
//! directory's `skills/` are what basis added on top, and they stay — a
//! workspace that wants skills for *this* harness alone has to be able to say
//! so.
//!
//! [`discover`] returns every one that exists, most specific first:
//!
//! 1. `<workspace>/.basis/skills` — this repository, this harness
//! 2. `<workspace>/.agents/skills` — this repository, any harness
//! 3. `<global config dir>/skills` — this user, this harness
//! 4. `$HOME/.agents/skills` — this user, any harness
//!
//! Within each scope the basis-specific root comes first because naming basis
//! is the more specific statement: a directory that says *this harness* was
//! written knowing which harness would read it. Roots layer rather than
//! replace (`Runtime::register_skills_dirs` is additive, and an earlier root's
//! name wins), so a repository shadows a personal skill by name and inherits
//! everything it did not shadow.

use std::path::{Path, PathBuf};

use crate::context::ContextScope;

/// Where basis looks inside a workspace, relative to its root.
pub const DEFAULT_WORKSPACE_SKILLS_DIR: &str = ".basis/skills";

/// The directory other harnesses share, relative to the workspace root and to
/// `$HOME`. One constant because it is one convention: the same spelling names
/// the project scope and the user scope, which is what makes it portable.
pub const SHARED_SKILLS_DIR: &str = ".agents/skills";

/// Where basis looks inside the global config directory.
pub const DEFAULT_GLOBAL_SKILLS_DIR: &str = "skills";

/// How to look for skills.
///
/// Two knobs, four roots: the shared `.agents/skills` spellings are not
/// configurable because they are not basis's to name — a fixed path is what
/// makes them shared. What a caller still decides is which basis-specific root
/// to use and whether the user's personal scope is read at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsConfig {
    /// Path relative to the workspace root.
    pub workspace_subdir: PathBuf,
    /// The global config directory, if any. `skills/` inside it is used.
    ///
    /// Also the switch for the personal scope as a whole: `None` means *read
    /// no directory of this user's*, which is how every offline test in this
    /// repository keeps discovery off the developer's own machine, and
    /// `$HOME/.agents/skills` is that same scope under the shared spelling.
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
/// Precedence order, because that is the order
/// `Runtime::register_skills_dirs` wants: an earlier root's skill shadows the
/// same name in a later one.
pub fn discover(workspace: &Path, config: &SkillsConfig) -> Vec<SkillsSource> {
    discover_in(workspace, config, user_home().as_deref())
}

/// [`discover`] with the user's home directory passed in rather than read from
/// the environment, so a test can state one instead of inheriting whichever
/// machine it runs on.
fn discover_in(workspace: &Path, config: &SkillsConfig, home: Option<&Path>) -> Vec<SkillsSource> {
    let mut sources = Vec::new();

    push(
        &mut sources,
        workspace.join(&config.workspace_subdir),
        ContextScope::Workspace,
    );
    push(
        &mut sources,
        workspace.join(SHARED_SKILLS_DIR),
        ContextScope::Workspace,
    );

    // The personal scope, in both of its spellings. `global_dir` gates the
    // shared root without locating it: `~/.agents/skills` is a fixed path by
    // convention, not somewhere a host relocates, so the only question a
    // caller can answer about it is whether this user's directories are read
    // at all — and `None` is how a caller already says no.
    if let Some(global) = &config.global_dir {
        push(
            &mut sources,
            global.join(DEFAULT_GLOBAL_SKILLS_DIR),
            ContextScope::Global,
        );
        if let Some(home) = home {
            push(
                &mut sources,
                home.join(SHARED_SKILLS_DIR),
                ContextScope::Global,
            );
        }
    }

    sources
}

/// Appends `path` when it is a directory no earlier root already names.
///
/// Two roots can reach one directory — a symlink, a global config directory
/// that *is* the workspace, a `$HOME` that is the workspace root. Registering
/// the same directory twice is a duplicate-name error in mentra and noise in
/// the run header either way, and the first mention is the stronger one, so
/// the later mention is what goes.
fn push(sources: &mut Vec<SkillsSource>, path: PathBuf, scope: ContextScope) {
    if !path.is_dir()
        || sources
            .iter()
            .any(|source| crate::paths::same_dir(&source.path, &path))
    {
        return;
    }

    sources.push(SkillsSource { path, scope });
}

/// `$HOME`, the only place the shared user root can be. `None` leaves that
/// root out rather than guessing at one.
fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
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

    /// Every test states its own `$HOME`. Reading the real one would make the
    /// results depend on whether whoever runs the suite keeps skills there.
    fn found(workspace: &Path, config: &SkillsConfig, home: Option<&Path>) -> Vec<SkillsSource> {
        discover_in(workspace, config, home)
    }

    fn dir(parent: &Path, relative: &str) -> PathBuf {
        let path = parent.join(relative);
        std::fs::create_dir_all(&path).expect("create dir");
        path
    }

    #[test]
    fn nothing_on_disk_means_no_sources() {
        let tmp = tempfile::tempdir().expect("tempdir");

        assert!(found(tmp.path(), &config(None), None).is_empty());
    }

    #[test]
    fn a_workspace_directory_is_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills = dir(tmp.path(), DEFAULT_WORKSPACE_SKILLS_DIR);

        let sources = found(tmp.path(), &config(None), None);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].scope, ContextScope::Workspace);
        assert_eq!(sources[0].path, skills);
    }

    #[test]
    fn the_shared_workspace_directory_is_found() {
        // The point of the SKILL.md format: a repository writes `.agents/skills`
        // once and every harness that speaks the format reads it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let shared = dir(tmp.path(), SHARED_SKILLS_DIR);

        let sources = found(tmp.path(), &config(None), None);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].scope, ContextScope::Workspace);
        assert_eq!(sources[0].path, shared);
    }

    #[test]
    fn the_basis_directory_outranks_the_shared_one_because_it_names_basis() {
        // Both roots describe the same repository, so the tie is broken by
        // which is the more specific statement: `.basis/skills` was written
        // knowing which harness would read it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let basis = dir(tmp.path(), DEFAULT_WORKSPACE_SKILLS_DIR);
        let shared = dir(tmp.path(), SHARED_SKILLS_DIR);

        let sources = found(tmp.path(), &config(None), None);

        assert_eq!(
            sources
                .iter()
                .map(|source| &source.path)
                .collect::<Vec<_>>(),
            vec![&basis, &shared],
            "registration is strongest-first, so a name defined in both loads from .basis/skills"
        );
    }

    #[test]
    fn the_workspace_directory_outranks_the_global_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        dir(tmp.path(), DEFAULT_WORKSPACE_SKILLS_DIR);
        let global = tmp.path().join("global");
        dir(&global, DEFAULT_GLOBAL_SKILLS_DIR);

        let sources = found(tmp.path(), &config(Some(global)), None);

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].scope, ContextScope::Workspace);
        assert_eq!(sources[1].scope, ContextScope::Global);
    }

    #[test]
    fn a_global_directory_alone_is_used() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        dir(&global, DEFAULT_GLOBAL_SKILLS_DIR);

        let sources = found(tmp.path(), &config(Some(global)), None);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].scope, ContextScope::Global);
    }

    #[test]
    fn the_shared_user_directory_is_found_behind_the_basis_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        let home = tmp.path().join("home");
        let basis_global = dir(&global, DEFAULT_GLOBAL_SKILLS_DIR);
        let shared_user = dir(&home, SHARED_SKILLS_DIR);

        let sources = found(tmp.path(), &config(Some(global)), Some(&home));

        assert_eq!(
            sources
                .iter()
                .map(|source| &source.path)
                .collect::<Vec<_>>(),
            vec![&basis_global, &shared_user]
        );
        assert!(
            sources
                .iter()
                .all(|source| source.scope == ContextScope::Global)
        );
    }

    #[test]
    fn all_four_roots_come_back_most_specific_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        let global = tmp.path().join("global");
        let home = tmp.path().join("home");

        let expected = [
            dir(&workspace, DEFAULT_WORKSPACE_SKILLS_DIR),
            dir(&workspace, SHARED_SKILLS_DIR),
            dir(&global, DEFAULT_GLOBAL_SKILLS_DIR),
            dir(&home, SHARED_SKILLS_DIR),
        ];

        let sources = found(&workspace, &config(Some(global)), Some(&home));

        assert_eq!(
            sources
                .iter()
                .map(|source| &source.path)
                .collect::<Vec<_>>(),
            expected.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_missing_root_is_simply_absent() {
        // Three of the four exist; the fourth leaves no hole and no error.
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        let global = tmp.path().join("global");
        let home = tmp.path().join("home");

        let basis = dir(&workspace, DEFAULT_WORKSPACE_SKILLS_DIR);
        let basis_global = dir(&global, DEFAULT_GLOBAL_SKILLS_DIR);
        let shared_user = dir(&home, SHARED_SKILLS_DIR);

        let sources = found(&workspace, &config(Some(global)), Some(&home));

        assert_eq!(
            sources
                .iter()
                .map(|source| &source.path)
                .collect::<Vec<_>>(),
            vec![&basis, &basis_global, &shared_user]
        );
    }

    #[test]
    fn no_personal_scope_means_no_shared_user_directory_either() {
        // `global_dir: None` is how a caller says *read none of this user's
        // directories*; honoring it for one spelling and not the other would
        // read the machine the caller was keeping out.
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        dir(&home, SHARED_SKILLS_DIR);

        assert!(found(tmp.path(), &config(None), Some(&home)).is_empty());
    }

    #[test]
    fn a_file_where_the_directory_should_be_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills = tmp.path().join(DEFAULT_WORKSPACE_SKILLS_DIR);
        std::fs::create_dir_all(skills.parent().expect("parent")).expect("create .basis");
        std::fs::write(&skills, "not a directory").expect("write file");

        assert!(found(tmp.path(), &config(None), None).is_empty());
    }

    #[test]
    fn the_same_directory_reached_twice_is_reported_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        dir(&global, DEFAULT_GLOBAL_SKILLS_DIR);

        // Point the workspace subdir at the very same place.
        let sources = found(
            &global,
            &SkillsConfig {
                workspace_subdir: PathBuf::from(DEFAULT_GLOBAL_SKILLS_DIR),
                global_dir: Some(global.clone()),
            },
            None,
        );

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].scope, ContextScope::Workspace);
    }

    #[test]
    fn a_home_that_is_the_workspace_does_not_register_one_directory_twice() {
        // The shared spelling is the same in both scopes, so a workspace
        // opened at `$HOME` reaches `.agents/skills` from either side.
        let tmp = tempfile::tempdir().expect("tempdir");
        let shared = dir(tmp.path(), SHARED_SKILLS_DIR);
        let global = tmp.path().join("global");
        dir(&global, DEFAULT_GLOBAL_SKILLS_DIR);

        let sources = found(tmp.path(), &config(Some(global)), Some(tmp.path()));

        assert_eq!(
            sources
                .iter()
                .filter(|source| source.path == shared)
                .count(),
            1
        );
        assert_eq!(sources[0].scope, ContextScope::Workspace);
    }
}
