//! A workspace opened once, minting runs cheaply — and concurrently.
//!
//! Two claims are checked here, and they are the two ADR-0010 made when it
//! asked for this split:
//!
//! 1. **Discovery happens at open.** A run minted afterwards carries what the
//!    workspace found, not what the filesystem says at mint time. The test for
//!    that deletes the context file between the two and expects the run to be
//!    unaffected — a per-run discovery would notice.
//! 2. **Runs minted from one workspace are independent and can be driven
//!    together.** The concurrency test drives two of them against a scripted
//!    endpoint on loopback and expects each to get its own answer.
//!
//! One test crate rather than five, which is what a directory with a `main.rs`
//! buys: [`resume`] is resuming a conversation and what it does and does not
//! carry forward, [`listing`] is which workspace's `session/list` a
//! conversation shows up under, and [`endpoint`] is the harness that drives a
//! real turn against a scripted loopback HTTP endpoint plus every test that
//! needs it — a second, third and fourth `tests/*.rs` would each compile and
//! link their own copy of the workspace-builder harness below.

mod endpoint;
mod listing;
mod resume;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use basis::{
    ContextConfig, MemoryConfig, Runtime, RuntimeBuilder, Snapshot, Workspace, WorkspaceBuilder,
    hooks::HooksConfig, skills::SkillsConfig, store, templates::TemplatesConfig,
    tools::declared::ToolsConfig,
};
use mentra::ModelSelector;

/// A port nothing listens on. Reaching it would be a test failure rather than a
/// hang, but no code path here should try.
pub(crate) const CLOSED_PORT: &str = "http://127.0.0.1:1/v1";

/// A builder that looks nowhere except where the test put something, and that
/// contacts nothing while opening.
///
/// The credential is supplied rather than read from the environment, so the
/// suite behaves the same whether or not the person running it has a key
/// exported. An explicit model id short-circuits model resolution, which is the
/// only part of opening a workspace that would otherwise make a request. The
/// history is ephemeral, so nothing here writes to the database under the
/// user's data directory — and the tests that are *about* persistence say
/// [`basis::RuntimeBuilder::with_store_dir`] afterwards, which is the last
/// word. The process knobs ride on the private runtime's recipe, where
/// ADR-0018 moved them; everything else is still the workspace's.
pub(crate) fn offline(workspace: &Path) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_runtime_builder(offline_runtime())
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_memory(MemoryConfig::disabled())
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
}

/// The process half of [`offline`], for the tests that re-say a runtime knob:
/// `with_runtime_builder` replaces the whole recipe, so a test that wants the
/// offline defaults plus one change starts from here.
pub(crate) fn offline_runtime() -> RuntimeBuilder {
    Runtime::builder()
        .with_base_url(CLOSED_PORT)
        .with_api_key("test-key")
        .with_ephemeral_history()
}

pub(crate) fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

/// [`offline`], but attached to a runtime the caller already built and may
/// share with another workspace — `RuntimeBuilder::build`'s public path
/// rather than the private one `with_runtime_builder` drives, and tagged
/// `"basis:runtime"` rather than any one workspace's identity (see
/// [`store::runtime_identifier`]). The shape a shared host like `basis-host`
/// is in, and the one `listing`'s
/// `a_resumed_conversation_keeps_listing_under_its_own_workspace` needs: a
/// runtime whose own tag provably differs from the workspace's.
pub(crate) fn offline_shared(workspace: &Path, runtime: Arc<Runtime>) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_runtime(runtime)
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_memory(MemoryConfig::disabled())
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
}

#[tokio::test]
async fn context_is_discovered_at_open_not_at_mint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let agents = dir.path().join("AGENTS.md");
    write(&agents, "house rules");

    let workspace = offline(dir.path()).open().await.expect("opens");

    // If minting re-discovered, this deletion would empty the run's context.
    std::fs::remove_file(&agents).expect("remove");
    let run = workspace.prepare("go").expect("mints");

    let documents = run.context().context.documents();
    assert_eq!(documents.len(), 1, "the run keeps what the open found");
    assert!(documents[0].content.contains("house rules"));
}

