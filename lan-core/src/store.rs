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
//! this scheme simply carry `"default"` and do not appear in any workspace's
//! list, which is the correct answer for a conversation whose workspace was
//! never recorded.

use std::path::{Path, PathBuf};

use mentra::{BuiltinProvider, Runtime};

use crate::run::RunError;

/// Distinguishes lan's rows from every other mentra program sharing the store.
const IDENTIFIER_PREFIX: &str = "lan:";

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
/// Teammates are left out. mentra spawns those as an agent's own collaborators;
/// they are internal to a conversation rather than conversations a person
/// started, and offering one to be resumed would be offering something that was
/// never theirs to resume.
///
/// The order is mentra's, which is creation order.
pub fn list(workspace: &Path) -> Result<Vec<PersistedSession>, RunError> {
    let identifier = runtime_identifier(workspace);

    Ok(enumerating_runtime(&identifier)?
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
/// The store is mentra's default, which is also what
/// [`run`](crate::run) builds with — the identifier is the only thing that has
/// to agree, and it comes from [`runtime_identifier`] on both sides.
fn enumerating_runtime(identifier: &str) -> Result<Runtime, RunError> {
    Ok(Runtime::empty_builder()
        .with_runtime_identifier(identifier.to_string())
        .with_provider(BuiltinProvider::OpenAI, "unused-for-listing")
        .build()?)
}

/// The directory mentra keeps lan's conversations in, for a caller that wants
/// to say where the history lives.
pub fn default_directory() -> PathBuf {
    mentra::runtime::SqliteRuntimeStore::default_directory()
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

        assert_eq!(
            list(workspace.path()).expect("listing an empty workspace is not an error"),
            Vec::new(),
            "nothing has ever been persisted here"
        );
    }
}
