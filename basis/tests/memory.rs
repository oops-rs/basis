//! The memory directory convention, through the public surface.
//!
//! What D2's constructive half promises: memory is files, discovered at
//! `Workspace::open`, indexed by frontmatter, shadowed workspace-over-global
//! by name — and off entirely when a caller says so. The prompt-side
//! assertions (the index block, `Replace` removing it) live with the agent
//! config's own tests; what belongs here is the discovery a host observes
//! through `Workspace::memories()`.
//!
//! Every workspace is opened against a closed port with an explicit model id,
//! so nothing is contacted; see `tests/workspace.rs` for why that holds.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use basis::{
    ContextConfig, MemoryConfig, Runtime, RuntimeBuilder, Workspace, WorkspaceBuilder,
    WorkspaceMemoryRoot, hooks::HooksConfig, memory::MemoryKind, skills::SkillsConfig,
    templates::TemplatesConfig, tools::declared::ToolsConfig,
};
use mentra::ModelSelector;

/// A port nothing listens on; reaching it would be a test failure.
const CLOSED_PORT: &str = "http://127.0.0.1:1/v1";

/// A builder that looks nowhere except where the test put something. Memory
/// discovery starts disabled; each test states its own roots.
fn offline(workspace: &Path) -> WorkspaceBuilder {
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
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
        })
}

fn offline_runtime() -> RuntimeBuilder {
    Runtime::builder()
        .with_base_url(CLOSED_PORT)
        .with_api_key("test-key")
        .with_ephemeral_history()
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

fn memory_file(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\ntype: project\n---\nbody\n")
}

#[tokio::test]
async fn memories_are_discovered_and_the_workspace_shadows_the_global_by_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    let workspace_memories = tmp.path().join("workspace-memories");
    let global_memories = tmp.path().join("global-memories");
    write(
        &workspace_memories.join("deploy.md"),
        &memory_file("deploy", "the workspace's version"),
    );
    write(
        &global_memories.join("deploy.md"),
        &memory_file("deploy", "the global version"),
    );
    write(
        &global_memories.join("style.md"),
        &memory_file("style", "only the user has this one"),
    );

    let workspace = offline(&repo)
        .with_memory(MemoryConfig {
            global_root: Some(global_memories),
            workspace_root: WorkspaceMemoryRoot::Dir(workspace_memories),
        })
        .open()
        .await
        .expect("opens");

    let listed: Vec<(&str, &str)> = workspace
        .memories()
        .iter()
        .map(|memory| (memory.name.as_str(), memory.description.as_str()))
        .collect();
    assert_eq!(
        listed,
        [
            ("deploy", "the workspace's version"),
            ("style", "only the user has this one"),
        ],
        "workspace shadows global by name; everything unshadowed still loads"
    );
    assert_eq!(workspace.memories()[0].kind, MemoryKind::Project);
}

#[tokio::test]
async fn a_memory_that_does_not_parse_fails_the_open_naming_the_file() {
    // The posture every discovered file has: missing is nothing, malformed is
    // loud. A silently skipped memory is a note the user believes the agent
    // has and it does not.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    let memories = tmp.path().join("memories");
    write(&memories.join("broken.md"), "---\nname: [unclosed\n---\n");

    let error = offline(&repo)
        .with_memory(MemoryConfig {
            global_root: None,
            workspace_root: WorkspaceMemoryRoot::Dir(memories),
        })
        .open()
        .await
        .expect_err("a malformed memory must fail the open");

    assert!(error.to_string().contains("broken.md"), "{error}");
}

#[tokio::test]
async fn disabled_discovery_reads_nothing() {
    // D9: nothing unconditional. The files can sit right there and a caller
    // that said no is not read.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");

    let workspace = offline(&repo)
        .with_memory(MemoryConfig::disabled())
        .open()
        .await
        .expect("opens");

    assert!(workspace.memories().is_empty());
}

#[tokio::test]
async fn the_workspace_root_derives_beside_the_named_store_dir() {
    // The convention: `with_store_dir("<x>/store")` puts this workspace's
    // memories at `<x>/memory` — beside the history they were learned in,
    // which is how the CLI's layout gets `<data>/workspaces/<key>/memory`
    // without basis knowing the CLI's data dir.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    let store = tmp.path().join("keyed").join("store");
    write(
        &tmp.path().join("keyed").join("memory").join("note.md"),
        &memory_file("note", "learned beside this store"),
    );

    let workspace = offline(&repo)
        .with_runtime_builder(offline_runtime().with_store_dir(&store))
        .with_memory(MemoryConfig {
            global_root: None,
            workspace_root: WorkspaceMemoryRoot::BesideStore,
        })
        .open()
        .await
        .expect("opens");

    assert_eq!(workspace.memories().len(), 1);
    assert_eq!(workspace.memories()[0].name, "note");
    assert_eq!(
        workspace.memories()[0].path,
        tmp.path().join("keyed").join("memory").join("note.md")
    );
}

