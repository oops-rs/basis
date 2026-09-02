//! Where basis's conversations are persisted, and how they are scoped.
//!
//! mentra persists every agent to its runtime store — since basis 0.7 the
//! **file-backed** one, plain files under one root, no database (ADR-0023;
//! upstream `mentra#28`) — and tags each record with a **runtime
//! identifier**. basis uses that tag to answer one question: *which
//! conversations belong to this workspace?* — which is what ACP's
//! `session/list` asks and the only reading of "my sessions" that is both
//! honest and useful, since ACP scopes a session to a `cwd` from the moment
//! `session/new` opens it.
//!
//! # Why the workspace path, verbatim
//!
//! mentra's default identifier is the literal string `"default"`, and its
//! default store is one shared root. Listing under `"default"` would
//! therefore enumerate the agents of *every* mentra program on the machine —
//! worse than returning nothing, because a client would offer a user
//! conversations that are not theirs.
//!
//! The identifier is the canonicalized workspace path with a `basis:` prefix. No
//! hash: the identifier never becomes a filename — mentra keeps it as a field
//! of each agent's `agent.json` and filters listings by comparing it — so
//! every character survives, and a readable value is one a person debugging
//! the store can understand by opening the file. The prefix keeps basis's
//! records from ever colliding with another program's `"default"`.
//!
//! Changing the identifier does not move the store — mentra's default path is
//! independent of it — so nothing already written is lost. Records created
//! before this scheme carry `"default"` and do not appear in any workspace's
//! list, which is the correct answer for a conversation whose workspace was
//! never recorded. They are not stranded either: mentra loads an agent by id
//! alone, so resuming one still works, and it re-tags itself the next time it
//! persists. [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open) is where
//! the tag is set, and where that ruling is written down.
//!
//! Every workspace tags its own rows, on a private runtime and on a shared
//! one alike: [`Runtime`](crate::Runtime)'s `mint` states the identifier per
//! session, so one store file serving five repositories still lists each one's
//! conversations apart.
//!
//! **One gap, and it is upstream's.** A *resumed* session carries no
//! identifier: mentra's `SessionResumeOptions` has no field for one, so a
//! resumed conversation persists under the runtime's own tag — its workspace's
//! on a private runtime, and `"basis:runtime"` on a shared one. On a shared
//! runtime, therefore, resuming a conversation and running it takes its row
//! out of that workspace's list, exactly as the `"default"` ruling above
//! takes an unrecorded one out. Nothing is stranded — mentra loads an agent by
//! id and a client that already holds the id can still resume it — but a
//! client that lists to find its conversations will not see that one again.
//! `Runtime::resume_minted` is the one line that changes when the field
//! lands.
//!
//! # Where the files go
//!
//! mentra's default directory is keyed by the *process's* current directory,
//! not by the workspace basis opened, so every program started from one place
//! shares one store root whatever workspace it went on to open — including
//! every test binary in one `cargo test`.
//! [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)
//! is how a caller says otherwise, and [`list_in`] is how the same caller reads
//! back what it wrote. The directory the caller names **is** the store's root
//! — mentra lays `agents/`, `rules.json` and `runs.jsonl` inside it — and the
//! root is bound to the store in exactly one place, `store_in`, because two
//! places would eventually disagree and a conversation written under one root
//! and looked for under another is simply missing.
//!
//! # When the directory holds a database instead
//!
//! basis 0.6 and earlier kept conversations in `runtime.sqlite` under the
//! same directory. This build links no SQLite and does not migrate
//! (ADR-0023's E2 precedent): opening, listing or deleting against a
//! directory that holds one is refused with [`RunError::LegacyStore`], which
//! names the two ways forward, rather than starting an empty file store
//! beside data it cannot see — the refusal is `refuse_legacy_store`, and
//! every path that opens a store the caller pointed somewhere runs it.
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
    runtime::{FileRuntimeStore, PermissionRuleStore, VolatileRuntimeStore},
};

use crate::error::RunError;

/// Distinguishes basis's records from every other mentra program sharing the
/// store.
const IDENTIFIER_PREFIX: &str = "basis:";

/// What basis 0.6 and earlier kept conversations in — mentra's SQLite
/// database, which this build can no longer read (ADR-0023).
///
/// Named so `refuse_legacy_store` can look for exactly the file the old
/// layout put where the file store's root now goes, and refuse by name
/// instead of shadowing it with an empty store.
const LEGACY_SQLITE_FILENAME: &str = "runtime.sqlite";

