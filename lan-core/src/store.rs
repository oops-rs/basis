//! Where lan's conversations are persisted, and how they are scoped.
//!
//! mentra persists every agent to SQLite and tags each row with a **runtime
//! identifier**. lan uses that tag to answer one question: *which
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
//! The identifier is the canonicalized workspace path with a `lan:` prefix. No
//! hash: mentra hex-encodes identifiers wherever they reach a filename
//! (`SqliteRuntimeStore::path_for_runtime_identifier`), so every character
//! survives, and a readable value is one that can be understood in the
//! database by a person debugging it. The prefix keeps lan's rows from ever
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
//! # Where the file goes
//!
//! mentra's default directory is keyed by the *process's* current directory,
//! not by the workspace lan opened, so every program started from one place
//! shares one database whatever workspace it went on to open — including every
//! test binary in one `cargo test`.
//! [`WorkspaceBuilder::with_store_dir`](crate::WorkspaceBuilder::with_store_dir)
//! is how a caller says otherwise, and [`list_in`] is how the same caller reads
//! back what it wrote. The filename inside that directory is chosen in exactly
//! one place, `store_in`, because two places would eventually disagree and a
//! conversation written to one file and looked for in another is simply
//! missing.
//!
//! # When there is no file
//!
//! [`WorkspaceBuilder::with_ephemeral_history`](crate::WorkspaceBuilder::with_ephemeral_history)
//! answers *where* with *nowhere*, and opens mentra's in-memory store instead.
//! Nothing in this module can see one of those conversations: there is no file
//! for [`list_in`] to read and no row for [`list`] to filter, whichever
//! directory either is pointed at. Everything below is about the durable case.

use std::path::{Path, PathBuf};

use mentra::{
    BuiltinProvider, Runtime,
    runtime::{SqliteRuntimeStore, VolatileRuntimeStore},
};

use crate::run::RunError;

/// Distinguishes lan's rows from every other mentra program sharing the store.
const IDENTIFIER_PREFIX: &str = "lan:";

/// What lan's conversations are kept in, inside whichever directory holds them.
///
/// mentra's own default filename, so a workspace pointed at
/// [`default_directory`] lands on precisely the file it would have used had
/// nobody said anything.
const STORE_FILENAME: &str = "runtime.sqlite";

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

/// A conversation mentra has on disk, as lan reports it.
///
/// lan's own shape rather than a re-export of mentra's `PersistedAgentSummary`,
/// for the same reason [`Event`](crate::Event) is lan's own type: what lan
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
/// [`with_store_dir`](crate::WorkspaceBuilder::with_store_dir) is read by
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
/// [`with_store_dir`](crate::WorkspaceBuilder::with_store_dir). A directory
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
/// this and [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open) read and
/// write one file. The identifier is the other thing that has to agree, and it
/// comes from [`runtime_identifier`] on both sides.
fn enumerating_runtime(identifier: &str, dir: &Path) -> Result<Runtime, RunError> {
    Ok(Runtime::empty_builder()
        .with_runtime_identifier(identifier.to_string())
        .with_store(store_in(dir))
        .with_provider(BuiltinProvider::OpenAI, "unused-for-listing")
        .build()?)
}

/// The store lan keeps a workspace's conversations in, under `dir`.
///
/// The one place the filename is chosen: `WorkspaceBuilder` writes through
/// this and [`list_in`] reads through it, so the two cannot drift.
///
/// Neither this store type nor [`volatile`]'s reaches lan's surface. A caller
/// picks a *posture* — history in a directory, or history nowhere — and lan
/// picks the backend that is it, rather than re-exporting `RuntimeStore` and
/// the nine traits it composes (see
/// [`WorkspaceBuilder::with_store_dir`](crate::WorkspaceBuilder::with_store_dir)).
pub(crate) fn store_in(dir: &Path) -> SqliteRuntimeStore {
    SqliteRuntimeStore::new(dir.join(STORE_FILENAME))
}

/// The store that keeps a workspace's conversations nowhere.
///
/// mentra's in-memory `RuntimeStore`: no file is opened, no transcript snapshot
/// is written, no directory is created, and dropping the runtime that holds it
/// takes every conversation with it. Constructed fresh per workspace, which is
/// what makes two ephemeral workspaces two histories rather than one — the type
/// is `Clone` and clones share state, so a shared instance would be a shared
/// database with none of the durability.
///
/// The backing for
/// [`WorkspaceBuilder::with_ephemeral_history`](crate::WorkspaceBuilder::with_ephemeral_history),
/// and named here rather than in the builder so that the two mentra store types
/// lan can open are chosen in one file.
pub(crate) fn volatile() -> VolatileRuntimeStore {
    VolatileRuntimeStore::new()
}

/// The directory mentra keeps lan's conversations in, for a caller that wants
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
            "lan's rows must not collide with another program's: {identifier}"
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
            "lan's filename must be mentra's, or moving the store would rename it"
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
            "the store lan was told to read is the store it opened"
        );
    }
}
