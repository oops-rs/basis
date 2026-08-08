//! The filesystem walk behind [`WorkspaceContext::discover_with`].
//!
//! [`WorkspaceContext::discover_with`]: super::WorkspaceContext::discover_with

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use super::{ContextConfig, ContextDocument, ContextError, ContextScope};

/// Finds every context file that applies to `workspace`, weakest precedence
/// first: global, then ancestors outermost-inward, then the workspace root.
pub(super) fn discover(
    workspace: &Path,
    config: &ContextConfig,
) -> Result<Vec<ContextDocument>, ContextError> {
    let workspace = validate_workspace(workspace)?;

    let mut seen = HashSet::new();
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

    Ok(documents)
}

/// Rejects a workspace that is missing or is not a directory, and resolves it
/// so the parent walk and the duplicate check agree on identity.
fn validate_workspace(workspace: &Path) -> Result<PathBuf, ContextError> {
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

    std::fs::canonicalize(workspace).map_err(|source| ContextError::WorkspaceUnresolvable {
        path: workspace.to_path_buf(),
        source,
    })
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

/// Reads `dir/<file_name>` if it is there, appending it to `documents`.
///
/// A directory that does not hold the file is skipped silently — that is the
/// normal case. A file that is present but unreadable is an error: staying
/// quiet about it would mean running with instructions the user believes are
/// in effect.
fn collect(
    dir: &Path,
    scope: ContextScope,
    config: &ContextConfig,
    seen: &mut HashSet<PathBuf>,
    documents: &mut Vec<ContextDocument>,
) -> Result<(), ContextError> {
    let path = dir.join(&config.file_name);
    if !path.is_file() {
        return Ok(());
    }

    // Two candidate directories can name the same file — a global dir that
    // also sits in the parent chain, or a symlinked path. Identity is the
    // canonical path; precedence goes to whichever scope found it first,
    // which is the weaker one, so a document never silently gains strength.
    let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if !seen.insert(identity) {
        return Ok(());
    }

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
