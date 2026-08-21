//! Discovery of workspace context files (`AGENTS.md` and friends).
//!
//! A mission arrives as data (PROPOSAL.md Bet 4), and the largest part of that
//! data is the workspace's own instructions. This module finds them and orders
//! them; it does not interpret them.
//!
//! Precedence runs least-specific to most-specific: a global file, then each
//! ancestor directory from the outermost inward, then the workspace root. The
//! rendered output preserves that order, so a more specific document is read
//! last and reads as an override.
//!
//! Each of those directories is asked for `AGENTS.md` and then, only if it has
//! none, for `CLAUDE.md` — one document per directory, the first name that is
//! there. Both files are the same convention under two names, and a repository
//! that carries only the older one is not a repository with no instructions.

mod discovery;
mod render;

use std::path::{Path, PathBuf};

use thiserror::Error;

/// The default file name basis looks for. Chosen by convention, not by basis —
/// `AGENTS.md` is a cross-agent standard.
pub const DEFAULT_CONTEXT_FILE: &str = "AGENTS.md";

/// What basis reads in a directory that has no [`DEFAULT_CONTEXT_FILE`].
///
/// The same convention under its older name: the repositories that carry a
/// `CLAUDE.md` and no `AGENTS.md` wrote it to instruct an agent, and reading
/// only the newer spelling means handing the model an empty system prompt for a
/// repository that took the trouble to write one. pi reads the pair in this
/// order for the same reason.
pub const DEFAULT_CONTEXT_FALLBACK_FILE: &str = "CLAUDE.md";

/// Where a context document was found. Ordering is precedence: `Global` is the
/// weakest, `Workspace` the strongest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextScope {
    /// A user-global file outside any workspace.
    Global,
    /// An ancestor of the workspace root. `depth` is 1 for the immediate
    /// parent and grows with distance, so a larger depth is weaker.
    Ancestor { depth: usize },
    /// The workspace root itself.
    Workspace,
}

impl ContextScope {
    /// Stable wire label. Used both in the rendered system prompt and in the
    /// event stream's header, so a client and the model name a scope the same
    /// way.
    pub fn label(&self) -> String {
        match self {
            Self::Global => "global".to_string(),
            Self::Ancestor { depth } => format!("ancestor:{depth}"),
            Self::Workspace => "workspace".to_string(),
        }
    }
}

/// One discovered context file, with the text as it was on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextDocument {
    pub path: PathBuf,
    pub scope: ContextScope,
    pub content: String,
}

/// How to look for context files. Every knob has a convention-shaped default;
/// none of them encode a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextConfig {
    /// File name to look for in each candidate directory.
    ///
    /// Left at [`DEFAULT_CONTEXT_FILE`] this is the *first* of two names —
    /// see [`file_names`](Self::file_names) for the fallback and for why
    /// renaming opts out of it.
    pub file_name: String,
    /// Directory holding the user-global context file, if any.
    pub global_dir: Option<PathBuf>,
    /// Whether to walk from the workspace root up to the filesystem root.
    pub walk_parents: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            file_name: DEFAULT_CONTEXT_FILE.to_string(),
            global_dir: default_global_dir(),
            walk_parents: true,
        }
    }
}

impl ContextConfig {
    /// The names to try in one directory, strongest first. The first that is
    /// there is the document that directory contributes; the rest are not read.
    ///
    /// The fallback belongs to the *default* name rather than to any name. A
    /// host that renamed the convention has said which file it wants, and
    /// reading `CLAUDE.md` behind that name would be basis loading instructions
    /// from a file the host never named — the one thing a discovery knob exists
    /// to prevent.
    pub fn file_names(&self) -> Vec<&str> {
        if self.file_name == DEFAULT_CONTEXT_FILE {
            vec![DEFAULT_CONTEXT_FILE, DEFAULT_CONTEXT_FALLBACK_FILE]
        } else {
            vec![self.file_name.as_str()]
        }
    }
}

