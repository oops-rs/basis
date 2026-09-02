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

/// The workspace root, absolute and canonical, or the error that says why the
/// path is not one. [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open)
/// is the caller: one resolution, at the open, for every part of a workspace
/// that names its directory.
pub(crate) use discovery::validate_workspace as resolve_workspace;

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
    /// A bare name and nothing else: this is joined onto every candidate
    /// directory, so a `..` or a leading `/` here would make each of them
    /// contribute a document from elsewhere while still being reported under
    /// that directory's scope. A path is refused at discovery with
    /// [`ContextError::ContextFileNameNotBare`] rather than followed; a host
    /// that wants a file from somewhere else names a
    /// [`global_dir`](Self::global_dir) or supplies a
    /// [`SystemPrompt`] instead. Empty is
    /// [`none`](Self::none): no name, so nothing is looked for.
    ///
    /// Left at [`DEFAULT_CONTEXT_FILE`] this is the *first* of two names —
    /// see [`file_names`](Self::file_names) for the fallback and for why
    /// renaming opts out of it.
    pub file_name: String,
    /// Directory holding the user-global context file, if any.
    ///
    /// A directory, so it may be anywhere — naming a path is a host taking
    /// responsibility for it, the same latitude
    /// [`MemoryConfig`](crate::MemoryConfig)'s roots have.
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
    /// No context discovery at all (decision D9): `AGENTS.md` and `CLAUDE.md`
    /// are simply not read, in the workspace, in any ancestor, or in the
    /// global config directory.
    ///
    /// **Workspace validation still happens.** [`WorkspaceContext::discover_with`]
    /// resolves and checks the path before it ever looks at `file_names` —
    /// a workspace that does not exist, is not a directory, or cannot be
    /// canonicalized still fails [`open`](crate::WorkspaceBuilder::open) the
    /// same way it would with discovery on. What stops is reading *files*: no
    /// document is collected, so [`WorkspaceContext::render`] is always
    /// `None` and the system prompt is whatever else supplied one —
    /// [`SystemPrompt`], the memory index, or nothing.
    ///
    /// Reuses the fields rather than adding a fourth: an empty `file_name`
    /// is what [`file_names`](Self::file_names) treats as *no name to look
    /// for*, so no candidate is ever found in any directory regardless of
    /// [`global_dir`](Self::global_dir) or
    /// [`walk_parents`](Self::walk_parents) — set to their least-surprising
    /// values here rather than left at whatever the caller had, so a `Debug`
    /// print of this value does not suggest a knob that still does something.
    ///
    /// Who wants this: a host with its own opinion about what an agent should
    /// be told and no interest in a repository's ([`SystemPrompt::Replace`]
    /// already drops discovery from the *prompt*, but still pays for reading
    /// the files to report them; this skips the read too) — or a test that
    /// wants a result independent of whatever `AGENTS.md` the machine
    /// running it happens to carry.
    pub fn none() -> Self {
        Self {
            file_name: String::new(),
            global_dir: None,
            walk_parents: false,
        }
    }

    /// The names to try in one directory, strongest first. The first that is
    /// there is the document that directory contributes; the rest are not read.
    ///
    /// The fallback belongs to the *default* name rather than to any name. A
    /// host that renamed the convention has said which file it wants, and
    /// reading `CLAUDE.md` behind that name would be basis loading instructions
    /// from a file the host never named — the one thing a discovery knob exists
    /// to prevent.
    ///
    /// Empty is [`ContextConfig::none`]'s doing: no name to look for is no
    /// candidate ever found, in any directory.
    pub fn file_names(&self) -> Vec<&str> {
        if self.file_name.is_empty() {
            Vec::new()
        } else if self.file_name == DEFAULT_CONTEXT_FILE {
            vec![DEFAULT_CONTEXT_FILE, DEFAULT_CONTEXT_FALLBACK_FILE]
        } else {
            vec![self.file_name.as_str()]
        }
    }
}

