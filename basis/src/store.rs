//! Where basis's conversations are persisted, and how they are scoped.
//!
//! mentra persists every agent to SQLite and tags each row with a **runtime
//! identifier**. basis uses that tag to answer one question: *which
//! conversations belong to this workspace?* — which is what ACP's
//! `session/list` asks and the only reading of "my sessions" that is both
//! honest and useful, since ACP scopes a session to a `cwd` from the moment
//! `session/new` opens it.
//!
//! # Why the workspace path, verbatim
//!
//! mentra's default identifier is the literal string `"default"`, and its
//! default store is one shared `runtime.sqlite`. Listing under `"default"`
//! would therefore enumerate the agents of *every* mentra program on the
//! machine — worse than returning nothing, because a client would offer a user
//! conversations that are not theirs.
//!
//! The identifier is the canonicalized workspace path with a `basis:` prefix. No
//! hash: mentra hex-encodes identifiers wherever they reach a filename
//! (`SqliteRuntimeStore::path_for_runtime_identifier`), so every character
//! survives, and a readable value is one that can be understood in the
//! database by a person debugging it. The prefix keeps basis's rows from ever
//! colliding with another program's `"default"`.
//!
//! Changing the identifier does not move the store — mentra's default path is
//! independent of it — so nothing already written is lost. Rows created before
//! this scheme carry `"default"` and do not appear in any workspace's list,
//! which is the correct answer for a conversation whose workspace was never
//! recorded. They are not stranded either: mentra loads an agent by id alone,
//! so resuming one still works, and it re-tags itself the next time it
//! persists. [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open) is where
//! the tag is set, and where that ruling is written down.
//!
//! One caveat since ADR-0018: mentra fixes the tag per *runtime* at build
//! time, so only a workspace on its own private runtime — every
//! `Workspace::open(path)`, the CLI, the free functions — tags rows with its
//! path. A workspace on a **shared** [`Runtime`](crate::Runtime) mints rows
//! tagged `"basis:runtime"` until mentra grows a per-session override; those
//! rows stay out of every per-workspace list (the `"default"` ruling above,
//! applied again) and re-file themselves the first time they persist under a
//! runtime that knows their workspace. [`Runtime`](crate::Runtime)'s `mint` is
//! the one line that changes when the override lands.
//!
//! # Where the file goes
//!
//! mentra's default directory is keyed by the *process's* current directory,
//! not by the workspace basis opened, so every program started from one place
//! shares one database whatever workspace it went on to open — including every
//! test binary in one `cargo test`.
//! [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)
//! is how a caller says otherwise, and [`list_in`] is how the same caller reads
//! back what it wrote. The filename inside that directory is chosen in exactly
//! one place, `store_in`, because two places would eventually disagree and a
//! conversation written to one file and looked for in another is simply
//! missing.
//!
//! # When there is no file
//!
//! [`RuntimeBuilder::with_ephemeral_history`](crate::RuntimeBuilder::with_ephemeral_history)
//! answers *where* with *nowhere*, and opens mentra's in-memory store instead.
//! Nothing in this module can see one of those conversations: there is no file
//! for [`list_in`] to read and no row for [`list`] to filter, whichever
//! directory either is pointed at. One file still gets written even then — a
//! compaction snapshot, which mentra persists without consulting the store —
//! and it goes to a per-runtime directory under the OS temp directory, which
//! is as close to *nowhere* as that file gets. Everything else below is about
//! the durable case.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use mentra::{
    BuiltinProvider, Runtime,
    runtime::{SqliteRuntimeStore, VolatileRuntimeStore},
};

use crate::run::RunError;

/// Distinguishes basis's rows from every other mentra program sharing the store.
const IDENTIFIER_PREFIX: &str = "basis:";

/// What basis's conversations are kept in, inside whichever directory holds them.
///
/// mentra's own default filename, so a workspace pointed at
/// [`default_directory`] lands on precisely the file it would have used had
/// nobody said anything.
const STORE_FILENAME: &str = "runtime.sqlite";

/// What basis calls the directory of compaction snapshots inside whichever
/// directory holds the store.
///
/// mentra's own name for it, for the reason [`STORE_FILENAME`] is mentra's:
/// the pair `runtime.sqlite` + `transcripts/` is the layout mentra lays down
/// under its default root, so a workspace pointed at [`default_directory`]
/// lands on exactly the paths it would have used had nobody said anything.
const TRANSCRIPTS_DIRNAME: &str = "transcripts";

/// The runtime identifier for conversations in `workspace`.
///
/// Every caller that creates or enumerates a conversation must derive it from
/// here — a session filed under one spelling of a path and looked for under
/// another is simply missing.
pub fn runtime_identifier(workspace: &Path) -> String {
    // Canonicalizing is what makes a symlinked path and its target one
    // workspace rather than two. A path that does not exist yet cannot be
    // canonicalized, and is used as written rather than rejected: naming the
    // store is not the place to validate a workspace.
    let resolved = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());

    format!("{IDENTIFIER_PREFIX}{}", resolved.display())
}

