//! Which workspace's `session/list` a conversation shows up under.

use std::{path::Path, sync::Arc};

use basis::{AllowAll, CollectingSink, RunOutcome, Runtime, store};
use mentra::{
    BuiltinProvider, ContentBlock, agent::AgentConfig, runtime::FileRuntimeStore, test::MockRuntime,
};

use crate::endpoint::ScriptedEndpoint;

use super::{CLOSED_PORT, offline, offline_runtime, offline_shared, write};

/// Every conversation a workspace mints is tagged with that workspace, which
/// is the whole of what makes listing possible.
///
/// The tag is mentra's runtime identifier and basis derives it from the
/// workspace path ([`store::runtime_identifier`]). Until `WorkspaceBuilder::open`
/// set one, everything basis persisted carried mentra's `"default"` while
/// `store::list_in` filtered on the workspace's — so listing had never returned
/// a conversation basis itself had written, and no test noticed because none of
/// them wrote one and then looked.
#[tokio::test]
async fn a_conversation_is_listed_for_the_workspace_that_minted_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store_dir.path()))
        .open()
        .await
        .expect("opens");
    let agent_id = workspace
        .prepare("go")
        .expect("mints")
        .agent_id()
        .to_string();

    let listed = store::list_in(store_dir.path(), dir.path()).expect("lists");

    assert_eq!(
        listed
            .iter()
            .map(|session| session.agent_id.as_str())
            .collect::<Vec<_>>(),
        vec![agent_id.as_str()],
        "a conversation this workspace minted must be one this workspace lists"
    );
}

/// A conversation resumed on a *shared* runtime and used again keeps listing
/// under its own workspace, rather than re-filing under the runtime's.
///
/// mentra used to carry no runtime identifier through a resume, so the next
/// persist re-tagged the row with the runtime's own — invisible on a private
/// runtime, where that tag already matches the one workspace it serves, but
/// real on a shared one: a resumed-then-run conversation would drop out of
/// `store::list_in` for the workspace that minted it (mentra#54). mentra 0.27
/// closes it by retaining the row's own stored identifier through every later
/// save, which is why this test needs no basis-side fix beside it —
/// `Runtime::resume_minted` states nothing about the identifier at all.
///
/// [`offline_shared`] is what makes the gap reachable: its runtime is tagged
/// `"basis:runtime"`, provably not this workspace's own `basis:<path>`, so a
/// silent fallback to the runtime's tag would be caught here rather than
/// hidden by the two happening to agree.
#[tokio::test]
async fn a_resumed_conversation_keeps_listing_under_its_own_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let shared = Arc::new(
        Runtime::builder()
            .with_base_url(CLOSED_PORT)
            .with_api_key("test-key")
            .with_store_dir(store_dir.path())
            .build()
            .expect("builds offline"),
    );

    let opened = offline_shared(dir.path(), shared.clone())
        .open()
        .await
        .expect("opens");
    let agent_id = {
        // Scoped so the mint's own lease is released before the resume below
        // takes it again.
        opened.prepare("go").expect("mints").agent_id().to_string()
    };
    assert_eq!(
        listed_ids(store_dir.path(), dir.path()),
        vec![agent_id.clone()],
        "a fresh mint lists under its own workspace, not the shared runtime's"
    );

    let mut resumed = opened.resume(&agent_id, "again").expect("resumes");
    // Renaming rewrites the agent's row — the persist that used to lose the
    // tag (`the_conversation_touched_last_is_listed_first` uses the same
    // trigger).
    resumed
        .set_name("touched after a resume")
        .expect("renames, which persists");

    assert_eq!(
        listed_ids(store_dir.path(), dir.path()),
        vec![agent_id],
        "a resumed-then-persisted conversation must keep listing under its own \
         workspace rather than falling back to the shared runtime's own tag"
    );
}

/// A list is read to find the conversation you were just in, so that one is at
/// the top — even when it is the oldest.
///
/// The discriminating shape, and the reason listing by creation was never the
/// answer: the conversation minted *first* is the one touched *last*, so an
/// order by `created_at` and an order by `updated_at` disagree, and only one of
/// them puts the right row first. mentra's store keeps both columns and
/// `PersistedAgentSummary` now carries them; before that, basis had nothing to
/// sort by and said so.
///
/// The sleep is not decoration. mentra's timestamps are whole seconds, so two
/// writes inside one second are a tie the stable sort deliberately leaves
/// alone — which is exactly what this test would then be checking.
#[tokio::test]
async fn the_conversation_touched_last_is_listed_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store_dir.path()))
        .open()
        .await
        .expect("opens");

    let mut first = workspace.prepare("first").expect("mints");
    let first_id = first.agent_id().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let second_id = workspace
        .prepare("second")
        .expect("mints")
        .agent_id()
        .to_string();

    assert_eq!(
        listed_ids(store_dir.path(), dir.path()),
        vec![second_id.clone(), first_id.clone()],
        "creation order is what mentra returns, and it is the reverse of this"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    // Renaming rewrites the agent's row, which is what "used" means to a store
    // that records when it last wrote one. Nothing about *creation* changed,
    // so an order that followed `created_at` would not move.
    first.set_name("came back to this one").expect("renames");

    let listed = store::list_in(store_dir.path(), dir.path()).expect("lists");
    assert_eq!(
        listed
            .iter()
            .map(|session| session.agent_id.clone())
            .collect::<Vec<_>>(),
        vec![first_id, second_id],
        "the conversation that was returned to is the one at the top"
    );

    let revisited = &listed[0];
    let created_at = revisited.created_at.expect("a durable store records both");
    let updated_at = revisited.updated_at.expect("a durable store records both");
    assert!(
        updated_at > created_at,
        "a conversation that was written twice must not report one instant: \
         created {created_at}, updated {updated_at}"
    );
}

