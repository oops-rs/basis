//! The memory directory convention — files, not a subsystem (D2).
//!
//! A memory is one markdown file: YAML frontmatter naming `name`, a one-line
//! `description`, and a `type` ([`MemoryKind`]), body free-form. basis finds
//! the files, reads *frontmatter only*, and appends an index — name, one
//! line, path — to the system prompt, so what a memory costs by default is
//! its listing and not its body. Everything else is the tools the model
//! already has: recall is `read`, search is `grep`, writing or revising a
//! memory is `write` and `edit`. There is no memory tool, no database, and no
//! recall heuristic — the model reads a memory because its description says
//! to, visibly, in the transcript. (mentra's memory *engine* is the store
//! basis decided against; it stays off — see `agent_config` in the workspace
//! builder.)
//!
//! # Two roots
//!
//! - **Global**: `memory/` in the global config directory — the user's own
//!   memories, present for every workspace, resolved the way `context.rs`
//!   resolves that directory.
//! - **Workspace**: the sibling `memory/` directory beside the runtime's
//!   store dir, when [`RuntimeBuilder::with_store_dir`] named one — memory
//!   lives beside the history it was learned in, so the CLI's layout lands on
//!   `<data root>/workspaces/<key>/memory` without basis knowing the CLI's
//!   data dir. Ephemeral history, and mentra's process-cwd default, name no
//!   directory basis chose, so there is no per-workspace root to derive.
//!
//!   **Resolved only on a workspace-bound (private) runtime.** A store dir is
//!   one fact about a *runtime*, and on a shared one (ADR-0018) that runtime
//!   is not this workspace's alone — deriving a sibling from it would hand
//!   every workspace borrowing the runtime the identical directory, each
//!   reading the others' memory index into its own prompt. So
//!   `WorkspaceBuilder::open` passes no store dir on the shared path at all,
//!   and [`WorkspaceMemoryRoot::BesideStore`] resolves to nothing there,
//!   An explicit
//!   [`WorkspaceMemoryRoot::Dir`] is unaffected either way — naming a path is
//!   the host taking responsibility for it.
//!
//! A workspace memory shadows a global one of the same name — the rule skills
//! and templates already follow. Zero memories render no block at all, and a
//! missing directory is simply absent; a file that exists and cannot be
//! parsed fails the open naming the file, which is the posture every other
//! discovered file has.
//!
//! [`MemoryConfig`] on [`WorkspaceBuilder`] overrides either root or turns
//! discovery off entirely — nothing here is unconditional (D9).
//!
//! [`RuntimeBuilder::with_store_dir`]: crate::RuntimeBuilder::with_store_dir
//! [`WorkspaceBuilder`]: crate::WorkspaceBuilder

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{context::ContextScope, frontmatter, named_roots};

/// The directory name both roots share: `memory/` inside the global config
/// directory, `memory/` beside the store. One constant because it is one
/// convention under two scopes, the way `.agents/skills` is.
pub const MEMORY_DIR: &str = "memory";

/// The extension a memory file must have.
pub const MEMORY_EXTENSION: &str = "md";

/// How memory files are discovered. Every knob has a convention-shaped
/// default; [`MemoryConfig::disabled`] is the whole off switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryConfig {
    /// The user-global memory root. `None` reads none of this user's
    /// memories — the same meaning `global_dir: None` has on every other
    /// discovery config.
    pub global_root: Option<PathBuf>,
    /// Where this workspace's own memories live.
    pub workspace_root: WorkspaceMemoryRoot,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            global_root: crate::context::ContextConfig::default()
                .global_dir
                .map(|dir| dir.join(MEMORY_DIR)),
            workspace_root: WorkspaceMemoryRoot::BesideStore,
        }
    }
}

impl MemoryConfig {
    /// No discovery at all: no roots, no index, no write roots joined.
    pub fn disabled() -> Self {
        Self {
            global_root: None,
            workspace_root: WorkspaceMemoryRoot::Off,
        }
    }
}