/// A conversation mentra has on disk, as basis reports it.
///
/// basis's own shape rather than a re-export of mentra's `PersistedAgentSummary`,
/// for the same reason [`Event`](crate::Event) is basis's own type: what basis
/// publishes should not move because a runtime internal did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSession {
    /// The persisted agent id, which is also the ACP session id (ADR-0007).
    pub agent_id: String,
    /// The name the session was opened under.
    pub name: String,
    /// How many messages the conversation holds. Zero means it was opened and
    /// never used.
    pub messages: usize,
}

/// Every conversation persisted for `workspace`, oldest first.
///
/// Reads the default directory. A workspace opened with
/// [`with_store_dir`](crate::RuntimeBuilder::with_store_dir) is read by
/// [`list_in`] instead — the two have to name the same place, and nothing here
/// can guess which one a caller chose.
///
/// Teammates are left out. mentra spawns those as an agent's own collaborators;
/// they are internal to a conversation rather than conversations a person
/// started, and offering one to be resumed would be offering something that was
/// never theirs to resume.
///
/// The order is mentra's, which is creation order.
pub fn list(workspace: &Path) -> Result<Vec<PersistedSession>, RunError> {
    list_in(&default_directory(), workspace)
}

/// The same, for conversations kept somewhere of the caller's choosing.
///
/// `dir` is what was passed to
/// [`with_store_dir`](crate::RuntimeBuilder::with_store_dir). A directory
/// nothing was ever written to lists nothing rather than failing: an empty
/// history and a store that does not exist yet are the same answer.
pub fn list_in(dir: &Path, workspace: &Path) -> Result<Vec<PersistedSession>, RunError> {
    let identifier = runtime_identifier(workspace);

    Ok(enumerating_runtime(&identifier, dir)?
        .list_persisted_agents(&identifier)?
        .into_iter()
        .filter(|agent| !agent.is_teammate)
        .map(|agent| PersistedSession {
            agent_id: agent.id,
            name: agent.name,
            messages: agent.history_len,
        })
        .collect())
}

/// A runtime that exists only to read the store.
///
/// Reading a persisted agent needs a `Runtime`, and `RuntimeBuilder::build`
/// refuses to produce one with an empty provider registry — so a provider is
/// registered to satisfy the builder, with a placeholder key. Nothing here
/// resolves a model or reaches the network: listing reads a SQLite table, and
/// the runtime is dropped as soon as it has. Requiring a real credential to
/// enumerate local rows would make `session/list` fail for a reason that has
/// nothing to do with listing.
///
/// The store is built from `dir` rather than left at mentra's default, so that
/// this and [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)
/// read and write one file. The identifier is the other thing that has to
/// agree, and it comes from [`runtime_identifier`] on both sides.
fn enumerating_runtime(identifier: &str, dir: &Path) -> Result<Runtime, RunError> {
    Ok(Runtime::empty_builder()
        .with_runtime_identifier(identifier.to_string())
        .with_store(store_in(dir))
        .with_provider(BuiltinProvider::OpenAI, "unused-for-listing")
        .build()?)
}

/// The store basis keeps a workspace's conversations in, under `dir`.
///
/// The one place the filename is chosen: `RuntimeBuilder` writes through
/// this and [`list_in`] reads through it, so the two cannot drift.
///
/// Neither this store type nor [`volatile`]'s reaches basis's surface. A caller
/// picks a *posture* — history in a directory, or history nowhere — and basis
/// picks the backend that is it, rather than re-exporting `RuntimeStore` and
/// the nine traits it composes (see
/// [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)).
pub(crate) fn store_in(dir: &Path) -> SqliteRuntimeStore {
    SqliteRuntimeStore::new(dir.join(STORE_FILENAME))
}

/// The store that keeps a workspace's conversations nowhere.
///
/// mentra's in-memory `RuntimeStore`: no database file is opened, no directory
/// is created, no tool output is spilled to disk, and dropping the runtime that
/// holds it takes every conversation with it. The one thing it does not stop is
/// a compaction snapshot, which mentra writes without asking the store —
/// [`volatile_transcripts`] is where those go instead. Constructed fresh per
/// workspace, which is
/// what makes two ephemeral workspaces two histories rather than one — the type
/// is `Clone` and clones share state, so a shared instance would be a shared
/// database with none of the durability.
///
/// The backing for
/// [`RuntimeBuilder::with_ephemeral_history`](crate::RuntimeBuilder::with_ephemeral_history),
/// and named here rather than in the builder so that the two mentra store types
/// basis can open are chosen in one file.
pub(crate) fn volatile() -> VolatileRuntimeStore {
    VolatileRuntimeStore::new()
}