/// `$BASIS_CONFIG_DIR`, else `$XDG_CONFIG_HOME/basis`, else `$HOME/.config/basis`.
fn default_global_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("BASIS_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("basis"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("basis"))
}

/// Anything that can go wrong while discovering context.
#[derive(Debug, Error)]
pub enum ContextError {
    #[error("workspace path does not exist: {path}")]
    WorkspaceMissing { path: PathBuf },

    #[error("workspace path is not a directory: {path}")]
    WorkspaceNotADirectory { path: PathBuf },

    #[error("failed to resolve workspace path {path}: {source}")]
    WorkspaceUnresolvable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read context file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The context files that apply to one workspace, ordered weakest-first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceContext {
    root: Option<PathBuf>,
    documents: Vec<ContextDocument>,
}

impl WorkspaceContext {
    /// Discovers context files for `workspace` using the default config.
    pub fn discover(workspace: impl AsRef<Path>) -> Result<Self, ContextError> {
        Self::discover_with(workspace, &ContextConfig::default())
    }

    /// Discovers context files for `workspace` using an explicit config.
    ///
    /// A candidate directory without the file is not an error; a file that
    /// exists but cannot be read is.
    pub fn discover_with(
        workspace: impl AsRef<Path>,
        config: &ContextConfig,
    ) -> Result<Self, ContextError> {
        let (root, documents) = discovery::discover(workspace.as_ref(), config)?;
        Ok(Self {
            root: Some(root),
            documents,
        })
    }

    /// Builds a context from documents already in hand, for hosts that source
    /// them from somewhere other than the filesystem.
    pub fn from_documents(documents: Vec<ContextDocument>) -> Self {
        Self {
            root: None,
            documents,
        }
    }

    /// The workspace root as basis resolved it — symlinks followed, so it names
    /// the same directory the discovered paths are relative to. `None` when
    /// the context did not come from a filesystem walk.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The discovered documents, weakest precedence first.
    pub fn documents(&self) -> &[ContextDocument] {
        &self.documents
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Renders the documents into one block for a system prompt, or `None`
    /// when nothing was found.
    pub fn render(&self) -> Option<String> {
        render::render(&self.documents)
    }
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

    fn config(global: Option<PathBuf>) -> ContextConfig {
        ContextConfig {
            file_name: DEFAULT_CONTEXT_FILE.to_string(),
            global_dir: global,
            walk_parents: true,
        }
    }

    #[test]
    fn finds_the_workspace_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        let path = write(&workspace, DEFAULT_CONTEXT_FILE, "workspace rules");

        let context =
            WorkspaceContext::discover_with(&workspace, &config(None)).expect("discovery succeeds");

        assert_eq!(context.documents().len(), 1);
        assert_eq!(context.documents()[0].scope, ContextScope::Workspace);
        assert_eq!(context.documents()[0].content, "workspace rules");
        assert!(
            context.documents()[0]
                .path
                .ends_with(path.file_name().unwrap())
        );
    }

    #[test]
    fn the_default_config_names_the_pair_strongest_first() {
        assert_eq!(
            ContextConfig::default().file_names(),
            vec![DEFAULT_CONTEXT_FILE, DEFAULT_CONTEXT_FALLBACK_FILE]
        );
    }

    #[test]
    fn a_directory_with_only_the_older_name_still_contributes() {
        // The failure this removes: a repository carrying only `CLAUDE.md`
        // handed basis an empty system prompt.
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        write(&workspace, DEFAULT_CONTEXT_FALLBACK_FILE, "older name");

        let context =
            WorkspaceContext::discover_with(&workspace, &config(None)).expect("discovery succeeds");

        assert_eq!(context.documents().len(), 1);
        assert_eq!(context.documents()[0].content, "older name");
        assert_eq!(context.documents()[0].scope, ContextScope::Workspace);
    }