/// Where a workspace's own memories live. Three states rather than an
/// `Option`, because *derive it* and *there is none* are different answers
/// and an `Option` could only spell one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceMemoryRoot {
    /// The convention: the sibling `memory/` directory beside the runtime's
    /// store dir, absent when no store dir was named (ephemeral history, or
    /// mentra's process-cwd default).
    BesideStore,
    /// An explicit directory, wherever the host keeps this workspace's
    /// memories.
    Dir(PathBuf),
    /// No workspace root at all; the global root, if any, still applies.
    Off,
}

/// A memory root that applies to one workspace — resolved, not necessarily
/// existing yet: the first memory is written by exactly the run that finds no
/// directory to read, so the roots are stated (to policy, to a host) before
/// anything is in them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySource {
    pub path: PathBuf,
    pub scope: ContextScope,
}

/// One discovered memory, frontmatter only — the body stays on disk for the
/// model to `read` when the description warrants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub name: String,
    /// One line: what makes this memory worth opening.
    pub description: String,
    pub kind: MemoryKind,
    pub path: PathBuf,
    /// Which root it came from, after shadowing.
    pub scope: ContextScope,
}

/// What kind of thing a memory records — the closed set the frontmatter's
/// `type` key takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    /// Something the user said about themselves or their preferences.
    User,
    /// A correction or judgement received on earlier work.
    Feedback,
    /// A durable fact about this project.
    Project,
    /// Reference material that was worth assembling once.
    Reference,
}

/// Anything that can go wrong while loading memories.
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("failed to read memory directory {path}: {source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read memory file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "memory {path} has no frontmatter; a memory opens with `---` and names \
         `name`, `description` and `type`"
    )]
    MissingFrontmatter { path: PathBuf },

    #[error("invalid memory frontmatter in {path}: {message}")]
    InvalidFrontmatter { path: PathBuf, message: String },

    #[error("duplicate memory name '{name}' in {first_path} and {second_path}")]
    DuplicateName {
        name: String,
        first_path: PathBuf,
        second_path: PathBuf,
    },
}

/// The keys basis reads and writes. Unknown keys in a file are left alone — a
/// memory written for a newer basis should still load here.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(rename = "type")]
    kind: MemoryKind,
}

/// The roots that apply to one workspace, strongest first — resolved from the
/// config and the runtime's store dir, existence not required.
///
/// `store_dir` is what [`RuntimeBuilder::with_store_dir`] named, or `None`
/// when history is ephemeral or at mentra's default — the cases with no
/// directory basis chose that a convention could build beside. Also `None`
/// on a *shared* runtime regardless of what it was built with:
/// `WorkspaceBuilder::open` passes nothing there on purpose, because a store
/// dir on a shared runtime is not this one workspace's fact to build beside —
/// see the module docs.
///
/// [`RuntimeBuilder::with_store_dir`]: crate::RuntimeBuilder::with_store_dir
pub(crate) fn roots(config: &MemoryConfig, store_dir: Option<&Path>) -> Vec<MemorySource> {
    let mut sources = Vec::new();

    let workspace_root = match &config.workspace_root {
        WorkspaceMemoryRoot::BesideStore => store_dir
            .and_then(Path::parent)
            .map(|parent| parent.join(MEMORY_DIR)),
        WorkspaceMemoryRoot::Dir(path) => Some(path.clone()),
        WorkspaceMemoryRoot::Off => None,
    };
    if let Some(path) = workspace_root {
        sources.push(MemorySource {
            path,
            scope: ContextScope::Workspace,
        });
    }

    // Two roots can name one directory — a host pointing both at one place.
    // The first mention is the stronger one, so the later mention goes; the
    // same rule every other discovery here applies.
    if let Some(global) = &config.global_root
        && !sources
            .iter()
            .any(|source| crate::paths::same_dir(&source.path, global))
    {
        sources.push(MemorySource {
            path: global.clone(),
            scope: ContextScope::Global,
        });
    }

    sources
}

/// Loads every memory the roots hold, strongest root first, name-ordered.
///
/// A root that is not a directory contributes nothing — nobody has written a
/// memory there yet. Within one root a repeated name is the mistake it looks
/// like; across roots it is intent, and the stronger root keeps the name —
/// [`crate::named_roots`] is where both rules actually live, shared with
/// [`crate::templates`].
pub(crate) fn load(sources: &[MemorySource]) -> Result<Vec<Memory>, MemoryError> {
    named_roots::merge_roots(
        sources
            .iter()
            .map(|source| load_root(&source.path, &source.scope)),
    )
}

