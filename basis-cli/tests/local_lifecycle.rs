//! The lifecycle contract, driven through the real binary: durable handles,
//! repeatable terminal waits, and actionable hints — with no daemon to start,
//! because there is none (ADR-0019).

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

const NOT_STUCK: Duration = Duration::from_secs(15);

#[test]
fn bare_usage_ends_with_an_actionable_hint() {
    let output = run_bounded(&mut Command::new(env!("CARGO_BIN_EXE_basis")));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let stderr = stderr(&output);
    assert!(
        stderr
            .lines()
            .last()
            .is_some_and(|line| line.starts_with("next:")),
        "the final line should tell an agent what it can do next:\n{stderr}"
    );
}

/// The upgrade path stated as a test: a workspace whose store directory still
/// holds basis ≤0.6's `runtime.sqlite` is refused with basis words on stderr —
/// what happened, that nothing is migrated (ADR-0023), and that
/// `BASIS_DATA_DIR` is the way to start fresh — never an empty file store
/// beside a database the operator would read as their history vanishing.
#[test]
fn a_pre_07_store_is_refused_on_stderr_in_basis_words() {
    let root = tempfile::tempdir().expect("tempdir");
    let workspace = root.path().join("workspace");
    let data = root.path().join("data");
    fs::create_dir_all(&workspace).expect("workspace");

    // First run: resolves (and creates) this workspace's per-key store
    // directory before the bogus provider name stops anything else from
    // happening. `basis_tasks::Tasks::store_dir` is the CLI's own derivation
    // but reads BASIS_DATA_DIR from *this* process, which a test must not
    // set, so the directory is read back from the data root instead — see
    // `sole_workspace_key` for what makes that deterministic.
    let resolved = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_basis"))
            .env("BASIS_DATA_DIR", &data)
            .env_remove("BASIS_TASK_ID")
            .args(["spawn", "irrelevant", "--json", "-C"])
            .arg(&workspace)
            .args(["--provider", "not-a-provider"]),
    );
    assert!(
        !resolved.status.success(),
        "a bogus provider must not run: {}",
        stderr(&resolved)
    );

    // What every pre-0.7 basis left in that directory. The `store/` subdir is
    // created here because 0.6 created it on first write, which is exactly
    // the state an upgrade finds.
    let store = sole_workspace_key(&data).join("store");
    fs::create_dir_all(&store).expect("the old store directory");
    fs::write(store.join("runtime.sqlite"), b"SQLite format 3\0").expect("plant the old database");

    // Second run: keyless custom endpoint, so nothing needs a credential and
    // nothing reaches a network — the refusal happens at workspace open.
    let refused = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_basis"))
            .env("BASIS_DATA_DIR", &data)
            .env_remove("BASIS_TASK_ID")
            .args(["spawn", "irrelevant", "--json", "-C"])
            .arg(&workspace)
            .args(["--base-url", "http://127.0.0.1:9/v1"]),
    );
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    let message = stderr(&refused);
    assert!(message.contains("basis 0.6 or earlier"), "{message}");
    assert!(message.contains("not migrated"), "{message}");
    assert!(message.contains("BASIS_DATA_DIR"), "{message}");
}

/// The one per-workspace directory under a data root this fixture has run
/// exactly one workspace against.
///
/// The key is a hash of the workspace path that `basis-tasks` derives and
/// does not expose without reading `BASIS_DATA_DIR` from the calling process,
/// which a test sharing a process with other tests must not set. So the
/// directory is read back — and the reading is made deterministic by saying
/// what the fixture guarantees and failing by name when it stops being true,
/// rather than by taking whichever entry happens to come first.
fn sole_workspace_key(data: &Path) -> PathBuf {
    let workspaces = data.join("workspaces");
    let mut keys: Vec<PathBuf> = fs::read_dir(&workspaces)
        .unwrap_or_else(|error| {
            panic!(
                "the fixture's first run should have created {}: {error}",
                workspaces.display()
            )
        })
        .map(|entry| entry.expect("read a workspace key").path())
        .collect();
    keys.sort();

    assert_eq!(
        keys.len(),
        1,
        "this fixture runs one workspace against a fresh data root, so exactly one key should \
         exist under {}; found {keys:?} — the fixture changed, not the behaviour under test",
        workspaces.display()
    );
    keys.remove(0)
}

#[test]
fn task_handles_survive_clients_and_terminal_waits_are_repeatable() {
    let root = tempfile::tempdir().expect("tempdir");
    let workspace = root.path().join("workspace");
    let data = root.path().join("data");
    fs::create_dir_all(&workspace).expect("workspace");

    // An invalid provider settles without making a network request. The test
    // is about durable state across processes, not provider behavior.
    let spawned = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_basis"))
            .env("BASIS_DATA_DIR", &data)
            .env_remove("BASIS_TASK_ID")
            .args(["spawn", "do not run", "--resumable", "-C"])
            .arg(&workspace)
            .args([
                "--provider",
                "not-a-provider",
                "--deadline",
                "5s",
                "--no-shell",
            ]),
    );
    assert!(spawned.status.success(), "{}", stderr(&spawned));
    let hints = stderr(&spawned);
    let output = String::from_utf8(spawned.stdout).expect("utf8 spawn output");
    assert!(
        output.contains("resumable"),
        "spawn --resumable reports the honest unattached state: {output}"
    );
    // On stderr, where every hint belongs: stdout carries the handle a script
    // reads, and nothing addressed to the person reading the terminal.
    assert!(
        hints.contains("next: use `basis wait "),
        "spawn should teach the next lifecycle action: {hints}"
    );
    assert!(
        !output.contains("next:"),
        "the hint must not land in the output a script parses: {output}"
    );
    let task = output
        .lines()
        .find_map(|line| line.strip_prefix("task "))
        .and_then(|line| line.split_once(':').map(|(task, _)| task.to_string()))
        .unwrap_or_else(|| panic!("spawn did not print a task handle: {output}"));

    let first = wait(&data, &task);
    assert_eq!(first.status.code(), Some(1), "{}", stderr(&first));
    let first: Value = serde_json::from_slice(&first.stdout).expect("first terminal JSON");
    assert_eq!(first["state"], "failed");
    assert_eq!(first["task"], task);
    assert_eq!(
        first["next"], "basis spawn <PROMPT>",
        "a failure names the retry, not a watch and an inbox that hold nothing"
    );

    let second = wait(&data, &task);
    assert_eq!(second.status.code(), Some(1), "{}", stderr(&second));
    let second: Value = serde_json::from_slice(&second.stdout).expect("second terminal JSON");
    assert_eq!(
        second, first,
        "waiting again must not rerun or rewrite work"
    );
}

fn wait(data: &Path, task: &str) -> Output {
    run_bounded(
        Command::new(env!("CARGO_BIN_EXE_basis"))
            .env("BASIS_DATA_DIR", data)
            .env_remove("BASIS_TASK_ID")
            .args(["wait", task, "--timeout", "10s", "--json"]),
    )
}

fn run_bounded(command: &mut Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("start basis command");
    let deadline = Instant::now() + NOT_STUCK;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll basis command") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("basis command did not settle within {NOT_STUCK:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_end(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_end(&mut stderr)
        .expect("read stderr");
    Output {
        status,
        stdout,
        stderr,
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