    #[test]
    fn the_standard_name_wins_where_a_directory_holds_both() {
        // One document per directory. Reading both would give the same
        // instructions twice to any repository that keeps the two in sync,
        // which is most of them.
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        write(&workspace, DEFAULT_CONTEXT_FILE, "standard");
        write(&workspace, DEFAULT_CONTEXT_FALLBACK_FILE, "older");

        let context =
            WorkspaceContext::discover_with(&workspace, &config(None)).expect("discovery succeeds");

        assert_eq!(context.documents().len(), 1);
        assert_eq!(context.documents()[0].content, "standard");
    }

    #[test]
    fn the_global_directory_honors_the_same_pair() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        let workspace = tmp.path().join("repo");

        write(&global, DEFAULT_CONTEXT_FALLBACK_FILE, "personal");
        write(&workspace, DEFAULT_CONTEXT_FILE, "workspace");

        let context = WorkspaceContext::discover_with(&workspace, &config(Some(global)))
            .expect("discovery succeeds");

        let bodies: Vec<&str> = context
            .documents()
            .iter()
            .map(|doc| doc.content.as_str())
            .collect();
        assert_eq!(bodies, vec!["personal", "workspace"]);
    }

    #[test]
    fn a_renamed_convention_reads_only_the_name_it_was_given() {
        // The fallback is part of the default pair, not a modifier on whatever
        // a host names: a host that said `HOUSE.md` did not ask for CLAUDE.md.
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        write(&workspace, DEFAULT_CONTEXT_FALLBACK_FILE, "not asked for");

        let config = ContextConfig {
            file_name: "HOUSE.md".to_string(),
            ..config(None)
        };
        let context =
            WorkspaceContext::discover_with(&workspace, &config).expect("discovery succeeds");

        assert!(context.is_empty());
    }

    #[test]
    fn an_unreadable_standard_file_is_an_error_rather_than_a_fallback() {
        // Falling through would run the older file while the user believes the
        // one they edited is in effect — the failure the read error exists for.
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::write(workspace.join(DEFAULT_CONTEXT_FILE), [0xff, 0xfe, 0x00]).expect("write");
        write(&workspace, DEFAULT_CONTEXT_FALLBACK_FILE, "older");

        let error = WorkspaceContext::discover_with(&workspace, &config(None))
            .expect_err("unreadable file is an error");

        assert!(matches!(error, ContextError::Read { .. }));
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("create workspace");

        let context =
            WorkspaceContext::discover_with(&workspace, &config(None)).expect("discovery succeeds");

        assert!(context.is_empty());
        assert_eq!(context.render(), None);
    }

    #[test]
    fn orders_ancestors_outermost_first_then_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outer = tmp.path().join("outer");
        let middle = outer.join("middle");
        let workspace = middle.join("repo");

        write(&outer, DEFAULT_CONTEXT_FILE, "outer");
        write(&middle, DEFAULT_CONTEXT_FILE, "middle");
        write(&workspace, DEFAULT_CONTEXT_FILE, "workspace");

        let context =
            WorkspaceContext::discover_with(&workspace, &config(None)).expect("discovery succeeds");

        let bodies: Vec<&str> = context
            .documents()
            .iter()
            .map(|doc| doc.content.as_str())
            .collect();
        assert_eq!(bodies, vec!["outer", "middle", "workspace"]);