/// What basis calls the directory of compaction snapshots inside whichever
/// directory holds the store.
///
/// mentra's own name for it: `transcripts/` beside the store's own entries is
/// the layout mentra lays down under its default root, so a workspace pointed
/// at [`default_directory`] lands on exactly the paths it would have used had
/// nobody said anything.
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
    /// Optional because mentra's summary is: a store that keeps nothing
    /// across process lifetimes has no record and therefore no answer, and
    /// basis carries that rather than inventing a number to fill the gap.
    /// Nothing reached through [`list_in`] is one of those — listing opens a
    /// file store of its own, whatever the workspace was running on — so in
    /// practice both of these arrive set. [`list_in`]'s ordering still handles `None`, because a
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

    let mut sessions: Vec<PersistedSession> = enumerating_runtime(&identifier, store_in(dir)?)?
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
/// (sorted `(created_at, id)` for the file store, insertion order for the
/// volatile one), which answers "which is oldest" and never "which was I just
/// in".
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
/// agent id, so this does not check that the id belongs anywhere in particular.
/// [`Workspace::resume`](crate::Workspace::resume) *does* check, and the two
/// differ for a reason — a resume states a workspace's policy and tool audience
/// onto the conversation it picks up, and a deletion states nothing. A caller
/// that means "one of mine" takes the id from [`list`] for its own workspace,
/// which is where a client got it anyway.
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
    let store = store_in(dir)?;
    // A clone of one file store shares its in-process lock, so the clear
    // below mutates through the same concurrency boundary the runtime's
    // deletion does. That boundary is this function's own store pair and
    // nothing wider: a *live workspace* on the same root holds its own
    // independently constructed store, and mentra places two independent
    // stores on one root outside its concurrency contract — so a forget
    // racing a live session's remembered answer can lose one side's write
    // (mentra#50). basis does not add locking of its own over a boundary
    // upstream owns; the race window is one rules.json rewrite.
    let rules = store.clone();

    // The identifier tags rows on write and filters them on `list_agents_by_
    // runtime`; deletion is keyed by id alone, so what is passed here cannot
    // change which conversation goes. It is still derived rather than invented,
    // so nothing in this module opens a store under a tag no workspace uses.
    enumerating_runtime(&runtime_identifier(dir), store)?.delete_agent(agent_id)?;

    // mentra's `delete_agent` removes `agents/<id>/` and nothing else, so the
    // conversation's remembered permission rules — command patterns included
    // — would sit in the store root's `rules.json` forever: a privacy
    // leftover and unbounded growth. Forgetting means the rules too, and it
    // means every row this conversation *wrote*, whatever its scope — a
    // host-seeded Global rule keeps answering prompts store-wide after its
    // writer is gone, which is worse than the dead Session rows. mentra's
    // creator-oriented `clear_rules` deletes by writer id regardless of
    // scope, which is exactly that reading. (Upstream cleaning its own rules
    // on delete is mentra#51; this stays correct either way, because
    // clearing rows that are already gone removes nothing.)
    rules.clear_rules(agent_id)?;

    Ok(())
}

/// A runtime that exists only to reach the store.
///
/// Reading or removing a persisted agent needs a `Runtime`, and
/// `RuntimeBuilder::build` refuses to produce one with an empty provider
/// registry — so a provider is registered to satisfy the builder, with a
/// placeholder key. Nothing here resolves a model or reaches the network:
/// listing reads the store's files and deleting removes some, and the runtime
/// is dropped as soon as it has. Requiring a real credential to touch local
/// records would make `session/list` and `session/delete` fail for a reason
/// that has nothing to do with either.
///
/// The store is passed in — built by the caller through [`store_in`], so this
/// and [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)
/// read and write one file, and a caller that also mutates the store directly
/// ([`forget_in`]'s rule clear) can clone the same instance rather than
/// constructing a second one outside the backend's concurrency boundary. The
/// identifier is the other thing that has to agree, and it comes from
/// [`runtime_identifier`] on both sides.
fn enumerating_runtime(identifier: &str, store: FileRuntimeStore) -> Result<Runtime, RunError> {
    Ok(Runtime::empty_builder()
        .with_runtime_identifier(identifier.to_string())
        .with_store(store)
        .with_provider(BuiltinProvider::OpenAI, "unused-for-listing")
        .build()?)
}