/// Where a workspace's compaction snapshots go, under `dir`.
///
/// mentra writes the whole transcript to a file before it replaces a prefix of
/// it with a summary, so *somewhere* is not optional; what is optional is
/// whether basis chooses it. It should: a snapshot is a verbatim copy of the
/// same conversation the database holds, and mentra's own default puts the two
/// in one directory. Keeping that relationship is what makes
/// [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)
/// move both — and pointing it at [`default_directory`] a no-op, exactly as it
/// is for [`store_in`].
pub(crate) fn transcripts_in(dir: &Path) -> PathBuf {
    dir.join(TRANSCRIPTS_DIRNAME)
}

/// Where they go when nobody said where the history lives.
///
/// Keyed by the process's current directory, like the database beside it — the
/// hazard `with_store_dir` exists to answer, left in place for the caller that
/// has not asked.
pub(crate) fn default_transcripts() -> PathBuf {
    transcripts_in(&default_directory())
}

/// Where they go for a runtime whose history is kept nowhere.
///
/// *Nowhere* is not on offer for these. mentra's `persist_transcript` writes
/// the snapshot before it summarizes and does not consult the store first —
/// `allows_disk_artifacts`, which the volatile store answers `false` to, gates
/// tool-output spill and nothing else — and `max_persisted_transcripts: None`
/// disables the *cleanup* of old snapshots rather than the writing of new ones.
/// So the only lever basis holds is *where*, and the honest answer is the
/// directory the operating system already treats as disposable: never the
/// user's data directory, never the workspace.
///
/// Unique per call, because two runtimes each promised their own disposable
/// history must not read each other's transcripts out of one directory. A
/// counter and not the clock: two runtimes built in one tick would otherwise
/// share a directory, which is the bug mentra's own mock runtime had.
pub(crate) fn volatile_transcripts() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    std::env::temp_dir()
        .join("basis-ephemeral-transcripts")
        .join(format!(
            "process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
}

/// The directory mentra keeps basis's conversations in, for a caller that wants
/// to say where the history lives.
///
/// Keyed by the process's current directory, which is why it is worth naming
/// rather than assuming: a host that changes directory changes which database
/// this answers with.
pub fn default_directory() -> PathBuf {
    SqliteRuntimeStore::default_directory()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_names_its_workspace_and_is_lans_own() {
        let identifier = runtime_identifier(Path::new("/definitely/not/a/real/path"));

        assert!(
            identifier.starts_with(IDENTIFIER_PREFIX),
            "basis's rows must not collide with another program's: {identifier}"
        );
        assert!(
            identifier.contains("/definitely/not/a/real/path"),
            "a path that cannot be canonicalized is used as written: {identifier}"
        );
    }

    #[test]
    fn two_workspaces_never_share_an_identifier() {
        assert_ne!(
            runtime_identifier(Path::new("/repo/one")),
            runtime_identifier(Path::new("/repo/two"))
        );
    }

    #[test]
    fn one_workspace_reached_two_ways_is_one_identifier() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let nested = workspace.path().join("inner");
        std::fs::create_dir(&nested).expect("dir");

        // The spelling a client sends is whatever it happened to have; the
        // conversation it opened is the same either way.
        assert_eq!(
            runtime_identifier(&nested),
            runtime_identifier(&workspace.path().join("inner").join(".").to_path_buf())
        );
    }

    #[test]
    fn an_unused_workspace_has_no_conversations() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let store = tempfile::tempdir().expect("tempdir");

        assert_eq!(
            list_in(store.path(), workspace.path())
                .expect("listing an empty workspace is not an error"),
            Vec::new(),
            "nothing has ever been persisted here"
        );
    }

    #[test]
    fn a_chosen_directory_holds_the_file_the_default_one_would_have() {
        // The identity that makes `with_store_dir` a relocation rather than a
        // second scheme: pointing it at the default directory is a no-op.
        let store = store_in(&default_directory());

        assert_eq!(
            store.path(),
            mentra::runtime::SqliteRuntimeStore::default().path(),
            "basis's filename must be mentra's, or moving the store would rename it"
        );
    }

    #[test]
    fn listing_opens_the_store_it_was_pointed_at() {
        // Listing is the one path that opens a store without a workspace
        // having been opened first, so it is the one most likely to fall back
        // to the machine-wide default without anyone noticing.
        let workspace = tempfile::tempdir().expect("tempdir");
        let store = tempfile::tempdir().expect("tempdir");

        list_in(store.path(), workspace.path()).expect("listing is not an error");

        assert!(
            store.path().join(STORE_FILENAME).exists(),
            "the store basis was told to read is the store it opened"
        );
    }
}
