use std::{
    fs,
    io::Read,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

const NOT_STUCK: Duration = Duration::from_secs(15);

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn task_handles_survive_clients_and_terminal_waits_are_repeatable() {
    let root = tempfile::tempdir().expect("tempdir");
    let workspace = root.path().join("workspace");
    let config = root.path().join("config");
    let registry = config.join("agents");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&registry).expect("registry");

    let daemon = Command::new(env!("CARGO_BIN_EXE_lan"))
        .args(["__daemon", "--workspace"])
        .arg(&workspace)
        .arg("--registry")
        .arg(&registry)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start daemon");
    let _daemon = ChildGuard(daemon);
    wait_for_descriptor(&registry);

    // An invalid provider settles without making a network request. The test
    // is about process ownership and durable state, not provider behavior.
    let spawned = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_lan"))
            .env("LAN_CONFIG_DIR", &config)
            .args(["spawn", "do not run", "-C"])
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
    let output = String::from_utf8(spawned.stdout).expect("utf8 spawn output");
    assert!(
        output.contains("next: use `lan wait "),
        "spawn should teach the next lifecycle action: {output}"
    );
    let task = output
        .lines()
        .find_map(|line| line.strip_prefix("task "))
        .and_then(|line| line.split_once(':').map(|(task, _)| task.to_string()))
        .unwrap_or_else(|| panic!("spawn did not print a task handle: {output}"));

    let first = wait(&config, &task);
    assert_eq!(first.status.code(), Some(1), "{}", stderr(&first));
    let first: Value = serde_json::from_slice(&first.stdout).expect("first terminal JSON");
    assert_eq!(first["state"], "failed");
    assert_eq!(first["task"], task);
    assert!(first["next"].as_str().is_some());

    let second = wait(&config, &task);
    assert_eq!(second.status.code(), Some(1), "{}", stderr(&second));
    let second: Value = serde_json::from_slice(&second.stdout).expect("second terminal JSON");
    assert_eq!(
        second, first,
        "waiting again must not rerun or rewrite work"
    );
}

fn wait(config: &Path, task: &str) -> Output {
    run_bounded(
        Command::new(env!("CARGO_BIN_EXE_lan"))
            .env("LAN_CONFIG_DIR", config)
            .args(["wait", task, "--timeout", "10s", "--json"]),
    )
}

fn wait_for_descriptor(registry: &Path) {
    let deadline = Instant::now() + NOT_STUCK;
    loop {
        let ready = fs::read_dir(registry)
            .expect("read registry")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("workspace-")
            });
        if ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never published its descriptor"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_bounded(command: &mut Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("start lan command");
    let deadline = Instant::now() + NOT_STUCK;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll lan command") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lan command did not settle within {NOT_STUCK:?}");
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
