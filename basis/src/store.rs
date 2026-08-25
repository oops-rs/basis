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

use crate::error::RunError;

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
    /// When the conversation was first written, in seconds since the epoch.
    ///
    /// Optional because mentra's summary is: a store that keeps nothing across
    /// process lifetimes has no row and therefore no answer, and basis carries
    /// that rather than inventing a number to fill the gap. Nothing reached
    /// through [`list_in`] is one of those — listing opens a SQLite store of
    /// its own, whatever the workspace was running on — so in practice both of
    /// these arrive set. [`list_in`]'s ordering still handles `None`, because a
    /// rule that only works for the values it happens to see is not a rule.
    pub created_at: Option<u64>,
    /// When it was last written, on the same clock and absent in the same case.
    ///
    /// This is *last persisted*, not last spoken: mentra rewrites an agent's
    /// row on every turn and on every `set_model`, `set_effort` or `set_name`,
    /// so it moves for a rename as readily as for an exchange. Close enough to
    /// "last activity" to sort a list by, and not close enough to bill by.
    pub updated_at: Option<u64>,
}

/// Every conversation persisted for `workspace`, most recently used first.
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

    let mut sessions: Vec<PersistedSession> = enumerating_runtime(&identifier, dir)?
        .list_persisted_agents(&identifier)?
        .into_iter()
        .filter(|agent| !agent.is_teammate)
        .map(|agent| PersistedSession {
            agent_id: agent.id,
            name: agent.name,
            messages: agent.history_len,
            created_at: agent.created_at,
            updated_at: agent.updated_at,
        })
        .collect();
    by_recency(&mut sessions);

    Ok(sessions)
}

/// Orders conversations the way a person looks for one: the last thing they
/// touched at the top.
///
/// Ordered here rather than at each surface, because there are two of them —
/// `basis list` and ACP's `session/list` — and a client that sorted for itself
/// would need a timestamp basis might not have. mentra returns creation order
/// (`ORDER BY created_at, id` for SQLite, insertion order for the volatile
/// store), which answers "which is oldest" and never "which was I just in".
///
/// Two rules, and the second is what makes the answer usable at all:
///
/// - A conversation with no timestamp sorts **last**, not first. `None` comes
///   from a store that persists nothing, so it is *unknown*, and floating an
///   unknown to the top of a list sorted by recency claims the one thing it
///   cannot know.
/// - The sort is **stable**, so conversations that tie — the same second, or a
///   volatile store where every one of them is `None` — keep the order mentra
///   gave them, which is deterministic in both backends. A tiebreak of basis's
///   own would replace a meaningful order with an arbitrary one.
fn by_recency(sessions: &mut [PersistedSession]) {
    sessions.sort_by(|left, right| match (left.updated_at, right.updated_at) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

/// Removes a conversation from the default directory's store for good.
///
/// The writing counterpart to [`list`], and the second half of what a client
/// with a session list can do with it: pick one to resume, or decide it is
/// finished with. mentra removes the record *and* its memory, because a record
/// without its memory is a row `resume` refuses with "missing persisted
/// memory" — a listing entry that cannot be opened.
///
/// Deleting a conversation that is not there is **not** an error. A caller
/// deleting by an id it read from a list is racing anyone else holding the
/// same store, and "it is gone" is the outcome both of them asked for.
///
/// Keyed by the conversation, not by a workspace: mentra's store is indexed by
/// agent id, so this does not check that the id belongs anywhere in
/// particular — the same ruling [`Workspace::resume`](crate::Workspace::resume)
/// makes for the same reason. A caller that means "one of mine" takes the id
/// from [`list`] for its own workspace, which is where a client got it anyway.
///
/// **A live conversation is not stopped by this.** mentra deletes rows; an
/// agent still in memory keeps running and writes its row back on its next
/// persist. A caller holding a [`PreparedRun`](crate::PreparedRun) on this id
/// must drop it first, or the row returns.
pub fn forget(agent_id: &str) -> Result<(), RunError> {
    forget_in(&default_directory(), agent_id)
}

/// The same, for conversations kept somewhere of the caller's choosing.
///
/// `dir` is what was passed to
/// [`with_store_dir`](crate::RuntimeBuilder::with_store_dir), exactly as for
/// [`list_in`]: a conversation is deleted from the file it was listed out of,
/// and nothing here can guess which one a caller chose.
pub fn forget_in(dir: &Path, agent_id: &str) -> Result<(), RunError> {
    // The identifier tags rows on write and filters them on `list_agents_by_
    // runtime`; deletion is keyed by id alone, so what is passed here cannot
    // change which conversation goes. It is still derived rather than invented,
    // so nothing in this module opens a store under a tag no workspace uses.
    Ok(enumerating_runtime(&runtime_identifier(dir), dir)?.delete_agent(agent_id)?)
}

/// A runtime that exists only to reach the store.
///
/// Reading or removing a persisted agent needs a `Runtime`, and
/// `RuntimeBuilder::build` refuses to produce one with an empty provider
/// registry — so a provider is registered to satisfy the builder, with a
/// placeholder key. Nothing here resolves a model or reaches the network:
/// listing reads a SQLite table and deleting writes one, and the runtime is
/// dropped as soon as it has. Requiring a real credential to touch local rows
/// would make `session/list` and `session/delete` fail for a reason that has
/// nothing to do with either.
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

    fn session(agent_id: &str, updated_at: Option<u64>) -> PersistedSession {
        PersistedSession {
            agent_id: agent_id.to_string(),
            name: agent_id.to_string(),
            messages: 2,
            created_at: updated_at,
            updated_at,
        }
    }

    fn ordered(sessions: Vec<PersistedSession>) -> Vec<String> {
        let mut sessions = sessions;
        by_recency(&mut sessions);
        sessions
            .into_iter()
            .map(|session| session.agent_id)
            .collect()
    }

    #[test]
    fn the_conversation_touched_last_is_listed_first() {
        assert_eq!(
            ordered(vec![
                session("older", Some(100)),
                session("newest", Some(300)),
                session("middle", Some(200)),
            ]),
            vec!["newest", "middle", "older"]
        );
    }

    #[test]
    fn a_conversation_with_no_timestamp_sorts_last_rather_than_first() {
        // `None` is a store that persists nothing, which means *unknown* — and
        // an unknown floated to the top of a list sorted by recency claims the
        // one thing it cannot know.
        assert_eq!(
            ordered(vec![
                session("unknown", None),
                session("known", Some(1)),
                session("also-unknown", None),
            ]),
            vec!["known", "unknown", "also-unknown"]
        );
    }

    #[test]
    fn conversations_that_tie_keep_the_order_the_store_gave_them() {
        // The volatile store's whole case — every timestamp `None` — plus the
        // ordinary one of two conversations written in the same second. Both
        // have to come back in mentra's own order, which is deterministic in
        // either backend; a tiebreak of basis's own would replace a meaningful
        // order with an arbitrary one.
        assert_eq!(
            ordered(vec![
                session("first", None),
                session("second", None),
                session("third", None),
            ]),
            vec!["first", "second", "third"]
        );
        assert_eq!(
            ordered(vec![
                session("first", Some(7)),
                session("second", Some(7)),
                session("third", Some(9)),
            ]),
            vec!["third", "first", "second"]
        );
    }

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
