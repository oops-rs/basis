//! Merging named files across a stack of roots — the sibling of
//! [`crate::frontmatter`], and for the same reason: two conventions
//! ([`crate::templates`] and [`crate::memory`]) load a set of markdown files,
//! strongest root first, into one name-keyed list, and this module lives here
//! once so the two cannot drift on what a duplicate name means.
//!
//! Two rules, and both are about what a *name* means, not what a *file* is:
//!
//! - **Within one root, a repeated name is the mistake it looks like.** Two
//!   files claiming one name in the same directory is not a decision anyone
//!   made on purpose, so [`load_root`] refuses rather than picking one
//!   silently — and refuses in file order sorted first, so which file gets
//!   blamed is stable across filesystems rather than whatever a directory
//!   listing happened to return.
//! - **Across roots, a repeated name is intent — an override.** A workspace
//!   template shadowing a global one of the same name, or a workspace memory
//!   shadowing a personal one, is the whole point of having more than one
//!   root; [`merge_roots`] keeps the strongest root's writer and drops the
//!   rest silently, the way `HashMap::entry(..).or_insert(..)` always has.
//!
//! What is deliberately **not** shared: how a root's candidate files are
//! found (templates recurse for namespacing — `git/commit.md` is
//! `git:commit`; memory does not, because a memory names itself in
//! frontmatter and nesting would add nothing a name does not already say) and
//! how one file becomes one record (a template's name comes from its path, a
//! memory's from its own frontmatter). Both stay each caller's own, passed in
//! as the `parse_one` closure.

use std::{collections::BTreeMap, path::PathBuf};

/// Parses every file in `paths` into one name-keyed map, sorting first so a
/// duplicate is always blamed on the same pair of files regardless of
/// directory-listing order.
///
/// `parse_one` reads and interprets one file, named by the same path this
/// function passes it, into `(name, value)`. `duplicate` builds the caller's
/// own error type when a second file in this same batch claims a name the
/// first already has — this function never decides *how* that is reported,
/// only *that* it is.
pub(crate) fn load_root<T, E>(
    mut paths: Vec<PathBuf>,
    mut parse_one: impl FnMut(&PathBuf) -> Result<(String, T), E>,
    duplicate: impl Fn(String, PathBuf, PathBuf) -> E,
) -> Result<BTreeMap<String, T>, E> {
    paths.sort();

    let mut found: BTreeMap<String, (PathBuf, T)> = BTreeMap::new();
    for path in paths {
        let (name, value) = parse_one(&path)?;
        if let Some((first_path, _)) = found.get(&name) {
            return Err(duplicate(name, first_path.clone(), path));
        }
        found.insert(name, (path, value));
    }

    Ok(found
        .into_iter()
        .map(|(name, (_, value))| (name, value))
        .collect())
}

/// Folds one convention's already-loaded roots into its final list,
/// strongest first: the first root to name something keeps it, every later
/// root's claim on the same name is silently dropped.
///
/// Takes the roots as an iterator of already-`Result`ed maps — each built by
/// [`load_root`] — rather than the raw sources, so a caller keeps its own
/// per-source loop (it is the one that knows how to turn one source into a
/// root) and this only ever does the folding.
pub(crate) fn merge_roots<T, E>(
    roots: impl IntoIterator<Item = Result<BTreeMap<String, T>, E>>,
) -> Result<Vec<T>, E> {
    let mut merged: BTreeMap<String, T> = BTreeMap::new();

    for root in roots {
        for (name, value) in root? {
            merged.entry(name).or_insert(value);
        }
    }

    Ok(merged.into_values().collect())
}

#[cfg(test)]
mod tests;