        let scopes: Vec<&ContextScope> = context.documents().iter().map(|doc| &doc.scope).collect();
        assert_eq!(
            scopes,
            vec![
                &ContextScope::Ancestor { depth: 2 },
                &ContextScope::Ancestor { depth: 1 },
                &ContextScope::Workspace,
            ]
        );
    }

    #[test]
    fn global_precedes_every_workspace_document() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = tmp.path().join("global");
        let workspace = tmp.path().join("repo");

        write(&global, DEFAULT_CONTEXT_FILE, "global");
        write(&workspace, DEFAULT_CONTEXT_FILE, "workspace");

        let context = WorkspaceContext::discover_with(&workspace, &config(Some(global)))
            .expect("discovery succeeds");

        let scopes: Vec<&ContextScope> = context.documents().iter().map(|doc| &doc.scope).collect();
        assert_eq!(
            scopes,
            vec![&ContextScope::Global, &ContextScope::Workspace]
        );
    }

    #[test]
    fn parent_walk_can_be_disabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("parent");
        let workspace = parent.join("repo");

        write(&parent, DEFAULT_CONTEXT_FILE, "parent");
        write(&workspace, DEFAULT_CONTEXT_FILE, "workspace");

        let config = ContextConfig {
            walk_parents: false,
            ..config(None)
        };
        let context =
            WorkspaceContext::discover_with(&workspace, &config).expect("discovery succeeds");

        let bodies: Vec<&str> = context
            .documents()
            .iter()
            .map(|doc| doc.content.as_str())
            .collect();
        assert_eq!(bodies, vec!["workspace"]);
    }

    #[test]
    fn a_global_dir_inside_the_walk_is_not_read_twice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("parent");
        let workspace = parent.join("repo");

        write(&parent, DEFAULT_CONTEXT_FILE, "shared");
        std::fs::create_dir_all(&workspace).expect("create workspace");

        let context = WorkspaceContext::discover_with(&workspace, &config(Some(parent)))
            .expect("discovery succeeds");

        assert_eq!(context.documents().len(), 1);
        assert_eq!(context.documents()[0].scope, ContextScope::Global);
    }

    #[test]
    fn whitespace_only_documents_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        write(&workspace, DEFAULT_CONTEXT_FILE, "   \n\n\t\n");

        let context =
            WorkspaceContext::discover_with(&workspace, &config(None)).expect("discovery succeeds");

        assert!(context.is_empty());
    }

    #[test]
    fn a_missing_workspace_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("nope");

        let error = WorkspaceContext::discover_with(&workspace, &config(None))
            .expect_err("missing workspace is an error");

        assert!(matches!(error, ContextError::WorkspaceMissing { .. }));
    }

    #[test]
    fn a_file_as_workspace_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write(tmp.path(), "not-a-dir", "hello");

        let error = WorkspaceContext::discover_with(&path, &config(None))
            .expect_err("file workspace is an error");

        assert!(matches!(error, ContextError::WorkspaceNotADirectory { .. }));
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_a_silent_skip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        // Invalid UTF-8 is the portable way to make a readable path fail
        // `read_to_string`; a chmod-based test would not hold when running
        // as root.
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::write(workspace.join(DEFAULT_CONTEXT_FILE), [0xff, 0xfe, 0x00]).expect("write");

        let error = WorkspaceContext::discover_with(&workspace, &config(None))
            .expect_err("unreadable file is an error");

        assert!(matches!(error, ContextError::Read { .. }));
    }

    #[test]
    fn render_labels_each_document_with_its_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        write(&workspace, DEFAULT_CONTEXT_FILE, "be careful");

        let context =
            WorkspaceContext::discover_with(&workspace, &config(None)).expect("discovery succeeds");
        let rendered = context.render().expect("something to render");

        assert!(rendered.contains("be careful"));
        assert!(rendered.contains(DEFAULT_CONTEXT_FILE));
    }

    #[test]
    fn render_keeps_precedence_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("parent");
        let workspace = parent.join("repo");
        write(&parent, DEFAULT_CONTEXT_FILE, "WEAKER");
        write(&workspace, DEFAULT_CONTEXT_FILE, "STRONGER");

        let context =
            WorkspaceContext::discover_with(&workspace, &config(None)).expect("discovery succeeds");
        let rendered = context.render().expect("something to render");

        let weaker = rendered.find("WEAKER").expect("weaker present");
        let stronger = rendered.find("STRONGER").expect("stronger present");
        assert!(weaker < stronger, "more specific context must come last");
    }
}