#[tokio::test]
async fn ephemeral_history_leaves_only_the_global_root() {
    // No store dir was named, so there is no directory basis chose to build
    // beside — the per-workspace root is honestly absent, not invented.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    let global_memories = tmp.path().join("global-memories");
    write(
        &global_memories.join("style.md"),
        &memory_file("style", "the user's"),
    );

    let workspace = offline(&repo)
        .with_memory(MemoryConfig {
            global_root: Some(global_memories),
            workspace_root: WorkspaceMemoryRoot::BesideStore,
        })
        .open()
        .await
        .expect("opens");

    let names: Vec<&str> = workspace
        .memories()
        .iter()
        .map(|memory| memory.name.as_str())
        .collect();
    assert_eq!(names, ["style"]);
}

#[tokio::test]
async fn a_shared_runtimes_store_dir_grants_no_workspace_memory_root_by_default() {
    // G2: BesideStore resolves only on a workspace-bound (private) runtime.
    // A shared runtime's store dir is one runtime-wide fact, not either
    // workspace's alone — if it resolved here, both of these unrelated
    // repositories would derive the identical sibling `memory/` directory and
    // read each other's memory index into their own prompt.
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = tmp.path().join("shared-store");
    write(
        &tmp.path().join("memory").join("note.md"),
        &memory_file("note", "would leak across every workspace if resolved"),
    );

    let runtime = Arc::new(
        offline_runtime()
            .with_store_dir(&store)
            .build()
            .expect("builds offline"),
    );

    let first_repo = tmp.path().join("first");
    std::fs::create_dir_all(&first_repo).expect("create repo");
    let second_repo = tmp.path().join("second");
    std::fs::create_dir_all(&second_repo).expect("create repo");

    let by_default = MemoryConfig {
        global_root: None,
        workspace_root: WorkspaceMemoryRoot::BesideStore,
    };

    let first = offline(&first_repo)
        .with_runtime(Arc::clone(&runtime))
        .with_memory(by_default.clone())
        .open()
        .await
        .expect("opens");
    let second = offline(&second_repo)
        .with_runtime(runtime)
        .with_memory(by_default)
        .open()
        .await
        .expect("opens");

    assert!(
        first.memories().is_empty(),
        "no workspace root on a shared runtime: {:?}",
        first.memories()
    );
    assert!(
        second.memories().is_empty(),
        "no workspace root on a shared runtime: {:?}",
        second.memories()
    );
}

#[tokio::test]
async fn an_explicit_dir_is_still_honored_on_a_shared_runtime() {
    // Naming a path is the host taking responsibility for it — unlike the
    // derived `BesideStore`, an explicit `Dir` is not runtime-wide guesswork,
    // so it is unaffected by G2 either way.
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(
        offline_runtime()
            .with_ephemeral_history()
            .build()
            .expect("builds offline"),
    );

    let first_repo = tmp.path().join("first");
    std::fs::create_dir_all(&first_repo).expect("create repo");
    let first_memories = tmp.path().join("first-memories");
    write(
        &first_memories.join("note.md"),
        &memory_file("note", "this workspace's own"),
    );

    let second_repo = tmp.path().join("second");
    std::fs::create_dir_all(&second_repo).expect("create repo");
    let second_memories = tmp.path().join("second-memories");
    write(
        &second_memories.join("other.md"),
        &memory_file("other", "the other workspace's own"),
    );

    let first = offline(&first_repo)
        .with_runtime(Arc::clone(&runtime))
        .with_memory(MemoryConfig {
            global_root: None,
            workspace_root: WorkspaceMemoryRoot::Dir(first_memories),
        })
        .open()
        .await
        .expect("opens");
    let second = offline(&second_repo)
        .with_runtime(runtime)
        .with_memory(MemoryConfig {
            global_root: None,
            workspace_root: WorkspaceMemoryRoot::Dir(second_memories),
        })
        .open()
        .await
        .expect("opens");

    assert_eq!(first.memories().len(), 1);
    assert_eq!(first.memories()[0].name, "note");
    assert_eq!(second.memories().len(), 1);
    assert_eq!(second.memories()[0].name, "other");
}
