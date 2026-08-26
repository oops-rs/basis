//! Where this runtime's conversations go — the store-posture half of
//! [`RuntimeBuilder`](super::RuntimeBuilder).
//!
//! One question, *where*, with three answers: a directory the caller names,
//! nowhere at all, and unsaid. [`History`] is that question as a type, and
//! the two builder methods below are the only things that set it — which is
//! what makes "whichever was called last decides" a property of one field
//! rather than a rule two flags have to keep between them.
//!
//! Split out of `builder.rs` as its own responsibility rather than for line
//! count: everything about a store posture is here, including the two
//! derivations `build` reaches for — which store to open (and the refusal
//! that comes with opening one, see [`store::store_in`]) and where the
//! compaction snapshots that belong beside it go. A reader asking "what does
//! basis do with the history knobs" reads one file.

use std::path::{Path, PathBuf};

use mentra::runtime::FileRuntimeStore;

use crate::{error::RunError, store};

use super::RuntimeBuilder;

/// What a caller said about where this runtime's conversations go.
///
/// One field rather than a directory beside a flag, so that the two knobs which
/// set it cannot both be in force: whichever was called last is the one that is
/// read, and there is no state in which they disagree. `None` is *unsaid* —
/// mentra chooses, which is neither of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum History {
    /// [`RuntimeBuilder::with_store_dir`]: kept in this directory.
    Directory(PathBuf),
    /// [`RuntimeBuilder::with_ephemeral_history`]: kept in memory, and
    /// nowhere else.
    Ephemeral,
}

impl History {
    /// The durable store this posture opens, or `None` when the posture is
    /// not a durable one.
    ///
    /// Fallible because opening a file store is what refuses a directory
    /// still holding a basis ≤0.6 database ([`store::store_in`]), and
    /// [`RuntimeBuilder::build`] calls this first — before the credential is
    /// even looked up — so an upgrade trips over the most fundamental fact
    /// about its data before anything else can fail for a smaller reason.
    ///
    /// The unsaid case still opens the directory mentra would have chosen
    /// and drops the result: what is being asked there is whether that
    /// directory is usable at all, since it is where a 0.6 host that named
    /// none kept its history, while mentra's builder goes on picking its own
    /// default.
    pub(super) fn open(posture: Option<&Self>) -> Result<Option<FileRuntimeStore>, RunError> {
        match posture {
            Some(Self::Directory(dir)) => Ok(Some(store::store_in(dir)?)),
            Some(Self::Ephemeral) => Ok(None),
            None => {
                store::store_in(&store::default_directory())?;
                Ok(None)
            }
        }
    }

    /// Where compaction files its snapshots for this posture.
    ///
    /// The same answer applied to the other thing mentra writes about a
    /// conversation. Compaction persists a verbatim snapshot before it
    /// summarizes, and mentra takes the directory for it on the *agent*
    /// config — where a workspace would otherwise inherit a default keyed by
    /// the process's cwd, the hazard [`RuntimeBuilder::with_store_dir`] was
    /// added for. Derived here, beside the store itself, so the two move
    /// together or not at all.
    pub(super) fn transcripts(posture: Option<&Self>) -> PathBuf {
        match posture {
            Some(Self::Directory(dir)) => store::transcripts_in(dir),
            Some(Self::Ephemeral) => store::volatile_transcripts(),
            None => store::default_transcripts(),
        }
    }
}

