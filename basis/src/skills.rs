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
//!
//! # A root is held, not given away
//!
//! Registration is on the *runtime*, which on a shared one (ADR-0018) outlives
//! every workspace opened on it. So a workspace holds its roots rather than
//! handing them over: an internal hold releases them when the `Workspace` drops,
//! and the runtime takes a root off mentra's registry once its last holder
//! goes. What that buys is a host — an editor server, an ACP session — that
//! can close a repository without its skills outliving it, and without taking
//! the user's own `~/.agents/skills` away from the repositories still open.
//!
//! # `disable-model-invocation`
//!
//! A `SKILL.md` may set `disable-model-invocation: true` in its frontmatter
//! (the underscore spelling is accepted too). mentra then leaves the skill out
//! of the list the model is shown and refuses it from `load_skill`, while
//! keeping it in `Runtime::skills()` with `model_invocable: false` — a skill a
//! person invokes deliberately rather than one a model reaches for.
//!
//! basis reads the flag nowhere: discovery is about *where to look*, and what
//! a `SKILL.md` says is mentra's to parse. What basis does is carry the answer
//! out, on [`LoadedSkill::model_invocable`](crate::run::LoadedSkill), so a host
//! listing a workspace's skills can tell which of them the model will never
//! reach. It is deliberately **not** offered as a `/name` command beside
//! templates: rendering one would mean re-reading and re-parsing the
//! `SKILL.md` basis handed mentra (mentra exposes the body of a skill it will
//! not invoke through no API at all), and giving it an argument convention
//! `SKILL.md` does not define — which is [`crate::templates`] a second time,
//! under a second set of rules.

mod registration;

use std::path::{Path, PathBuf};

use crate::context::ContextScope;

pub(crate) use registration::SkillRoots;

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
/// Four fields, one per root (D9): the shared `.agents/skills` roots carry no
/// path of their own — a fixed spelling is what makes them shared, and
/// relocating one would stop it being the convention `.agents/skills` names —
/// so those two are plain switches, on by default. The two basis-specific
/// roots carry the path *and* the switch in one `Option`: `None` disables the
/// root, `Some(path)` both enables it and says where. Every root disables
/// independently of the other three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsConfig {
    /// `<workspace>/.basis/skills`, relative to the workspace root. `None`
    /// disables this root alone.
    pub workspace_subdir: Option<PathBuf>,
    /// Whether `<workspace>/.agents/skills` — the convention `pi` and Claude
    /// Code already read — is one of the roots.
    pub shared_workspace_dir: bool,
    /// The global config directory, if any. `skills/` inside it is the third
    /// root; `None` disables that root alone and says nothing about
    /// [`shared_home_dir`](Self::shared_home_dir).
    pub global_dir: Option<PathBuf>,
    /// Whether `$HOME/.agents/skills` — the shared convention's personal
    /// scope — is one of the roots. Gated on its own switch rather than on
    /// [`global_dir`](Self::global_dir), unlike before D9: a caller can now
    /// keep this while disabling basis's own global root, or the reverse.
    pub shared_home_dir: bool,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            workspace_subdir: Some(PathBuf::from(DEFAULT_WORKSPACE_SKILLS_DIR)),
            shared_workspace_dir: true,
            global_dir: crate::context::ContextConfig::default().global_dir,
            shared_home_dir: true,
        }
    }
}