/// Loads one root, flat: memories name themselves in frontmatter, so nesting
/// would add nothing a name does not already say.
fn load_root(root: &Path, scope: &ContextScope) -> Result<BTreeMap<String, Memory>, MemoryError> {
    if !root.is_dir() {
        return Ok(BTreeMap::new());
    }

    let entries = fs::read_dir(root).map_err(|source| MemoryError::ReadDir {
        path: root.to_path_buf(),
        source,
    })?;

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| MemoryError::ReadDir {
            path: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        // `file_type` does not follow symlinks, matching the walk templates
        // do; only plain `.md` files are memories.
        let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
        if is_file && is_memory_file(&path) {
            paths.push(path);
        }
    }

    named_roots::load_root(
        paths,
        |path| {
            let memory = read_memory(path, scope)?;
            Ok((memory.name.clone(), memory))
        },
        |name, first_path, second_path| MemoryError::DuplicateName {
            name,
            first_path,
            second_path,
        },
    )
}

/// Reads one file's frontmatter into a [`Memory`]. The body is deliberately
/// not kept: the index is what a memory costs by default.
fn read_memory(path: &Path, scope: &ContextScope) -> Result<Memory, MemoryError> {
    let raw = fs::read_to_string(path).map_err(|source| MemoryError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;

    let scanned = frontmatter::scan(&raw).map_err(|frontmatter::Unterminated| {
        MemoryError::InvalidFrontmatter {
            path: path.to_path_buf(),
            message: "missing closing frontmatter delimiter".to_string(),
        }
    })?;

    let block = match scanned.frontmatter {
        Some(block) if !block.trim().is_empty() => block,
        _ => {
            return Err(MemoryError::MissingFrontmatter {
                path: path.to_path_buf(),
            });
        }
    };

    let meta: Frontmatter =
        serde_yaml_ng::from_str(block).map_err(|error| MemoryError::InvalidFrontmatter {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

    let name = spoken_field(path, &meta.name, "name")?;
    let description = spoken_field(path, &meta.description, "description")?;

    Ok(Memory {
        name,
        description,
        kind: meta.kind,
        path: path.to_path_buf(),
        scope: scope.clone(),
    })
}

/// A required field that is present but blank is missing with extra steps.
fn spoken_field(path: &Path, value: &str, key: &str) -> Result<String, MemoryError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(MemoryError::InvalidFrontmatter {
            path: path.to_path_buf(),
            message: format!("`{key}` is empty"),
        });
    }
    Ok(value.to_string())
}

fn is_memory_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(MEMORY_EXTENSION))
}