#[tokio::test]
async fn a_skill_the_model_may_not_reach_is_reported_as_one() {
    // `disable-model-invocation` keeps a skill out of the model's list and
    // makes `load_skill` refuse it, while leaving it in the set a host is
    // shown. A host is the only one who can act on that, so the workspace's
    // report has to carry the distinction rather than hand back two entries
    // that look alike and behave differently.
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        &dir.path().join(".basis/skills/release/SKILL.md"),
        "---\nname: release\ndescription: cut a release\ndisable-model-invocation: true\n---\nSteps.",
    );
    write(
        &dir.path().join(".basis/skills/review/SKILL.md"),
        "---\nname: review\ndescription: review a diff\n---\nSteps.",
    );

    let workspace = offline(dir.path()).open().await.expect("opens");

    let reported: Vec<(&str, bool)> = workspace
        .skills()
        .iter()
        .map(|skill| (skill.name.as_str(), skill.model_invocable))
        .collect();

    assert_eq!(reported, [("release", false), ("review", true)]);
}

#[tokio::test]
async fn every_run_from_one_workspace_reports_the_same_resolution() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let workspace = offline(dir.path()).open().await.expect("opens");
    let first = workspace.prepare("one").expect("mints");
    let second = workspace.prepare("two").expect("mints");

    assert_eq!(first.context().model, second.context().model);
    assert_eq!(first.context().provider, second.context().provider);
    assert_eq!(first.context().workspace, second.context().workspace);
    assert_eq!(first.context().prompt, "one");
    assert_eq!(second.context().prompt, "two");
    assert_ne!(
        first.session_id(),
        second.session_id(),
        "two runs are two conversations"
    );
    assert_ne!(
        first.agent_id(),
        second.agent_id(),
        "and two persisted agents"
    );
}

#[tokio::test]
async fn a_spec_bounds_only_the_run_it_was_given_to() {
    use std::time::Duration;

    use basis::RunSpec;

    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let workspace = offline(dir.path()).open().await.expect("opens");
    let bounded = workspace
        .prepare(RunSpec::new("careful").with_deadline(Duration::from_secs(30)))
        .expect("mints");
    let unbounded = workspace.prepare("whatever it takes").expect("mints");

    assert_eq!(
        bounded.bounds().bounds.deadline,
        Some(Duration::from_secs(30))
    );
    assert_eq!(unbounded.bounds().bounds.deadline, None);
}

/// Conversations are persisted where the caller said, and nowhere else.
///
/// The discriminating half is the last one: without a store directory both
/// workspaces would fall back to the same machine-wide default and *every*
/// resume would succeed, so a test that only opened the store twice would pass
/// whether or not the knob did anything.
#[tokio::test]
async fn a_conversation_is_found_again_only_through_the_directory_it_was_written_to() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");

    let opened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    let agent_id = opened.prepare("go").expect("mints").agent_id().to_string();
    drop(opened);

    assert!(
        std::fs::read_dir(store.path())
            .expect("the store directory was created")
            .next()
            .is_some(),
        "minting a run persists an agent, and it persists it where the caller said"
    );

    let reopened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect("opens");
    assert_eq!(
        reopened
            .resume(&agent_id, "again")
            .expect("the conversation is in the store it was written to")
            .agent_id(),
        agent_id
    );

    let reopened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(elsewhere.path()))
        .open()
        .await
        .expect("opens");
    assert!(
        reopened.resume(&agent_id, "again").is_err(),
        "a different directory is a different history"
    );
}

