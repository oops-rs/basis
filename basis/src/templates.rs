//! Discovery of prompt templates — the `/command` convention.
//!
//! A template is a markdown file whose body *is* a prompt, with optional YAML
//! frontmatter describing it. basis finds them, layers them, and substitutes
//! arguments into them; it does not decide what any of them are for
//! (PROPOSAL.md Bet 4). Over ACP each becomes an `AvailableCommand`, which is
//! the reason they carry names at all — that mapping lives in `basis-acp`, since
//! nothing here knows a protocol.
//!
//! # Where they come from
//!
//! `.basis/templates/` in the workspace, then `templates/` in the global config
//! directory: the same two roots, in the same precedence order, as
//! [`skills`](crate::skills). Roots layer rather than replace — a workspace
//! template shadows a personal one of the same name and everything else in the
//! personal root still loads. That is `PATH`'s rule, and it is the rule mentra
//! already applies to skills, so the two conventions cannot surprise each other.
//!
//! # Names
//!
//! A template's name is its path below the root with the `.md` dropped and
//! directories joined by `:` — `.basis/templates/git/commit.md` is `git:commit`.
//! Nesting is therefore namespacing, and two authors can each write a
//! `review.md` without colliding.
//!
//! # Errors, not silence
//!
//! A directory that is absent is not an error; nobody wrote any templates. A
//! *file* that exists and cannot be understood is: unterminated frontmatter,
//! YAML that does not parse, a missing description, two files claiming one
//! name. Skipping those would make "the template failed to load" and "the
//! template was never written" look identical to the person who wrote it.

mod discovery;
mod invocation;
mod parse;
mod render;

pub use invocation::invocation;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::context::ContextScope;

/// Where basis looks inside a workspace, relative to its root.
pub const DEFAULT_WORKSPACE_TEMPLATES_DIR: &str = ".basis/templates";

/// Where basis looks inside the global config directory.
pub const DEFAULT_GLOBAL_TEMPLATES_DIR: &str = "templates";

/// The extension a template file must have.
pub const TEMPLATE_EXTENSION: &str = "md";

/// What joins directory levels in a template's name.
pub const NAMESPACE_SEPARATOR: &str = ":";

/// How to look for templates. Every knob has a convention-shaped default; none
/// of them encode a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplatesConfig {
    /// Path relative to the workspace root.
    pub workspace_subdir: PathBuf,
    /// The global config directory, if any. `templates/` inside it is used.
    pub global_dir: Option<PathBuf>,
}

impl Default for TemplatesConfig {
    fn default() -> Self {
        Self {
            workspace_subdir: PathBuf::from(DEFAULT_WORKSPACE_TEMPLATES_DIR),
            global_dir: crate::context::default_global_dir(),
        }
    }
}

impl TemplatesConfig {
    /// No template discovery at all: neither `.basis/templates` nor the global
    /// directory is read.
    ///
    /// What `WorkspaceBuilder::without_discovery` leaves of this config.
    /// Templates are convention data with no host-supplied half, so there is
    /// nothing here for `none` to keep.
    pub fn none() -> Self {
        Self {
            workspace_subdir: PathBuf::new(),
            global_dir: None,
        }
    }
}

/// A templates directory that exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSource {
    pub path: PathBuf,
    pub scope: ContextScope,
}

/// One template, body included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// Path below the root, `.md` dropped, directories joined by
    /// [`NAMESPACE_SEPARATOR`].
    pub name: String,
    /// What the template is for. Required, because over ACP this is
    /// `AvailableCommand::description` and a command with nothing to say about
    /// itself cannot be picked out of a list.
    pub description: String,
    /// What to type after the name, when the author said. `None` stays `None`:
    /// a client shows this string verbatim, so inventing one would put basis's
    /// words in the author's mouth.
    pub argument_hint: Option<String>,
    /// The prompt, frontmatter stripped, before substitution.
    pub body: String,
    pub path: PathBuf,
    /// Which root this one came from, after shadowing.
    pub scope: ContextScope,
}