/// The store basis keeps a workspace's conversations in, under `dir` — or the
/// refusal, when `dir` still holds a basis ≤0.6 conversation database.
///
/// The one place the directory becomes a store root: `RuntimeBuilder` writes
/// through this and [`list_in`] reads through it, so the two cannot drift.
/// `dir` itself is the root — mentra lays `agents/`, `rules.json` and
/// `runs.jsonl` inside it — which is what makes 0.7's layout land in exactly
/// the directory 0.6's database sat in.
///
/// **That is why this is fallible, and why the check lives here rather than
/// beside each caller.** A file store started in a directory holding
/// `runtime.sqlite` would look exactly like every conversation being lost, so
/// the check has to happen every time one is opened — and a check a caller
/// has to remember is one a future caller will forget. Constructing the store
/// *is* the check: there is no way to reach a `FileRuntimeStore` for a named
/// directory without passing it.
///
/// mentra's own file store detects the same file, and that is not enough on
/// its own: it errors in mentra's words — naming its `store-sqlite` cargo
/// feature, a fix for a mentra embedder rather than for the person whose
/// history is sitting there — and it raises it from `prepare_recovery`, which
/// `RuntimeHandle::prepare_recovery` treats as best-effort and discards, so
/// nothing legible reaches anyone through `build`. [`RunError::LegacyStore`]
/// says what actually happened and names both ways forward.
///
/// Neither this store type nor [`volatile`]'s reaches basis's surface. A caller
/// picks a *posture* — history in a directory, or history nowhere — and basis
/// picks the backend that is it, rather than re-exporting `RuntimeStore` and
/// the nine traits it composes (see
/// [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)).
pub(crate) fn store_in(dir: &Path) -> Result<FileRuntimeStore, RunError> {
    if dir.join(LEGACY_SQLITE_FILENAME).exists() {
        return Err(RunError::LegacyStore { dir: dir.into() });
    }

    Ok(FileRuntimeStore::new(dir))
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
/// store with none of the durability.
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
/// same conversation the store holds, and mentra's own default puts the two
/// in one directory. Keeping that relationship is what makes
/// [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)
/// move both — and pointing it at [`default_directory`] a no-op, exactly as it
/// is for [`store_in`].
pub(crate) fn transcripts_in(dir: &Path) -> PathBuf {
    dir.join(TRANSCRIPTS_DIRNAME)
}

/// Where they go when nobody said where the history lives.
///
/// Keyed by the process's current directory, like the store beside it — the
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
/// rather than assuming: a host that changes directory changes which store
/// this answers with.
pub fn default_directory() -> PathBuf {
    FileRuntimeStore::default_root()
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
    fn a_chosen_directory_holds_the_layout_the_default_one_would_have() {
        // The identity that makes `with_store_dir` a relocation rather than a
        // second scheme: pointing it at the default directory is a no-op.
        // Asserted as path identity rather than by opening: a machine that ran
        // basis 0.6 has a database in exactly that directory, so opening it is
        // a legitimate refusal (`store_in`'s own guard) and would say nothing
        // about where the two schemes put their roots.
        assert_eq!(
            default_directory(),
            mentra::runtime::FileRuntimeStore::default().root(),
            "basis's root must be mentra's, or moving the store would relocate it"
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
            store.path().join("agents").is_dir(),
            "the store basis was told to read is the store it opened"
        );
    }

    #[test]
    fn a_directory_holding_a_pre_07_database_is_refused_by_name() {
        // basis ≤0.6 kept this workspace's conversations in `runtime.sqlite`
        // under exactly this directory. Listing nothing over it would look
        // like every conversation being lost; the refusal has to say what
        // actually happened and name the ways forward, in basis's words.
        let workspace = tempfile::tempdir().expect("tempdir");
        let store = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            store.path().join(LEGACY_SQLITE_FILENAME),
            b"SQLite format 3\0",
        )
        .expect("plant the old database");

        let error = list_in(store.path(), workspace.path())
            .expect_err("an unreadable existing store must be named, not shadowed");

        let message = error.to_string();
        assert!(message.contains("0.6"), "{message}");
        assert!(message.contains("runtime.sqlite"), "{message}");
        assert!(
            message.contains("not migrated"),
            "the no-migration ruling (ADR-0023) is part of the message: {message}"
        );
        assert!(
            !store.path().join("agents").exists(),
            "a refused directory must not gain an empty store beside the database"
        );
    }

    #[test]
    fn forgetting_refuses_the_same_directory_listing_does() {
        let store = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            store.path().join(LEGACY_SQLITE_FILENAME),
            b"SQLite format 3\0",
        )
        .expect("plant the old database");

        forget_in(store.path(), "some-agent")
            .expect_err("deleting from a database this build cannot read is not a no-op");
    }
}
