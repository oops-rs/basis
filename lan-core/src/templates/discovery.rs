//! Walking the template roots and layering what they hold.
//!
//! Each root is loaded on its own, so a name repeated *inside* one root is the
//! mistake it looks like. Across roots the same name is intent — an override —
//! so the stronger root keeps it and the weaker one contributes everything
//! else. That is mentra's rule for skills, and templates would be surprising
//! if they layered differently.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use super::{
    NAMESPACE_SEPARATOR, TEMPLATE_EXTENSION, Template, TemplateError, TemplateSource, parse,
};
use crate::context::ContextScope;

/// Loads every source, strongest first, and returns the result ordered by name.
pub fn load_sources(sources: &[TemplateSource]) -> Result<Vec<Template>, TemplateError> {
    let mut merged: BTreeMap<String, Template> = BTreeMap::new();

    for source in sources {
        for (name, template) in load_root(&source.path, &source.scope)? {
            // `or_insert` and not `insert`: a name a stronger root already
            // claimed is not overwritten by a weaker one.
            merged.entry(name).or_insert(template);
        }
    }

    Ok(merged.into_values().collect())
}

/// Loads one root. A repeated name here is an error rather than a shadow.
fn load_root(
    root: &Path,
    scope: &ContextScope,
) -> Result<BTreeMap<String, Template>, TemplateError> {
    let mut relative_paths = Vec::new();
    collect(root, Path::new(""), &mut relative_paths)?;
    // Sorted so the file blamed for a duplicate is stable across filesystems.
    relative_paths.sort();

    let mut templates: BTreeMap<String, Template> = BTreeMap::new();

    for relative in relative_paths {
        let path = root.join(&relative);
        let name = name_for(&relative, &path)?;

        let raw = fs::read_to_string(&path).map_err(|source| TemplateError::ReadFile {
            path: path.clone(),
            source,
        })?;
        let (meta, body) = parse::split(&path, &raw)?;

        let description = meta
            .description
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| TemplateError::MissingDescription { path: path.clone() })?
            .to_string();

        if let Some(first) = templates.get(&name) {
            return Err(TemplateError::DuplicateName {
                name,
                first_path: first.path.clone(),
                second_path: path,
            });
        }

        templates.insert(
            name.clone(),
            Template {
                name,
                description,
                argument_hint: meta
                    .argument_hint
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                body,
                path,
                scope: scope.clone(),
            },
        );
    }

    Ok(templates)
}

/// Collects template files below `root`, as paths relative to it.
///
/// Relative rather than absolute because the relative path *is* the name;
/// deriving one from the other later would mean stripping a prefix that could,
/// in principle, fail.
fn collect(root: &Path, relative: &Path, found: &mut Vec<PathBuf>) -> Result<(), TemplateError> {
    let dir = root.join(relative);
    let entries = fs::read_dir(&dir).map_err(|source| TemplateError::ReadDir {
        path: dir.clone(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| TemplateError::ReadDir {
            path: dir.clone(),
            source,
        })?;
        let child = relative.join(entry.file_name());

        // `entry.file_type` does not follow symlinks, so a symlinked directory
        // is neither a file nor a directory here and is skipped. That is what
        // keeps a link pointing at an ancestor from walking forever.
        let file_type = entry.file_type().map_err(|source| TemplateError::ReadDir {
            path: root.join(&child),
            source,
        })?;

        if file_type.is_dir() {
            collect(root, &child, found)?;
        } else if file_type.is_file() && is_template_file(&child) {
            found.push(child);
        }
    }

    Ok(())
}

fn is_template_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(TEMPLATE_EXTENSION))
}