fn listed_ids(store_dir: &Path, workspace: &Path) -> Vec<String> {
    store::list_in(store_dir, workspace)
        .expect("lists")
        .into_iter()
        .map(|session| session.agent_id)
        .collect()
}

#[tokio::test]
async fn one_workspace_does_not_list_anothers_conversations() {
    // The discriminating half: two workspaces sharing one store file, which is
    // the arrangement every basis on one machine is in by default.
    let mine = tempfile::tempdir().expect("tempdir");
    let theirs = tempfile::tempdir().expect("tempdir");
    write(&mine.path().join("AGENTS.md"), "house rules");
    write(&theirs.path().join("AGENTS.md"), "other rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(mine.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store_dir.path()))
        .open()
        .await
        .expect("opens");
    workspace.prepare("go").expect("mints");

    assert!(
        store::list_in(store_dir.path(), theirs.path())
            .expect("lists")
            .is_empty(),
        "offering a person another repository's conversations is worse than offering none"
    );
}

/// A conversation written before workspaces were tagged is still resumable —
/// but, since mentra 0.27, no longer joins its workspace's list the first
/// time it is used.
///
/// That self-healing was the back-compat answer 0.7 gave: nothing migrates an
/// old record, but *using* one used to adopt it, because every persist
/// re-derived the tag from the live runtime's own current identifier,
/// unconditionally overwriting whatever the row already carried. mentra 0.27
/// changed that specifically to fix mentra#54 (a resumed-then-run
/// conversation on a *shared* runtime re-filing under the runtime's generic
/// tag): `Agent::from_loaded` now rebinds to the row's own stored identifier
/// and carries it forward, so a legacy `"default"`-tagged row stays
/// `"default"`-tagged forever — there is no longer a code path that adopts
/// it, on a private runtime or a shared one. `SessionResumeOptions` still has
/// no field to override the tag on resume (the alternative mentra#54's own
/// "Ask" also named and did not take up), so basis cannot restate it either;
/// filed as [mentra#59](https://github.com/oops-rs/mentra/issues/59).
///
/// The record below carries the workspace as its agent's `base_dir`, because
/// that is what every basis that ever wrote one carried: the agent config has
/// been scoped to the opened workspace since the first `run`, long before the
/// tag existed. It is also what makes the conversation *this* workspace's for
/// `Workspace::resume`'s binding check — the tag never gated resuming, and the
/// base directory always did name the repository. Resuming and listing by id
/// both still work; only self-filing into the list is gone.
#[tokio::test]
async fn a_conversation_tagged_before_workspaces_were_is_resumable_but_no_longer_files_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    // What every basis before this fix wrote: mentra's own default tag.
    let agent_id = {
        let mock = MockRuntime::builder()
            .model("test-model", BuiltinProvider::OpenAI)
            .runtime_identifier("default")
            .with_store(FileRuntimeStore::new(store_dir.path()))
            .text("from before")
            .build()
            .expect("the mock runtime builds");
        let mut session = mock
            .runtime()
            .create_session_with_config(
                "old",
                mock.model(),
                AgentConfig {
                    workspace: mentra::agent::WorkspaceConfig {
                        base_dir: dir.path().to_path_buf(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .expect("session");
        session
            .append_turn(vec![ContentBlock::text("hello")])
            .await
            .expect("a scripted turn completes");

        session.agent_id().to_string()
    };

    assert!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .is_empty(),
        "an untagged conversation is not claimed by a workspace it never recorded"
    );

    let endpoint = ScriptedEndpoint::start();
    let workspace = offline(dir.path())
        .with_runtime_builder(
            offline_runtime()
                .with_base_url(&endpoint.base_url)
                .with_store_dir(store_dir.path()),
        )
        .open()
        .await
        .expect("opens");
    let report = workspace
        .resume(&agent_id, "again")
        .expect("an old conversation is still resumable")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the resumed run completes");

    assert!(matches!(report.outcome, RunOutcome::Ok));
    assert!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .is_empty(),
        "mentra#54's fix (resume preserves a row's own stored tag) means using \
         an old conversation no longer adopts it into this workspace's list — \
         a real loss, tracked as mentra#59, not a choice basis made"
    );
}
