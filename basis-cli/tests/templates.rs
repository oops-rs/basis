//! `/name` at a shell, through the real binary.
//!
//! No scripted endpoint: `--resumable` mints the checkpoint and drives
//! nothing, so `meta.json` is a complete record of what the invocation asked
//! for — which is exactly the claim these tests make. The rendered text is
//! what the task records, so `basis watch` and `basis list` show the question
//! that was really asked rather than the shorthand for it.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use serde_json::Value;

struct Fixture {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    data: PathBuf,
}

impl Fixture {
    /// A workspace holding `templates`, each `(path below .basis/templates,
    /// file body)`.
    fn with(templates: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let data = root.path().join("data");
        fs::create_dir_all(&workspace).expect("workspace");
        for (path, body) in templates {
            let file = workspace.join(".basis/templates").join(path);
            fs::create_dir_all(file.parent().expect("parent")).expect("templates dir");
            fs::write(file, body).expect("write template");
        }
        Self {
            _root: root,
            workspace,
            data,
        }
    }

    fn basis(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_basis"));
        command
            .env("BASIS_DATA_DIR", &self.data)
            .env("BASIS_API_KEY", "test-key")
            .env_remove("BASIS_TASK_ID")
            .args(args)
            .arg("-C")
            .arg(&self.workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().expect("run basis")
    }

    /// The prompt a `--resumable` spawn recorded for `prompt`.
    fn recorded(&self, argv: &[&str]) -> String {
        let output = self.basis(argv);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let task = stdout
            .lines()
            .find_map(|line| line.strip_prefix("task "))
            .and_then(|line| line.split_once(':').map(|(task, _)| task.to_string()))
            .unwrap_or_else(|| panic!("no handle in: {stdout}"));
        let (key, id) = task.split_once('/').expect("handle shape");
        let meta = self
            .data
            .join("workspaces")
            .join(key)
            .join("agents")
            .join(id)
            .join("meta.json");
        let meta: Value =
            serde_json::from_slice(&fs::read(meta).expect("meta.json")).expect("meta is JSON");
        meta["prompt"].as_str().expect("a prompt").to_string()
    }
}

const FIX: &str = "---\ndescription: fix something\n---\nFix $1 in $2.";
const COMMIT: &str = "---\ndescription: write a commit\n---\nCommit: $ARGUMENTS";

/// The thing that could not be done before: the person who wrote
/// `.basis/templates/fix.md` types its name at a shell.
#[test]
fn a_slash_name_reaches_the_task_as_the_prompt_it_stands_for() {
    let fixture = Fixture::with(&[("fix.md", FIX)]);

    assert_eq!(
        fixture.recorded(&["spawn", "/fix auth login.rs", "--resumable"]),
        "Fix auth in login.rs."
    );

    // And the shorthand is the same command, so it resolves the same way.
    assert_eq!(
        fixture.recorded(&["/fix parsing lexer.rs", "--resumable"]),
        "Fix parsing in lexer.rs."
    );
}

/// `.basis/templates/git/commit.md` is `git:commit` — the same name ACP puts
/// on the wire, because both come from `basis::templates::load`.
#[test]
fn a_namespaced_template_keeps_the_name_acp_already_uses() {
    let fixture = Fixture::with(&[("git/commit.md", COMMIT)]);

    assert_eq!(
        fixture.recorded(&["/git:commit the parser fix", "--resumable"]),
        "Commit: the parser fix"
    );
}

/// A typo handed to the model as prose is a run that answers the wrong
/// question and bills for it. Exit 2, the names that do exist, and the one
/// character that sends a literal `/`.
#[test]
fn an_unknown_name_is_refused_with_the_names_that_exist() {
    let fixture = Fixture::with(&[("fix.md", FIX), ("git/commit.md", COMMIT)]);

    let output = fixture.basis(&["spawn", "/comit the parser fix", "--resumable"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("no template named `comit`"), "{stderr}");
    assert!(
        stderr.contains("fix") && stderr.contains("git:commit"),
        "{stderr}"
    );
    assert!(stderr.contains("stdin"), "{stderr}");
    assert!(stderr.contains("basis spawn -"), "{stderr}");
}

/// A template name never contains `/`, so a first token that does is a path.
/// This is what lets the rule apply to every prompt without an escape for the
/// ordinary case.
#[test]
fn a_prompt_that_opens_with_a_path_is_left_exactly_as_written() {
    let fixture = Fixture::with(&[("fix.md", FIX)]);

    for prose in ["/usr/bin/x crashes on startup", "/ is the root directory"] {
        assert_eq!(
            fixture.recorded(&["spawn", prose, "--resumable"]),
            prose,
            "{prose} is prose, not a command"
        );
    }
}

/// A workspace that defines no templates still refuses a name-shaped first
/// token, and says where a template would go.
#[test]
fn a_workspace_without_templates_says_where_one_would_live() {
    let fixture = Fixture::with(&[]);

    let output = fixture.basis(&["spawn", "/fix it", "--resumable"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains(".basis/templates"), "{stderr}");
    assert!(
        !Path::new(&fixture.data).join("workspaces").exists(),
        "a refused spawn mints nothing"
    );
}
