//! The filesystem walk behind [`WorkspaceContext::discover_with`].
//!
//! [`WorkspaceContext::discover_with`]: super::WorkspaceContext::discover_with

use std::path::{Component, Path, PathBuf};

use super::{ContextConfig, ContextDocument, ContextError, ContextScope};

/// Finds every context file that applies to `workspace`, weakest precedence
/// first: global, then ancestors outermost-inward, then the workspace root.
///
/// Returns the resolved workspace root alongside the documents, because
/// resolution follows symlinks and a caller reporting what was loaded should
/// report the root those paths actually sit under.
pub(super) fn discover(
    workspace: &Path,
    config: &ContextConfig,
) -> Result<(PathBuf, Vec<ContextDocument>), ContextError> {
    // First, before the workspace is even resolved: a name that is not a name
    // is a configuration mistake and not a runtime condition, the same ruling
    // `validate_target_names` makes about command targets.
    validate_file_name(&config.file_name)?;

    let workspace = validate_workspace(workspace)?;

    let mut seen = Vec::new();
    let mut documents = Vec::new();

    if let Some(global_dir) = &config.global_dir {
        collect(
            global_dir,
            ContextScope::Global,
            config,
            &mut seen,
            &mut documents,
        )?;
    }

    if config.walk_parents {
        let chain = ancestors(&workspace);
        let total = chain.len();
        for (index, ancestor) in chain.into_iter().enumerate() {
            // `chain` runs outermost-first, but depth measures distance from
            // the workspace, so the last entry is the immediate parent.
            let depth = total - index;
            collect(
                &ancestor,
                ContextScope::Ancestor { depth },
                config,
                &mut seen,
                &mut documents,
            )?;
        }
    }

    collect(
        &workspace,
        ContextScope::Workspace,
        config,
        &mut seen,
        &mut documents,
    )?;

    Ok((workspace, documents))
}

/// Rejects a [`ContextConfig::file_name`] that is a path rather than a name.
///
/// [`collect`] joins the configured name onto *every* candidate directory —
/// the global one, each ancestor, the workspace root — so a name carrying `..`
/// or a leading `/` makes each of them contribute a document from somewhere
/// else, reported under that directory's scope. The field is a `String` named
/// `file_name`, [`file_names`](ContextConfig::file_names) calls what it
/// returns *names*, and both defaults are bare (`AGENTS.md`, `CLAUDE.md`);
/// this is the type system catching up with what the code and its docs already
/// say.
///
/// Not a boundary, and not claimed as one (ADR-0013): a host that wants basis
/// to read a file elsewhere can say so with
/// [`SystemPrompt`](super::SystemPrompt), or point
/// [`global_dir`](ContextConfig::global_dir) — a *directory* — wherever it
/// likes. Naming a directory is a host taking responsibility for a path, the
/// same latitude the memory roots have. A *name* silently naming a place is
/// the thing that has no honest reading, so it is refused by name at the open
/// instead of loading instructions from a file nobody named.
///
/// Empty is [`ContextConfig::none`] and is not a name at all — nothing is ever
/// looked for, so there is nothing to check.
///
/// One component, and `./AGENTS.md` is therefore refused along with the rest.
/// That spelling names the same file the bare name does and reaches nowhere
/// else, so refusing it buys no hygiene — what it buys is a rule with one
/// spelling. "A bare name" is a rule a host can hold and this function can
/// state in a line; "a path that happens to resolve to a bare name" is neither,
/// and it is the version that has to answer for `./../x` and
/// `a/../AGENTS.md`. The cost is one clear error at the open, from a message
/// that names the field and says what it wants. Pinned by test, so it is a
/// decision rather than a side effect of how `Components` treats a leading dot.
fn validate_file_name(name: &str) -> Result<(), ContextError> {
    if name.is_empty() {
        return Ok(());
    }

    let mut components = Path::new(name).components();
    let bare =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();

    if bare {
        Ok(())
    } else {
        Err(ContextError::ContextFileNameNotBare {
            name: name.to_string(),
        })
    }
}