impl RuntimeBuilder {
    /// Keeps this runtime's conversations in `dir` rather than in the
    /// machine-wide default.
    ///
    /// Unset, mentra chooses, and what it chooses is keyed by the **process's
    /// current directory** rather than by any workspace basis opened — so a host
    /// that opens two workspaces from one place writes both histories to one
    /// place, and a test suite writes into the user's real data directory
    /// whatever temp directory it opened. Two callers want to say
    /// otherwise: a host that keeps basis's history inside its own application
    /// data, and a test that wants no persistent side effect at all. Both are
    /// asking the same question — *where* — so that is what this takes.
    /// [`with_ephemeral_history`](Self::with_ephemeral_history) answers it with
    /// *nowhere*, and is the last word between the two: whichever was called
    /// last decides.
    ///
    /// # What lands in the directory
    ///
    /// `dir` **is** the store's root, and since 0.7 what fills it is plain
    /// files, no database (ADR-0023): `agents/<id>/` holding an `agent.json`,
    /// a `state.json`, a `transcript.jsonl` and a `leaf`, plus a `rules.json`
    /// and a `runs.jsonl` beside them — mentra's own file-store layout,
    /// readable with `grep` and `jq`. Compaction snapshots go in a
    /// `transcripts/` sibling under the same root, so this one call moves
    /// both or neither. Nothing is created until the first write, and
    /// pointing this at
    /// [`store::default_directory`](crate::store::default_directory) is
    /// exactly the default. [`store::list_in`](crate::store::list_in) is how
    /// the same conversations are read back, and it is pointed at the same
    /// directory.
    ///
    /// # A directory from basis 0.6 is refused, not adopted
    ///
    /// basis ≤0.6 kept conversations in a `runtime.sqlite` in this same
    /// directory, and this build neither links SQLite nor migrates
    /// (ADR-0023's E2 precedent). Naming a directory that still holds one
    /// fails [`build`](Self::build) with
    /// [`RunError::LegacyStore`](crate::RunError::LegacyStore) rather than
    /// starting an empty store beside it, which would read as every
    /// conversation having vanished. The ways forward are in the message:
    /// basis 0.6 to continue an old conversation, or this knob pointed
    /// somewhere fresh — `BASIS_DATA_DIR` for the CLI — to start new work.
    ///
    /// # Not the store itself
    ///
    /// Though mentra's `RuntimeBuilder::with_store` would take one.
    /// `RuntimeStore` is a composition of nine traits, and under the
    /// rule written on [`CancellationToken`](crate::CancellationToken) — every
    /// mentra type basis's surface makes a caller *name*, basis re-exports — that
    /// shape would cost the re-export of all nine plus the record types they
    /// pass. What it would buy is reachable without it: between this and
    /// [`with_ephemeral_history`](Self::with_ephemeral_history) a caller
    /// already picks durable-here or nowhere-at-all without naming a mentra
    /// type. A caller that genuinely wants its own backend still has one, on
    /// [`Runtime::mentra_runtime`](crate::Runtime::mentra_runtime)'s side of the bargain: build the mentra
    /// runtime and drive it directly.
    ///
    /// Deliberately not a per-run knob: a run describes an invocation, and
    /// where a machine keeps its history is not something an invocation
    /// decides. A one-shot caller that needs it opens the
    /// [`Workspace`](crate::Workspace) itself and hands
    /// [`WorkspaceBuilder::with_runtime_builder`](crate::WorkspaceBuilder::with_runtime_builder)
    /// a recipe, which is the documented migration path.
    pub fn with_store_dir(self, dir: impl Into<PathBuf>) -> Self {
        Self {
            history: Some(History::Directory(dir.into())),
            ..self
        }
    }

    /// Keeps this runtime's conversations in memory, and nowhere else.
    ///
    /// The sibling of [`with_store_dir`](Self::with_store_dir), for the caller
    /// whose answer to *where* is *nowhere*. mentra's in-memory store backs it:
    /// nothing is written, no tool output is spilled, no directory is
    /// created, and dropping the [`Runtime`](crate::Runtime) takes the history with it.
    ///
    /// One file is still written, and only if a conversation gets long enough
    /// to be summarized: mentra persists a compaction snapshot before it
    /// replaces a prefix of the transcript, and does that without consulting
    /// the store. basis files those under the operating system's temp
    /// directory, unique per runtime — never the user's data directory and
    /// never the workspace.
    ///
    /// **Nothing survives the process.** While the runtime lives a conversation
    /// behaves as it always does — [`Workspace::resume`](crate::Workspace::resume)
    /// finds an agent this runtime minted, because the store lives exactly as
    /// long as the runtime does. Past that edge there is nothing to find: a
    /// later process cannot resume one of these by agent id, a second runtime
    /// gets its own empty store, and
    /// [`store::list_in`](crate::store::list_in) has no file to read whichever
    /// directory it is pointed at, so `session/list` over ACP reports nothing.
    /// There is no flush and no export — a host that might want a transcript
    /// later wants [`with_store_dir`](Self::with_store_dir) now.
    ///
    /// Who asks for it. A test suite, which otherwise writes to the real
    /// database under the user's data directory. And a host whose conversations
    /// are genuinely disposable — a request-scoped run inside a server, a
    /// one-shot classifier — where keeping a transcript is a cost and a
    /// disclosure rather than a feature.
    ///
    /// Setting this and [`with_store_dir`](Self::with_store_dir) is not an
    /// error: they write one field, so the last call wins — the same rule as
    /// every single-valued knob on this builder, and what makes the
    /// half-configured builder this type advertises usable.
    pub fn with_ephemeral_history(self) -> Self {
        Self {
            history: Some(History::Ephemeral),
            ..self
        }
    }

    /// The history directory this recipe names, if any — what
    /// [`Workspace::open`](crate::Workspace::open) derives the workspace
    /// memory root beside ([`crate::memory`]), read here because the private
    /// path resolves memory before the runtime exists.
    pub(crate) fn named_store_dir(&self) -> Option<&Path> {
        match &self.history {
            Some(History::Directory(dir)) => Some(dir),
            _ => None,
        }
    }
}