impl SkillsConfig {
    /// No skill discovery at all: none of the four roots is read, so nothing
    /// is registered on the runtime and the model is offered no `load_skill`.
    ///
    /// What `WorkspaceBuilder::without_discovery` leaves of this config.
    /// Skills are directories on disk with no host-supplied half, so there is
    /// nothing here for `none` to keep.
    pub fn none() -> Self {
        Self {
            workspace_subdir: None,
            shared_workspace_dir: false,
            global_dir: None,
            shared_home_dir: false,
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

    if let Some(subdir) = &config.workspace_subdir {
        push(
            &mut sources,
            workspace.join(subdir),
            ContextScope::Workspace,
        );
    }
    if config.shared_workspace_dir {
        push(
            &mut sources,
            workspace.join(SHARED_SKILLS_DIR),
            ContextScope::Workspace,
        );
    }

    if let Some(global) = &config.global_dir {
        push(
            &mut sources,
            global.join(DEFAULT_GLOBAL_SKILLS_DIR),
            ContextScope::Global,
        );
    }
    // `~/.agents/skills` is a fixed path by convention, not somewhere a host
    // relocates, so the only question a caller answers about it (D9) is
    // whether it is read at all — independently of the global root above,
    // unlike before D9, when one `Option` gated both.
    if config.shared_home_dir
        && let Some(home) = home
    {
        push(
            &mut sources,
            home.join(SHARED_SKILLS_DIR),
            ContextScope::Global,
        );
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

    /// The default three roots enabled, `global_dir` as given — the shape
    /// most tests below vary along exactly one axis.
    fn config(global: Option<PathBuf>) -> SkillsConfig {
        SkillsConfig {
            workspace_subdir: Some(PathBuf::from(DEFAULT_WORKSPACE_SKILLS_DIR)),
            shared_workspace_dir: true,
            global_dir: global,
            shared_home_dir: true,
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
    fn disabling_the_shared_home_root_alone_leaves_the_other_three() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        let global = tmp.path().join("global");
        let home = tmp.path().join("home");
        dir(&workspace, DEFAULT_WORKSPACE_SKILLS_DIR);
        dir(&global, DEFAULT_GLOBAL_SKILLS_DIR);
        dir(&home, SHARED_SKILLS_DIR);

        let sources = found(
            &workspace,
            &SkillsConfig {
                shared_home_dir: false,
                ..config(Some(global))
            },
            Some(&home),
        );

        assert_eq!(sources.len(), 2, "workspace root and global root only");
        assert!(
            sources
                .iter()
                .all(|source| source.path != home.join(SHARED_SKILLS_DIR))
        );
    }

    #[test]
    fn a_missing_global_dir_no_longer_silences_the_home_root() {
        // Before D9 one `Option` gated both the global root and the home
        // root; each disables on its own switch now, so a caller can keep
        // this one while turning the other off, or the reverse.
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let shared_user = dir(&home, SHARED_SKILLS_DIR);

        let sources = found(tmp.path(), &config(None), Some(&home));

        assert_eq!(
            sources,
            vec![SkillsSource {
                path: shared_user,
                scope: ContextScope::Global,
            }]
        );
    }

    #[test]
    fn disabling_the_global_dir_alone_leaves_the_home_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        let home = tmp.path().join("home");
        dir(&global, DEFAULT_GLOBAL_SKILLS_DIR);
        let shared_user = dir(&home, SHARED_SKILLS_DIR);

        let sources = found(
            tmp.path(),
            &SkillsConfig {
                global_dir: None,
                ..config(Some(global))
            },
            Some(&home),
        );

        assert_eq!(
            sources,
            vec![SkillsSource {
                path: shared_user,
                scope: ContextScope::Global,
            }]
        );
    }

    #[test]
    fn disabling_the_basis_workspace_root_alone_leaves_the_shared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        dir(tmp.path(), DEFAULT_WORKSPACE_SKILLS_DIR);
        let shared = dir(tmp.path(), SHARED_SKILLS_DIR);

        let sources = found(
            tmp.path(),
            &SkillsConfig {
                workspace_subdir: None,
                ..config(None)
            },
            None,
        );

        assert_eq!(
            sources,
            vec![SkillsSource {
                path: shared,
                scope: ContextScope::Workspace,
            }]
        );
    }

    #[test]
    fn disabling_the_shared_workspace_root_alone_leaves_the_basis_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let basis = dir(tmp.path(), DEFAULT_WORKSPACE_SKILLS_DIR);
        dir(tmp.path(), SHARED_SKILLS_DIR);

        let sources = found(
            tmp.path(),
            &SkillsConfig {
                shared_workspace_dir: false,
                ..config(None)
            },
            None,
        );

        assert_eq!(
            sources,
            vec![SkillsSource {
                path: basis,
                scope: ContextScope::Workspace,
            }]
        );
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
                workspace_subdir: Some(PathBuf::from(DEFAULT_GLOBAL_SKILLS_DIR)),
                shared_workspace_dir: false,
                global_dir: Some(global.clone()),
                shared_home_dir: false,
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
