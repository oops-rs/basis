//! Templates as the commands a client offers.
//!
//! The unit tests beside the mapping cover the shapes. What is worth checking
//! from outside is that the name discovery derives is the name ACP advertises
//! — namespacing included — because that name is the whole contract between a
//! file somebody wrote and a menu somebody clicks.

use std::path::{Path, PathBuf};

use basis_acp::available_commands;
use basis_core::templates::{self, DEFAULT_WORKSPACE_TEMPLATES_DIR, TemplatesConfig};

/// A config with no global root, so nothing on the developer's own machine can
/// change an outcome here.
fn config() -> TemplatesConfig {
    TemplatesConfig {
        workspace_subdir: PathBuf::from(DEFAULT_WORKSPACE_TEMPLATES_DIR),
        global_dir: None,
    }
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).expect("create dir");
    std::fs::write(dir.join(name), body).expect("write template");
}

fn workspace_templates(root: &Path) -> PathBuf {
    root.join(DEFAULT_WORKSPACE_TEMPLATES_DIR)
}

#[test]
fn a_workspace_without_templates_offers_no_commands() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let loaded = templates::load(tmp.path(), &config()).expect("absent is not an error");

    assert!(available_commands(&loaded).is_empty());
}

#[test]
fn discovered_templates_become_acp_commands() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = workspace_templates(tmp.path());
    write(
        &dir,
        "plan.md",
        "---\ndescription: Draft a plan\nargument-hint: <goal>\n---\nPlan $ARGUMENTS.\n",
    );
    write(
        &dir.join("git"),
        "commit.md",
        "---\ndescription: Write a commit message\n---\nSummarize the staged diff.\n",
    );

    let loaded = templates::load(tmp.path(), &config()).expect("load succeeds");
    let commands = available_commands(&loaded);

    let names: Vec<&str> = commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    assert_eq!(names, vec!["git:commit", "plan"]);

    let commit = &commands[0];
    assert_eq!(commit.description, "Write a commit message");
    assert!(
        commit.input.is_none(),
        "a template that declared no hint must not advertise one"
    );
    assert!(
        commands[1].input.is_some(),
        "a declared argument-hint must reach the client"
    );
}