/// A workspace that keeps its history nowhere is still a workspace.
///
/// The knob's floor. Swapping the backing store is exactly the kind of change
/// that looks fine until a turn is driven through it: minting persists an
/// agent, every round loads and saves it again, and resuming reads it back —
/// all through the store, none of it exercised by opening one.
#[tokio::test]
async fn an_ephemeral_workspace_leaves_the_directory_it_was_offered_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let workspace = offline(dir.path())
        .with_runtime_builder(
            offline_runtime()
                .with_store_dir(store_dir.path())
                .with_ephemeral_history(),
        )
        .open()
        .await
        .expect("opens");
    workspace.prepare("go").expect("mints");

    assert_eq!(
        std::fs::read_dir(store_dir.path())
            .expect("the directory the test made")
            .count(),
        0,
        "minting a run persists an agent, and this one persists it nowhere"
    );
    // Ordered after the directory check on purpose: listing opens the store it
    // is pointed at, so asking first would create the very file being denied.
    assert!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .is_empty(),
        "and there is nothing to list either"
    );
}

/// Nothing outlives the workspace: no resume by agent id, nothing to list.
///
/// What `with_ephemeral_history` promises about a later *process*, proved here
/// without starting one — a second `Workspace::open` gets a store of its own
/// exactly as a second process would. The second one keeps real history, which
/// is the sharpest form of the question: it has a database, it is pointed at
/// the same workspace, and the conversation is still not in it.
#[tokio::test]
async fn an_ephemeral_conversation_is_gone_once_its_workspace_is() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store_dir = tempfile::tempdir().expect("tempdir");

    let opened = offline(dir.path()).open().await.expect("opens");
    let agent_id = opened.prepare("go").expect("mints").agent_id().to_string();
    drop(opened);

    let reopened = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store_dir.path()))
        .open()
        .await
        .expect("opens");

    assert!(
        reopened.resume(&agent_id, "again").is_err(),
        "an ephemeral conversation cannot be resumed from anywhere else"
    );
    assert!(
        store::list_in(store_dir.path(), dir.path())
            .expect("lists")
            .is_empty(),
        "nor can it be found by looking"
    );
}

/// Opening a workspace over a basis ≤0.6 store — `runtime.sqlite` in the
/// directory `with_store_dir` names — is refused in basis's words, before any
/// empty file store appears beside the database (ADR-0023: files, no
/// migration). The CLI reads this exact message off `Workspace::open`, so the
/// operator-facing wording is pinned here once for every surface.
#[tokio::test]
async fn a_workspace_over_a_pre_07_store_is_refused_in_basis_words() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");
    let store = tempfile::tempdir().expect("tempdir");
    write_bytes(&store.path().join("runtime.sqlite"), b"SQLite format 3\0");

    let error = offline(dir.path())
        .with_runtime_builder(offline_runtime().with_store_dir(store.path()))
        .open()
        .await
        .expect_err("a database this build cannot read must be named, not shadowed");

    let message = error.to_string();
    assert!(message.contains("basis 0.6 or earlier"), "{message}");
    assert!(message.contains("runtime.sqlite"), "{message}");
    assert!(message.contains("not migrated"), "{message}");
    assert!(
        message.contains("BASIS_DATA_DIR"),
        "the CLI operator's way forward is named: {message}"
    );
    assert!(
        !store.path().join("agents").exists(),
        "a refused directory must not gain an empty store beside the database"
    );
}

pub(crate) fn write_bytes(path: &Path, body: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

#[tokio::test]
async fn a_workspace_fingerprints_itself_as_it_is_now() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("AGENTS.md"), "house rules");

    let workspace = offline(dir.path()).open().await.expect("opens");
    let Snapshot::Known(before) = workspace.fingerprint() else {
        panic!("a workspace with a file in it fingerprints");
    };

    // ADR-0014 kept the fingerprint so a caller's loop can skip an unchanged
    // workspace. That only works if it reads the tree now rather than as it
    // was when the workspace was opened.
    write(&dir.path().join("new.txt"), "arrived later");
    let Snapshot::Known(after) = workspace.fingerprint() else {
        panic!("a workspace with two files in it fingerprints");
    };

    assert_ne!(before, after);
}