/// The index the system prompt carries: one line per memory, then the
/// instruction paragraph that ships as data beside this module. `None` for
/// zero memories, so an empty convention costs an empty nothing.
pub(crate) fn index_block(memories: &[Memory]) -> Option<String> {
    if memories.is_empty() {
        return None;
    }

    let entries = memories
        .iter()
        .map(|memory| {
            format!(
                "- {} — {} ({})",
                memory.name,
                memory.description,
                memory.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Some(format!(
        "<memories>\n{entries}\n</memories>\n\n{}",
        include_str!("memory/instructions.md").trim_end()
    ))
}

/// The bytes of a memory file, frontmatter serialized so `load` parses back
/// exactly what was written — a description carrying a colon survives.
///
/// For hosts writing memories from their own code — a sink filing what a
/// compaction replaced is `examples/memory.rs` — and deliberately the whole
/// helper: everything past composing the file is the filesystem the host
/// already has.
pub fn file_contents(name: &str, description: &str, kind: MemoryKind, body: &str) -> String {
    let meta = serde_yaml_ng::to_string(&Frontmatter {
        name: name.to_string(),
        description: description.to_string(),
        kind,
    })
    .expect("three string-or-enum fields always serialize");

    format!("---\n{meta}---\n\n{}\n", body.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).expect("create dir");
        let path = dir.join(name);
        fs::write(&path, body).expect("write file");
        path
    }

    fn memory_file(name: &str) -> String {
        format!("---\nname: {name}\ndescription: about {name}\ntype: project\n---\nbody\n")
    }

    fn source(path: &Path, scope: ContextScope) -> MemorySource {
        MemorySource {
            path: path.to_path_buf(),
            scope,
        }
    }

    fn config(global: Option<PathBuf>, workspace: WorkspaceMemoryRoot) -> MemoryConfig {
        MemoryConfig {
            global_root: global,
            workspace_root: workspace,
        }
    }

    #[test]
    fn the_workspace_root_is_the_siblings_memory_directory_beside_the_store() {
        // `with_store_dir("<data>/workspaces/<key>/store")` puts memories at
        // `<data>/workspaces/<key>/memory` — beside the history, not in it.
        let found = roots(
            &config(None, WorkspaceMemoryRoot::BesideStore),
            Some(Path::new("/data/workspaces/abc/store")),
        );

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, PathBuf::from("/data/workspaces/abc/memory"));
        assert_eq!(found[0].scope, ContextScope::Workspace);
    }

    #[test]
    fn no_store_dir_means_no_workspace_root() {
        // Ephemeral history, or mentra's process-cwd default: no directory
        // basis chose, so nothing to build beside.
        assert!(roots(&config(None, WorkspaceMemoryRoot::BesideStore), None).is_empty());
    }

    #[test]
    fn an_explicit_workspace_root_is_used_as_given() {
        let found = roots(
            &config(None, WorkspaceMemoryRoot::Dir(PathBuf::from("/elsewhere"))),
            Some(Path::new("/data/store")),
        );

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, PathBuf::from("/elsewhere"));
    }

    #[test]
    fn the_workspace_root_outranks_the_global_one() {
        let found = roots(
            &config(
                Some(PathBuf::from("/home/config/memory")),
                WorkspaceMemoryRoot::Dir(PathBuf::from("/work/memory")),
            ),
            None,
        );

        let scopes: Vec<&ContextScope> = found.iter().map(|source| &source.scope).collect();
        assert_eq!(
            scopes,
            vec![&ContextScope::Workspace, &ContextScope::Global]
        );
    }

    #[test]
    fn disabled_resolves_no_roots_at_all() {
        assert!(roots(&MemoryConfig::disabled(), Some(Path::new("/data/store"))).is_empty());
    }

    #[test]
    fn one_directory_reached_by_both_roots_is_one_source() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let found = roots(
            &config(
                Some(tmp.path().to_path_buf()),
                WorkspaceMemoryRoot::Dir(tmp.path().to_path_buf()),
            ),
            None,
        );

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, ContextScope::Workspace);
    }

    #[test]
    fn a_missing_root_contributes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("never-written");

        let loaded =
            load(&[source(&missing, ContextScope::Workspace)]).expect("absent is not an error");

        assert!(loaded.is_empty());
    }

    #[test]
    fn a_memory_carries_its_frontmatter_and_not_its_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write(
            tmp.path(),
            "deploy.md",
            "---\nname: deploy-notes\ndescription: how deploys go out\ntype: project\n---\nlong body\n",
        );

        let loaded = load(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "deploy-notes");
        assert_eq!(loaded[0].description, "how deploys go out");
        assert_eq!(loaded[0].kind, MemoryKind::Project);
        assert_eq!(loaded[0].path, path);
        assert_eq!(loaded[0].scope, ContextScope::Workspace);
    }

    #[test]
    fn a_workspace_memory_shadows_a_global_one_of_the_same_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        let global = tmp.path().join("global");
        write(
            &workspace,
            "a.md",
            "---\nname: deploy\ndescription: workspace's\ntype: project\n---\n",
        );
        write(
            &global,
            "b.md",
            "---\nname: deploy\ndescription: global's\ntype: project\n---\n",
        );
        write(&global, "c.md", &memory_file("only-global"));

        let loaded = load(&[
            source(&workspace, ContextScope::Workspace),
            source(&global, ContextScope::Global),
        ])
        .expect("loads");

        // Shadowing replaces one name, not the whole weaker root.
        assert_eq!(loaded.len(), 2);
        let deploy = loaded.iter().find(|m| m.name == "deploy").expect("deploy");
        assert_eq!(deploy.description, "workspace's");
        assert_eq!(deploy.scope, ContextScope::Workspace);
        assert!(loaded.iter().any(|m| m.name == "only-global"));
    }

    #[test]
    fn two_files_claiming_one_name_in_a_single_root_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), "a.md", &memory_file("same"));
        write(tmp.path(), "b.md", &memory_file("same"));

        let error = load(&[source(tmp.path(), ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, MemoryError::DuplicateName { .. }));
        assert!(error.to_string().contains("same"));
    }

    #[test]
    fn a_file_without_frontmatter_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), "bare.md", "just prose, no frontmatter\n");

        let error = load(&[source(tmp.path(), ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, MemoryError::MissingFrontmatter { .. }));
        assert!(error.to_string().contains("bare.md"));
    }

    #[test]
    fn malformed_yaml_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), "bad.md", "---\nname: [unclosed\n---\nbody\n");

        let error = load(&[source(tmp.path(), ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, MemoryError::InvalidFrontmatter { .. }));
        assert!(error.to_string().contains("bad.md"));
    }

    #[test]
    fn a_type_outside_the_set_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            tmp.path(),
            "odd.md",
            "---\nname: odd\ndescription: d\ntype: whimsy\n---\n",
        );

        let error = load(&[source(tmp.path(), ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, MemoryError::InvalidFrontmatter { .. }));
        assert!(error.to_string().contains("odd.md"));
    }

    #[test]
    fn a_blank_name_counts_as_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            tmp.path(),
            "blank.md",
            "---\nname: \"  \"\ndescription: d\ntype: user\n---\n",
        );

        let error = load(&[source(tmp.path(), ContextScope::Workspace)]).expect_err("rejected");

        assert!(matches!(error, MemoryError::InvalidFrontmatter { .. }));
        assert!(error.to_string().contains("`name` is empty"));
    }

    #[test]
    fn unknown_keys_are_ignored_rather_than_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(
            tmp.path(),
            "future.md",
            "---\nname: future\ndescription: d\ntype: reference\nfrom-the-future: yes\n---\n",
        );

        let loaded = load(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded[0].name, "future");
    }

    #[test]
    fn non_markdown_files_are_not_memories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), "notes.txt", "not a memory");
        write(tmp.path(), ".gitkeep", "");
        write(tmp.path(), "real.md", &memory_file("real"));

        let loaded = load(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "real");
    }

    #[test]
    fn zero_memories_render_no_block_at_all() {
        assert_eq!(index_block(&[]), None);
    }

    #[test]
    fn the_block_lists_name_description_and_path_and_carries_the_instructions() {
        let memories = vec![Memory {
            name: "deploy-notes".to_string(),
            description: "how deploys go out".to_string(),
            kind: MemoryKind::Project,
            path: PathBuf::from("/mem/deploy.md"),
            scope: ContextScope::Global,
        }];

        let block = index_block(&memories).expect("renders");

        assert!(block.contains("<memories>"));
        assert!(block.contains("- deploy-notes — how deploys go out (/mem/deploy.md)"));
        // The instruction paragraph ships as data and rides with the block.
        assert!(block.contains("frontmatter"));
    }

    #[test]
    fn file_contents_round_trips_through_the_loader() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let written = file_contents(
            "ci-flake",
            "the hooks suite flakes: rerun alone before blaming a change",
            MemoryKind::Feedback,
            "Details worth keeping.",
        );
        write(tmp.path(), "ci-flake.md", &written);

        let loaded = load(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded[0].name, "ci-flake");
        assert_eq!(loaded[0].kind, MemoryKind::Feedback);
    }

    #[test]
    fn file_contents_survives_a_description_with_a_colon() {
        // The reason the frontmatter is serialized rather than formatted: a
        // colon in a bare YAML scalar changes what the line means.
        let tmp = tempfile::tempdir().expect("tempdir");
        let written = file_contents(
            "note",
            "remember: the parser is strict",
            MemoryKind::User,
            "body",
        );
        write(tmp.path(), "note.md", &written);

        let loaded = load(&[source(tmp.path(), ContextScope::Workspace)]).expect("loads");

        assert_eq!(loaded[0].description, "remember: the parser is strict");
    }
}