impl Template {
    /// The prompt this template produces for `args`.
    ///
    /// Substitution is one left-to-right pass over the body:
    ///
    /// - `$ARGUMENTS` — the whole argument string, trimmed.
    /// - `$1`, `$2`, … — arguments split on whitespace. A position nobody
    ///   supplied renders empty rather than failing: this is a prompt, and a
    ///   missing optional argument should leave a gap, not break the run.
    /// - `$$` — a literal `$`, and the only way to write one immediately
    ///   before `ARGUMENTS` or a digit.
    ///
    /// A body that references no placeholder at all is still given the
    /// arguments — appended after a blank line. Dropping them would mean a
    /// person typed something the model never saw. A body that references
    /// *any* placeholder is taken at its word and gets nothing extra, even
    /// when it used only some of what was supplied.
    ///
    /// Quoting is not interpreted. Arguments here are prose, and any quote
    /// rule worth the name would make `don't` change how the rest of the line
    /// splits; an argument that must keep its spaces belongs in `$ARGUMENTS`.
    pub fn render(&self, args: &str) -> String {
        render::render(&self.body, args)
    }
}

/// Anything that can go wrong while loading templates.
#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("failed to read templates directory {path}: {source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read template file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid template frontmatter in {path}: {message}")]
    InvalidFrontmatter { path: PathBuf, message: String },

    #[error("template {path} has no description; add `description:` to its frontmatter")]
    MissingDescription { path: PathBuf },

    #[error("duplicate template name '{name}' in {first_path} and {second_path}")]
    DuplicateName {
        name: String,
        first_path: PathBuf,
        second_path: PathBuf,
    },

    #[error("template path {path} is not valid UTF-8, so it cannot name a command")]
    NonUtf8Path { path: PathBuf },
}

/// Every templates directory that exists, most specific first.
///
/// Returned in precedence order, so a caller that wants only the strongest
/// root can take the first and still be correct.
pub fn discover(workspace: &Path, config: &TemplatesConfig) -> Vec<TemplateSource> {
    let mut sources = Vec::new();

    if let Some(workspace_dir) = crate::paths::candidate(workspace, &config.workspace_subdir)
        && workspace_dir.is_dir()
    {
        sources.push(TemplateSource {
            path: workspace_dir,
            scope: ContextScope::Workspace,
        });
    }

    if let Some(global) = &config.global_dir {
        let global_dir = global.join(DEFAULT_GLOBAL_TEMPLATES_DIR);
        // A global directory that *is* the workspace one is not a second
        // source; loading it twice would turn every template in it into a
        // duplicate of itself.
        if global_dir.is_dir()
            && !sources
                .iter()
                .any(|source| crate::paths::same_dir(&source.path, &global_dir))
        {
            sources.push(TemplateSource {
                path: global_dir,
                scope: ContextScope::Global,
            });
        }
    }

    sources
}

/// Every template the workspace defines, layered and ordered by name.
pub fn load(workspace: &Path, config: &TemplatesConfig) -> Result<Vec<Template>, TemplateError> {
    load_sources(&discover(workspace, config))
}

