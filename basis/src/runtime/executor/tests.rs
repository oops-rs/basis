//! What routing settles, and what it must not change.
//!
//! Every case here runs against stub executors that record the request they
//! were handed, because the two claims worth pinning are both about the
//! request: that the fixed environment is already merged into it by the time
//! anything routes, and that it arrives at the executor its target names and
//! at no other.

use std::{path::PathBuf, sync::Mutex, time::Duration};

use mentra::runtime::CommandSpec;

use super::*;

/// Answers nothing useful and remembers everything it was asked.
struct Recording {
    name: &'static str,
    seen: Arc<Mutex<Vec<CommandRequest>>>,
}

impl Recording {
    fn new(name: &'static str) -> (Self, Arc<Mutex<Vec<CommandRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                name,
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

#[async_trait]
impl RuntimeExecutor for Recording {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String> {
        self.seen.lock().expect("not poisoned").push(request);

        Ok(CommandOutput {
            stdout: self.name.to_string(),
            stderr: String::new(),
            success: true,
            status_code: Some(0),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        })
    }
}

fn request(target: Option<&str>, cwd: PathBuf) -> CommandRequest {
    CommandRequest {
        spec: CommandSpec::Shell {
            command: "echo hi".to_string(),
        },
        cwd,
        timeout: Duration::from_secs(1),
        env: vec![("PATH".to_string(), "/bin".to_string())],
        max_output_bytes_per_stream: 1024,
        target: target.map(str::to_string),
    }
}

fn environment() -> BTreeMap<String, String> {
    BTreeMap::from([("BASIS_TASK_ID".to_string(), "task-1".to_string())])
}

#[test]
fn fixed_values_replace_ambient_values_without_duplicates() {
    let mut current = vec![
        ("PATH".to_string(), "/bin".to_string()),
        ("BASIS_TASK_ID".to_string(), "wrong".to_string()),
    ];
    let fixed = BTreeMap::from([
        ("BASIS_DATA_DIR".to_string(), "/tmp/basis".to_string()),
        ("BASIS_TASK_ID".to_string(), "task-1".to_string()),
    ]);

    merge(&mut current, &fixed);

    assert_eq!(
        current,
        vec![
            ("PATH".to_string(), "/bin".to_string()),
            ("BASIS_DATA_DIR".to_string(), "/tmp/basis".to_string()),
            ("BASIS_TASK_ID".to_string(), "task-1".to_string()),
        ]
    );
}

#[tokio::test]
async fn a_named_command_reaches_the_executor_registered_under_that_name() {
    let (mac, mac_seen) = Recording::new("mac");
    let (builder, builder_seen) = Recording::new("builder");
    let executor = TargetedExecutor::new(
        Arc::new(BTreeMap::new()),
        CommandTargets::from([
            ("mac".to_string(), Arc::new(mac) as Arc<dyn RuntimeExecutor>),
            (
                "builder".to_string(),
                Arc::new(builder) as Arc<dyn RuntimeExecutor>,
            ),
        ]),
    );

    let output = executor
        .run(request(Some("mac"), PathBuf::from("/repo")))
        .await
        .expect("routes");

    assert_eq!(output.stdout, "mac", "one name, one executor");
    assert!(
        builder_seen.lock().expect("not poisoned").is_empty(),
        "a target must not be shown a command addressed elsewhere"
    );

    let seen = mac_seen.lock().expect("not poisoned");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].target.as_deref(),
        Some("mac"),
        "the name stays on the request, so an executor registered under two \
         names can still tell which it was called as"
    );
}

#[tokio::test]
async fn the_fixed_environment_is_merged_before_anything_routes() {
    // The claim the module's ordering rests on: what a command carries is a
    // fact about this runtime, not about where it lands. A target that saw a
    // different environment from the local executor would be a second thing to
    // keep in step, and nobody would notice the day they drifted.
    let (mac, seen) = Recording::new("mac");
    let executor = TargetedExecutor::new(
        Arc::new(environment()),
        CommandTargets::from([("mac".to_string(), Arc::new(mac) as Arc<dyn RuntimeExecutor>)]),
    );

    executor
        .run(request(Some("mac"), PathBuf::from("/repo")))
        .await
        .expect("routes");

    let seen = seen.lock().expect("not poisoned");
    assert_eq!(
        seen[0].env,
        vec![
            ("PATH".to_string(), "/bin".to_string()),
            ("BASIS_TASK_ID".to_string(), "task-1".to_string()),
        ],
        "the target receives the runtime's fixed pairs, already merged"
    );
}

#[tokio::test]
async fn an_untargeted_command_never_reaches_a_target() {
    let (mac, seen) = Recording::new("mac");
    let executor = TargetedExecutor::new(
        Arc::new(BTreeMap::new()),
        CommandTargets::from([("mac".to_string(), Arc::new(mac) as Arc<dyn RuntimeExecutor>)]),
    );

    // It runs locally, which here means `echo hi` actually executes — the one
    // thing in this file that leaves the process, and the only way to show the
    // untargeted path still ends at mentra's own executor.
    // A real directory, because this is the one case that actually spawns a
    // process.
    let here = tempfile::tempdir().expect("tempdir");
    let output = executor
        .run(request(None, here.path().to_path_buf()))
        .await
        .expect("runs locally");

    assert_eq!(output.stdout.trim(), "hi");
    assert!(
        seen.lock().expect("not poisoned").is_empty(),
        "registering a target must not change where an untargeted command runs"
    );
}

#[tokio::test]
async fn a_target_nothing_serves_is_an_error_and_never_a_local_run() {
    // The direction this has to fail in: a command a host addressed to a build
    // machine, silently executing here instead, is the failure a target exists
    // to prevent.
    let (mac, _) = Recording::new("mac");
    let executor = TargetedExecutor::new(
        Arc::new(BTreeMap::new()),
        CommandTargets::from([("mac".to_string(), Arc::new(mac) as Arc<dyn RuntimeExecutor>)]),
    );

    let error = executor
        .run(request(Some("linux"), PathBuf::from("/repo")))
        .await
        .expect_err("nothing serves `linux`");

    assert!(error.contains("`linux`"), "{error}");
    assert!(error.contains("`mac`"), "the reader needs the set: {error}");
}

#[tokio::test]
async fn a_runtime_with_no_targets_says_so_rather_than_listing_nothing() {
    let executor = TargetedExecutor::new(Arc::new(environment()), CommandTargets::new());

    let error = executor
        .run(request(Some("mac"), PathBuf::from("/repo")))
        .await
        .expect_err("nothing is registered");

    assert!(error.contains("registers no command targets"), "{error}");
}