/// `$BASIS_CONFIG_DIR`, else `$XDG_CONFIG_HOME/basis`, else `$HOME/.config/basis`.
///
/// The *one* global directory, shared by every convention that has a global
/// half — context, config, hooks, declared tools, skills, templates, MCP.
/// Each keeps its own `global_dir` field, because a host may point any one of
/// them somewhere else; what they must not do is each work out the default
/// differently, so they all default from here. `None` on a machine with none
/// of those variables set, which is what makes the global half optional.
pub(crate) fn default_global_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("BASIS_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("basis"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("basis"))
}

/// Anything that can go wrong while discovering context.
///
/// `#[non_exhaustive]` for the reason [`RunError`](crate::RunError) is, and
/// added in the same release as the variant that made the point: this enum was
/// the last public error type here a caller could match exhaustively, so every
/// new way discovery can fail broke that match. `ContextFileNameNotBare` is
/// the last variant addition that costs a downstream crate a compile.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ContextError {
    #[error(
        "context file name '{name}' is not a file name; \
         `file_name` names a file to look for in each candidate directory, \
         not a path to one"
    )]
    ContextFileNameNotBare { name: String },

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

    /// [`render`](Self::render) with the host's own say folded in, or `None`
    /// when neither the workspace nor the host has anything to say.
    ///
    /// `None` for `host` is the discovery-only prompt, byte for byte.
    pub fn render_with(&self, host: Option<&SystemPrompt>) -> Option<String> {
        self.render_with_appendix(host, None)
    }

    /// [`render_with`](Self::render_with), with a workspace-derived appendix —
    /// the memory index — slotted between the discovered documents and the
    /// host's say.
    ///
    /// One function rather than concatenation at the call site, so the
    /// appendix rides the same rules as everything else in the prompt:
    /// [`SystemPrompt::Replace`] removes it with the context block, a host's
    /// [`SystemPrompt::Append`] still lands last as the most specific
    /// statement, and a blank appendix is no appendix at all.
    pub(crate) fn render_with_appendix(
        &self,
        host: Option<&SystemPrompt>,
        appendix: Option<&str>,
    ) -> Option<String> {
        match host {
            None => joined([self.render(), appendix.and_then(spoken)]),
            Some(SystemPrompt::Replace(text)) => spoken(text),
            // The host's text goes last, where the rendered block's own
            // preamble says the most specific statement goes.
            Some(SystemPrompt::Append(text)) => {
                joined([self.render(), appendix.and_then(spoken), spoken(text)])
            }
        }
    }
}

/// What a host says on top of what the workspace says.
///
/// basis ships no system prompt of its own and this does not give it one: the
/// text is the *host's*, and a build that never names this type behaves exactly
/// as it did. What it removes is the workaround — an embedding host that wanted
/// its product to have a voice, or to say *for my runs, answer in Chinese*, had
/// to write into the user's repository's `AGENTS.md`, which is the one file
/// that is not the host's to edit.
///
/// One enum rather than two builder methods, because the two are alternatives
/// and not layers: a host either replaces what the workspace said or adds to
/// it, and asking the type system to hold that is cheaper than documenting what
/// happens when both are set.
///
/// Both scopes already had their weakest end covered — the global `AGENTS.md`
/// is a personal append below every workspace file. Neither variant touches
/// the skills block: mentra appends that itself, after whatever basis hands it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPrompt {
    /// The host's text alone. Discovered context files stay out of the prompt
    /// entirely — including the global one — which is what makes this usable
    /// for a host whose product is not "an agent that reads your repository".
    ///
    /// They are still *reported*: `run_started` names what discovery found,
    /// because that question — which files does this workspace have — has one
    /// true answer regardless of what the host did with them, and the host that
    /// replaced the prompt is the one party that already knows it did.
    ///
    /// Replacing with nothing is not an error. It is the way to say *no system
    /// prompt at all*, and it renders as `None` for the same reason an empty
    /// workspace does.
    Replace(String),
    /// The host's text after the rendered context, as the strongest block.
    ///
    /// Last because the rendered block tells the model that later blocks are
    /// more specific and take precedence, and the host's text is more specific
    /// than any file on disk: it is the statement of the program actually
    /// running this agent, about this deployment, which no repository can know
    /// about — and a knob that a repository could override by writing a file is
    /// not a knob. It is appended verbatim, outside the `<context>` framing,
    /// because it did not come from a file and giving it a path would say it
    /// did.
    ///
    /// Appending nothing is a no-op rather than an error: there is one obvious
    /// meaning and it is the one the workspace already had.
    Append(String),
}