/// Loads from sources already in hand, for a host that chose the roots itself.
///
/// `sources` is read strongest-first: the first root to define a name keeps it.
pub fn load_sources(sources: &[TemplateSource]) -> Result<Vec<Template>, TemplateError> {
    discovery::load_sources(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(global: Option<PathBuf>) -> TemplatesConfig {
        TemplatesConfig {
            workspace_subdir: PathBuf::from(DEFAULT_WORKSPACE_TEMPLATES_DIR),
            global_dir: global,
        }
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write file");
        path
    }

    #[test]
    fn nothing_on_disk_means_no_sources() {
        let tmp = tempfile::tempdir().expect("tempdir");

        assert!(discover(tmp.path(), &config(None)).is_empty());
    }

    #[test]
    fn a_workspace_directory_is_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(DEFAULT_WORKSPACE_TEMPLATES_DIR);
        std::fs::create_dir_all(&dir).expect("create templates dir");

        let found = discover(tmp.path(), &config(None));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Workspace);
        assert_eq!(found[0].path, dir);
    }

    #[test]
    fn the_workspace_directory_outranks_the_global_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        std::fs::create_dir_all(tmp.path().join(DEFAULT_WORKSPACE_TEMPLATES_DIR))
            .expect("create workspace templates");
        std::fs::create_dir_all(global.join(DEFAULT_GLOBAL_TEMPLATES_DIR)).expect("create global");

        let found = discover(tmp.path(), &config(Some(global)));

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].scope, ContextScope::Workspace);
        assert_eq!(found[1].scope, ContextScope::Global);
    }

    #[test]
    fn a_global_directory_alone_is_used() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        std::fs::create_dir_all(global.join(DEFAULT_GLOBAL_TEMPLATES_DIR)).expect("create global");

        let found = discover(tmp.path(), &config(Some(global)));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Global);
    }

    #[test]
    fn a_file_where_the_directory_should_be_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(DEFAULT_WORKSPACE_TEMPLATES_DIR);
        std::fs::create_dir_all(dir.parent().expect("parent")).expect("create .basis");
        std::fs::write(&dir, "not a directory").expect("write file");

        assert!(discover(tmp.path(), &config(None)).is_empty());
    }

    #[test]
    fn the_same_directory_reached_twice_is_reported_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        std::fs::create_dir_all(global.join(DEFAULT_GLOBAL_TEMPLATES_DIR))
            .expect("create global templates");

        // Point the workspace subdir at the very same place.
        let found = discover(
            &global,
            &TemplatesConfig {
                workspace_subdir: PathBuf::from(DEFAULT_GLOBAL_TEMPLATES_DIR),
                global_dir: Some(global.clone()),
            },
        );

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Workspace);
    }

    #[test]
    fn loading_an_absent_directory_yields_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let loaded = load(tmp.path(), &config(None)).expect("absent is not an error");

        assert!(loaded.is_empty());
    }

    #[test]
    fn a_template_carries_its_frontmatter_and_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(DEFAULT_WORKSPACE_TEMPLATES_DIR);
        write(
            &dir,
            "review.md",
            "---\ndescription: Review a diff\nargument-hint: <path>\n---\nReview $ARGUMENTS.\n",
        );

        let loaded = load(tmp.path(), &config(None)).expect("load succeeds");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "review");
        assert_eq!(loaded[0].description, "Review a diff");
        assert_eq!(loaded[0].argument_hint.as_deref(), Some("<path>"));
        assert_eq!(loaded[0].body, "Review $ARGUMENTS.\n");
        assert_eq!(loaded[0].scope, ContextScope::Workspace);
    }

    #[test]
    fn a_workspace_template_shadows_a_global_one_of_the_same_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        write(
            &tmp.path().join(DEFAULT_WORKSPACE_TEMPLATES_DIR),
            "review.md",
            "---\ndescription: workspace\n---\nworkspace body\n",
        );
        write(
            &global.join(DEFAULT_GLOBAL_TEMPLATES_DIR),
            "review.md",
            "---\ndescription: global\n---\nglobal body\n",
        );
        write(
            &global.join(DEFAULT_GLOBAL_TEMPLATES_DIR),
            "plan.md",
            "---\ndescription: only global\n---\nplan body\n",
        );

        let loaded = load(tmp.path(), &config(Some(global))).expect("load succeeds");

        // Shadowing replaces one name, not the whole weaker root.
        assert_eq!(loaded.len(), 2);
        let review = loaded.iter().find(|t| t.name == "review").expect("review");
        assert_eq!(review.description, "workspace");
        assert_eq!(review.scope, ContextScope::Workspace);
        let plan = loaded.iter().find(|t| t.name == "plan").expect("plan");
        assert_eq!(plan.scope, ContextScope::Global);
    }

    #[test]
    fn templates_come_back_ordered_by_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(DEFAULT_WORKSPACE_TEMPLATES_DIR);
        for name in ["zeta", "alpha", "mid"] {
            write(
                &dir,
                &format!("{name}.md"),
                &format!("---\ndescription: {name}\n---\nbody\n"),
            );
        }

        let loaded = load(tmp.path(), &config(None)).expect("load succeeds");

        let names: Vec<&str> = loaded.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn a_missing_description_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(DEFAULT_WORKSPACE_TEMPLATES_DIR);
        let path = write(&dir, "bare.md", "just a prompt, no frontmatter\n");

        let error = load(tmp.path(), &config(None)).expect_err("rejected");

        assert!(matches!(
            &error,
            TemplateError::MissingDescription { path: reported } if reported == &path
        ));
    }

    #[test]
    fn render_is_reachable_from_a_loaded_template() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            &tmp.path().join(DEFAULT_WORKSPACE_TEMPLATES_DIR),
            "fix.md",
            "---\ndescription: fix\n---\nFix $1 in $2.",
        );

        let loaded = load(tmp.path(), &config(None)).expect("load succeeds");

        assert_eq!(loaded[0].render("auth login.rs"), "Fix auth in login.rs.");
    }
}