/// The command name a relative path stands for: `git/commit.md` is
/// `git:commit`.
///
/// `path` is carried only so a name that cannot be formed can say which file it
/// came from.
fn name_for(relative: &Path, path: &Path) -> Result<String, TemplateError> {
    let stem = relative.with_extension("");
    let mut parts = Vec::new();

    for component in stem.components() {
        let text = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| TemplateError::NonUtf8Path {
                path: path.to_path_buf(),
            })?;
        parts.push(text);
    }

    Ok(parts.join(NAMESPACE_SEPARATOR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write file");
        path
    }

    fn source(path: &Path, scope: ContextScope) -> TemplateSource {
        TemplateSource {
            path: path.to_path_buf(),
            scope,
        }
    }

    #[test]
    fn a_nested_file_is_namespaced_by_its_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            &tmp.path().join("git"),
            "commit.md",
            "---\ndescription: Write a commit\n---\nbody\n",
        );

        let loaded = load_sources(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "git:commit");
    }

    #[test]
    fn the_same_stem_in_two_directories_does_not_collide() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            &tmp.path().join("git"),
            "review.md",
            "---\ndescription: git\n---\nbody\n",
        );
        write(
            &tmp.path().join("docs"),
            "review.md",
            "---\ndescription: docs\n---\nbody\n",
        );

        let loaded = load_sources(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        let names: Vec<&str> = loaded.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["docs:review", "git:review"]);
    }

    #[test]
    fn non_markdown_files_are_not_templates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), "notes.txt", "not a template");
        write(tmp.path(), ".gitkeep", "");
        write(tmp.path(), "real.md", "---\ndescription: real\n---\nbody\n");

        let loaded = load_sources(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "real");
    }

    #[test]
    fn an_uppercase_extension_is_still_markdown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            tmp.path(),
            "SHOUT.MD",
            "---\ndescription: loud\n---\nbody\n",
        );

        let loaded = load_sources(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "SHOUT");
    }

    #[test]
    fn two_files_claiming_one_name_in_a_single_root_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // `x:y.md` and `x/y.md` both want the name `x:y`.
        write(tmp.path(), "x:y.md", "---\ndescription: flat\n---\nbody\n");
        write(
            &tmp.path().join("x"),
            "y.md",
            "---\ndescription: nested\n---\nbody\n",
        );

        let error =
            load_sources(&[source(tmp.path(), ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, TemplateError::DuplicateName { .. }));
        assert!(error.to_string().contains("x:y"));
    }

    #[test]
    fn a_blank_description_counts_as_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            tmp.path(),
            "empty.md",
            "---\ndescription: \"   \"\n---\nbody\n",
        );

        let error =
            load_sources(&[source(tmp.path(), ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, TemplateError::MissingDescription { .. }));
    }

    #[test]
    fn a_blank_argument_hint_is_no_hint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            tmp.path(),
            "hint.md",
            "---\ndescription: d\nargument-hint: \"  \"\n---\nbody\n",
        );

        let loaded = load_sources(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded[0].argument_hint, None);
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_a_silent_skip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Invalid UTF-8 is the portable way to make a readable path fail
        // `read_to_string`; a chmod-based test would not hold as root.
        std::fs::write(tmp.path().join("bad.md"), [0xff, 0xfe, 0x00]).expect("write");

        let error =
            load_sources(&[source(tmp.path(), ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, TemplateError::ReadFile { .. }));
    }

    #[test]
    fn a_root_that_vanished_is_an_error_rather_than_an_empty_set() {
        // Discovery found it; by the time it is read it is gone. Reporting
        // nothing would look like "no templates were written".
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("gone");

        let error =
            load_sources(&[source(&missing, ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, TemplateError::ReadDir { .. }));
    }

    #[test]
    fn each_template_remembers_which_root_it_came_from() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        let global = tmp.path().join("global");
        write(&workspace, "a.md", "---\ndescription: a\n---\nbody\n");
        write(&global, "b.md", "---\ndescription: b\n---\nbody\n");

        let loaded = load_sources(&[
            source(&workspace, ContextScope::Workspace),
            source(&global, ContextScope::Global),
        ])
        .expect("loads");

        assert_eq!(loaded[0].scope, ContextScope::Workspace);
        assert_eq!(loaded[1].scope, ContextScope::Global);
    }
}