/// Rejects a workspace that is missing or is not a directory, and resolves it
/// so the parent walk and the duplicate check agree on identity.
///
/// `pub(crate)` because [`WorkspaceBuilder::open`] resolves the root through
/// this same function before anything else reads it: the parent walk, the
/// dispatcher key, the runtime's policy roots and the agent's base directory
/// must all name one directory, and they do that by resolving *once* rather
/// than each canonicalizing its own copy. Calling it again here is free —
/// canonicalizing a canonical path returns it unchanged — and keeps
/// [`WorkspaceContext::discover_with`](super::WorkspaceContext::discover_with)
/// correct for the hosts that call it directly.
///
/// # Why the Windows verbatim prefix comes back off
///
/// Because this one value leaves basis: it becomes mentra's policy root and
/// the agent's base directory, and mentra decides whether a path the model
/// named is allowed by `starts_with` against that root
/// (`RuntimePolicy::path_is_allowed`). Its normalizer copies a
/// `Component::Prefix` through untouched, so the verbatim form
/// `std::fs::canonicalize` returns on Windows — `\\?\C:\repo` — would never
/// prefix the plain `C:\repo\file.txt` a model writes, and every absolute path
/// the model named would be refused. Simplified here rather than at each
/// consumer, because one spelling for one directory is the whole point of
/// resolving once. `dunce` leaves the verbatim form in place in the cases
/// where a plain one would name something else or nothing at all — a reserved
/// DOS name, a segment ending in a dot or a space, a path past `MAX_PATH` —
/// and is a no-op on the other two platforms.
pub(crate) fn validate_workspace(workspace: &Path) -> Result<PathBuf, ContextError> {
    let metadata = std::fs::metadata(workspace).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => ContextError::WorkspaceMissing {
            path: workspace.to_path_buf(),
        },
        _ => ContextError::WorkspaceUnresolvable {
            path: workspace.to_path_buf(),
            source,
        },
    })?;

    if !metadata.is_dir() {
        return Err(ContextError::WorkspaceNotADirectory {
            path: workspace.to_path_buf(),
        });
    }

    let canonical =
        std::fs::canonicalize(workspace).map_err(|source| ContextError::WorkspaceUnresolvable {
            path: workspace.to_path_buf(),
            source,
        })?;

    Ok(dunce::simplified(&canonical).to_path_buf())
}

/// The workspace's ancestors, ordered outermost first so that walking them in
/// order yields weakest-to-strongest precedence. Terminates at the filesystem
/// root because `Path::parent` eventually returns `None`.
fn ancestors(workspace: &Path) -> Vec<PathBuf> {
    let mut ancestors: Vec<PathBuf> = workspace
        .ancestors()
        .skip(1)
        .map(Path::to_path_buf)
        .collect();
    ancestors.reverse();
    ancestors
}

/// Reads the first of [`ContextConfig::file_names`] that `dir` holds,
/// appending it to `documents`.
///
/// A directory that holds none of them is skipped silently — that is the
/// normal case. A file that is present but unreadable is an error: staying
/// quiet about it would mean running with instructions the user believes are
/// in effect.
///
/// One document per directory, and *present* is what decides which — not
/// *non-empty*. A directory carrying both names has already answered the
/// question with the stronger one, and an `AGENTS.md` that says nothing says
/// nothing deliberately; falling through to `CLAUDE.md` behind it would make
/// which file is in effect depend on its contents.
fn collect(
    dir: &Path,
    scope: ContextScope,
    config: &ContextConfig,
    seen: &mut Vec<PathBuf>,
    documents: &mut Vec<ContextDocument>,
) -> Result<(), ContextError> {
    let candidate = config
        .file_names()
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file());
    let Some(path) = candidate else {
        return Ok(());
    };

    // Two candidate directories can name the same file — a global dir that
    // also sits in the parent chain, or a symlinked path. Identity is the
    // canonical path; precedence goes to whichever scope found it first,
    // which is the weaker one, so a document never silently gains strength.
    if seen.iter().any(|seen| crate::paths::same_dir(seen, &path)) {
        return Ok(());
    }
    seen.push(path.clone());

    let content = std::fs::read_to_string(&path).map_err(|source| ContextError::Read {
        path: path.clone(),
        source,
    })?;

    if content.trim().is_empty() {
        return Ok(());
    }

    documents.push(ContextDocument {
        path,
        scope,
        content,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestors_are_outermost_first() {
        let workspace = PathBuf::from("/a/b/c");
        let found = ancestors(&workspace);

        assert_eq!(
            found,
            vec![
                PathBuf::from("/"),
                PathBuf::from("/a"),
                PathBuf::from("/a/b"),
            ]
        );
    }

    #[test]
    fn a_root_workspace_has_no_ancestors() {
        assert!(ancestors(Path::new("/")).is_empty());
    }
}