/// The text, unless there is none to speak of.
///
/// Whitespace-only counts as none, which is the rule
/// [`render`](WorkspaceContext::render) already applies to a document on disk:
/// a prompt section that says nothing costs context and reads as an omission
/// the model has to explain to itself.
fn spoken(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_string())
}

/// The sections that had something to say, joined the way the rendered block
/// separates documents; `None` when none did.
fn joined<const N: usize>(sections: [Option<String>; N]) -> Option<String> {
    let spoken: Vec<String> = sections.into_iter().flatten().collect();
    if spoken.is_empty() {
        None
    } else {
        Some(spoken.join("\n\n"))
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
    fn none_names_nothing_to_look_for() {
        assert!(ContextConfig::none().file_names().is_empty());
    }

    #[test]
    fn none_discovers_no_documents_even_when_agents_md_exists() {
        // The workspace's own AGENTS.md is present and readable; `none()`
        // still finds nothing, because it never looks.
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        write(&workspace, DEFAULT_CONTEXT_FILE, "workspace rules");

        let context = WorkspaceContext::discover_with(&workspace, &ContextConfig::none())
            .expect("workspace validation still runs and still succeeds");

        assert!(context.is_empty());
        assert_eq!(context.render(), None);
    }

    #[test]
    fn none_still_validates_the_workspace_path() {
        // Discovery is off, not the existence check: a caller should learn a
        // bad path is bad the same way it always has.
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");

        let error = WorkspaceContext::discover_with(&missing, &ContextConfig::none())
            .expect_err("a missing workspace is still an error under `none()`");

        assert!(matches!(error, ContextError::WorkspaceMissing { .. }));
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

    /// `file_name` is a *name*, not a path: `collect` joins it onto every
    /// candidate directory, so a path there would make each of them
    /// contribute a document from somewhere else entirely — and the report
    /// would still label it `workspace` or `global`. ADR-0013 makes this
    /// hygiene rather than a boundary: nothing here stops a host reading
    /// whatever it likes, it stops a *name*-shaped knob quietly naming a
    /// place.
    #[test]
    fn a_context_file_name_that_is_a_path_is_refused_by_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        write(&workspace, DEFAULT_CONTEXT_FILE, "workspace rules");

        // `./AGENTS.md` is in the list deliberately. It names the same file
        // the bare name does and reaches nowhere else, so refusing it buys no
        // hygiene — what it buys is one rule with one spelling. "A bare name"
        // is a rule a host can hold; "a path that happens to resolve to a bare
        // name" is not, and it is the rule that has to answer for `./../x` and
        // `a/../AGENTS.md`. The refusal names the field and says what it wants,
        // so the cost of the strictness is one clear error at the open.
        for name in [
            "../AGENTS.md",
            "/etc/agents.md",
            "nested/AGENTS.md",
            "./AGENTS.md",
            "..",
        ] {
            let config = ContextConfig {
                file_name: name.to_string(),
                global_dir: None,
                walk_parents: false,
            };

            let error = WorkspaceContext::discover_with(&workspace, &config)
                .expect_err("a path is not a file name");

            assert!(
                matches!(error, ContextError::ContextFileNameNotBare { name: ref found } if found == name),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_bare_context_file_name_is_what_the_rule_allows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workspace = tmp.path().join("repo");
        write(&workspace, "HOUSE.md", "house rules");

        let config = ContextConfig {
            file_name: "HOUSE.md".to_string(),
            global_dir: None,
            walk_parents: false,
        };

        let context =
            WorkspaceContext::discover_with(&workspace, &config).expect("a bare name is fine");

        assert_eq!(context.documents().len(), 1);
        assert_eq!(context.documents()[0].content, "house rules");
    }
}
