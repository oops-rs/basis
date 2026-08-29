//! Running somebody else's program on a deadline, with JSON on its stdin.
//!
//! ADR-0012 gives two seams a subprocess binding — interception
//! ([`crate::hooks`]) and tools ([`crate::tools::declared`]) — and both speak
//! the same IO: one payload in, whatever the program prints out, a deadline
//! over the whole exchange. Since mentra 0.24 that IO is
//! [`mentra::process::BoundedCommand`], and this module is basis's use of it
//! rather than a second copy (ADR-0001, ADR-0005): the spawn, the process-group
//! kill, the capped read and `kill_on_drop` live upstream, in the same code
//! mentra's own shell executor runs on. What basis keeps here is what is
//! basis's to decide — which variables a program is handed, how much of its
//! output is kept, and how a failure is quoted back.
//!
//! # The environment is what basis passes
//!
//! The primitive clears the child's environment before it sets the pairs it was
//! given, `PATH` included (mentra `process/bounded.rs`, `spawn`). So nothing
//! this process is holding — a provider key read for the run, a proxy setting,
//! a token the host exported — reaches a hook or a declared tool unless basis
//! lists it. What basis lists is [`baseline_environment`]: the handful of
//! variables that make a program *runnable* at all, each named with its reason,
//! and never a credential. A hook gets the baseline and nothing else; a
//! declared tool gets the baseline, the runtime's fixed command environment and
//! its manifest's `env`, which is where a credential is meant to arrive.
//!
//! This is hygiene, not confinement (ADR-0013). A hook can still read
//! `~/.netrc`, because `HOME` is in the baseline and the process holds the
//! account's authority; what stops is the *ambient* credential — the one that
//! arrives by being in the parent's environment without anyone deciding it
//! should.
//!
//! # What ends a run
//!
//! The budget covers spawning, running and reading, and at the deadline the
//! whole process group goes — a hook whose last line is `sleep 60 &` no longer
//! leaves that sleep behind, which is what the old `std::process` runner could
//! not do and said so. A [`Completion::TimedOut`] carries whatever arrived
//! before the kill; both bindings discard it, because a half-written answer is
//! not an answer and a partial tool result would read as a whole one.

use std::{ffi::OsString, io, path::Path, time::Duration};

pub(crate) use mentra::process::{BoundedCommand, CapturedStream, Completion};

/// How much of a program's stdout is kept.
///
/// Well past anything a hook decision or a tool result should be, and bounded
/// because the primitive insists on a bound: a program that prints without
/// end costs the reader this much and no more. A declared tool's result is
/// bounded again by mentra's `ToolOutputLimiter` on its way to the model,
/// which is the cap that decides what is *read*; this one decides what is
/// *held*.
pub(crate) const OUTPUT_CAPTURE_LIMIT: usize = 8 * 1024 * 1024;

/// How much of a program's stderr is kept for the failure message.
///
/// Enough for a stack trace's first frames, bounded because the text ends up
/// in a denial or a tool error the model reads.
pub(crate) const STDERR_CAPTURE_LIMIT: usize = 2048;

/// How much of a program's stdout is quoted back when it was not a decision.
///
/// Shorter than stderr: the point is to let someone recognize what their hook
/// printed, not to reproduce it.
const OUTPUT_QUOTE_LIMIT: usize = 512;

/// The variables every program basis spawns is handed, with the reason each
/// is there.
///
/// The test of membership is *runnable without it*: a variable is in the
/// baseline when a well-behaved program fails or misbehaves in its absence,
/// and out of it when the only thing it carries is context — and every
/// credential is context. Values are read from this process at spawn time,
/// and a name this process does not have is simply not passed.
#[cfg(not(windows))]
const BASELINE: &[(&str, &str)] = &[
    ("PATH", "how a bare program name is found at all"),
    (
        "HOME",
        "where a program's own configuration lives; git, ssh and most language \
         runtimes fail without it",
    ),
    ("TMPDIR", "where mktemp and the like put scratch files"),
    ("TMP", "the same, for programs that read the other name"),
    ("TEMP", "the same, for programs that read the third"),
    (
        "LANG",
        "how a program encodes the text it reads and prints; python3 picks its \
         stdio encoding from it",
    ),
    (
        "LC_ALL",
        "the override for LANG, kept so the two stay consistent",
    ),
];

/// The Windows baseline: what mentra's own tests keep to run `cmd.exe` at all.
#[cfg(windows)]
const BASELINE: &[(&str, &str)] = &[
    ("PATH", "how a bare program name is found at all"),
    ("PATHEXT", "which extensions a bare name may resolve to"),
    (
        "SystemRoot",
        "where the system's own DLLs are; nothing loads without it",
    ),
    ("COMSPEC", "which cmd.exe a script that shells out gets"),
    ("TEMP", "where scratch files go"),
    ("TMP", "the same, for programs that read the other name"),
];

/// The baseline, as read from this process right now.
pub(crate) fn baseline_environment() -> Vec<(OsString, OsString)> {
    BASELINE
        .iter()
        .filter_map(|(name, _reason)| {
            std::env::var_os(name).map(|value| (OsString::from(name), value))
        })
        .collect()
}

/// Whether `name` is one the baseline may carry — what a test asserting that
/// nothing else arrived checks each name against.
#[cfg(test)]
pub(crate) fn is_baseline(name: &str) -> bool {
    BASELINE.iter().any(|(candidate, _)| *candidate == name)
}

/// The command both bindings run: `command` as argv in `working_dir`, handed
/// the baseline plus `env`, `payload` on stdin, killed at `timeout`.
///
/// `env` is layered over the baseline and wins for a name they share, so a
/// manifest that sets `PATH` gets the `PATH` it asked for. Nothing here ever
/// prints a value.
///
/// A relative program with a directory part is resolved against
/// `working_dir` by the primitive, so `./.basis/hooks/guard.sh` means what the
/// file says regardless of where basis was started; a bare name is left to
/// `PATH`.
pub(crate) fn bounded(
    command: &[String],
    working_dir: &Path,
    env: impl IntoIterator<Item = (String, String)>,
    payload: &str,
    timeout: Duration,
) -> io::Result<BoundedCommand> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| io::Error::other("no command to run"))?;

    let overrides: Vec<(OsString, OsString)> = env
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect();
    let baseline = baseline_environment()
        .into_iter()
        .filter(|(name, _)| !overrides.iter().any(|(declared, _)| declared == name));

    Ok(BoundedCommand::new(program, timeout, OUTPUT_CAPTURE_LIMIT)
        .args(args)
        .current_dir(working_dir)
        .envs(baseline)
        .envs(overrides)
        .stdin(payload))
}

/// Runs `command` as [`bounded`] describes and waits for it.
///
/// Returns `Err` only when the process could not be started or supervised at
/// all. A program that ran and misbehaved is a [`Completion`], because the
/// caller decides what misbehavior means.
pub(crate) async fn execute(
    command: &[String],
    working_dir: &Path,
    env: impl IntoIterator<Item = (String, String)>,
    payload: &str,
    timeout: Duration,
) -> io::Result<Completion> {
    bounded(command, working_dir, env, payload, timeout)?
        .run()
        .await
}

/// A program's stdout as the text both bindings read.
///
/// Lossy rather than fallible: a program that prints one bad byte has still
/// said something, and a cap can land mid-codepoint by construction.
pub(crate) fn stdout_text(stream: &CapturedStream) -> String {
    stream.to_string_lossy().into_owned()
}

/// A program's stderr as the text a failure message quotes, cut to
/// [`STDERR_CAPTURE_LIMIT`].
pub(crate) fn stderr_text(stream: &CapturedStream) -> String {
    truncate(&stream.to_string_lossy(), STDERR_CAPTURE_LIMIT)
}

/// Quotes a program's stdout for a failure message.
pub(crate) fn truncated_output(stdout: &str) -> String {
    truncate(stdout.trim(), OUTPUT_QUOTE_LIMIT)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }

    let cut = (0..=limit)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(0);

    format!("{}… ({} bytes total)", &text[..cut], text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_baseline_variable_this_process_has_is_passed_and_nothing_else_is() {
        let passed = baseline_environment();

        for (name, _) in &passed {
            let name = name.to_str().expect("baseline names are ascii");
            assert!(is_baseline(name), "{name} is not a baseline variable");
            assert_eq!(
                std::env::var_os(name).as_deref(),
                passed
                    .iter()
                    .find(|(candidate, _)| candidate == name)
                    .map(|(_, value)| value.as_os_str()),
                "{name} is passed with this process's own value"
            );
        }
        assert!(
            is_baseline("PATH"),
            "PATH is the one variable no platform's baseline can omit"
        );
        assert!(
            !is_baseline("BASIS_API_KEY"),
            "a credential is never baseline"
        );
    }

    #[test]
    fn long_stderr_is_cut_on_a_character_boundary() {
        let text = "é".repeat(100);

        let cut = truncate(&text, 15);

        assert!(cut.starts_with("ééééééé"));
        assert!(cut.contains("200 bytes total"));
    }

    #[test]
    fn an_empty_command_cannot_be_built() {
        let Err(error) = bounded(&[], Path::new("."), [], "", Duration::from_secs(1)) else {
            panic!("nothing to run");
        };

        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    // Gated to unix: these spawn `/bin/sh` scripts, which is the cheapest way
    // to exercise a real subprocess. The code under test is portable; the
    // fixtures are not, and inventing a Windows shell script per case would
    // test the fixture rather than the runner.
    #[cfg(unix)]
    mod unix {
        use super::*;

        fn sh(script: &str) -> Vec<String> {
            vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
        }

        async fn run(script: &str, payload: &str, timeout: Duration) -> Completion {
            execute(&sh(script), Path::new("."), [], payload, timeout)
                .await
                .expect("the process is supervised")
        }

        /// Whether `pid` is gone, waiting briefly for the kernel to reap it.
        async fn wait_until_dead(pid: i32) -> bool {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                // `kill -0` asks the question without sending anything, and
                // costs no `libc` dependency for one test.
                let alive = std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if !alive {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        #[tokio::test]
        async fn stdin_reaches_the_program_and_stdout_comes_back() {
            let completion = run("cat", "hello", Duration::from_secs(5)).await;

            assert_eq!(completion.code(), Some(0), "{completion:?}");
            assert_eq!(stdout_text(completion.stdout()), "hello");
            assert!(completion.stderr().is_empty());
        }

        #[tokio::test]
        async fn an_exit_code_and_stderr_survive() {
            let completion = run("echo trouble >&2; exit 3", "", Duration::from_secs(5)).await;

            assert_eq!(completion.code(), Some(3), "{completion:?}");
            assert!(completion.stdout().is_empty());
            assert_eq!(stderr_text(completion.stderr()), "trouble\n");
        }

        #[tokio::test]
        async fn a_hanging_program_is_killed_at_the_deadline() {
            let started = std::time::Instant::now();

            let completion = run("sleep 30", "", Duration::from_millis(150)).await;

            assert!(completion.timed_out(), "{completion:?}");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the deadline, not the program, decides how long this takes"
            );
        }

        #[tokio::test]
        async fn a_backgrounded_descendant_dies_with_the_process_group() {
            // What the old runner could not do: the shell backgrounds work and
            // then outlives its budget; both go at the deadline, not just the
            // shell.
            let completion = run(
                "sleep 60 & echo $!; sleep 60",
                "",
                Duration::from_millis(200),
            )
            .await;

            assert!(completion.timed_out(), "{completion:?}");
            let pid: i32 = stdout_text(completion.stdout())
                .trim()
                .parse()
                .expect("the backgrounded pid was printed");
            assert!(
                wait_until_dead(pid).await,
                "the backgrounded descendant {pid} outlived the deadline"
            );
        }

        #[tokio::test]
        async fn stdout_is_capped_while_it_is_read() {
            // Far past the cap, as fast as the program can print it: the bound
            // holds during the read, so the program is never blocked on a full
            // pipe and the reader never holds more than the cap.
            let completion = run(
                "head -c 20000000 /dev/zero | tr '\\0' x",
                "",
                Duration::from_secs(30),
            )
            .await;

            assert_eq!(completion.code(), Some(0), "{completion:?}");
            assert!(completion.stdout().truncated());
            assert!(completion.stdout().len() <= OUTPUT_CAPTURE_LIMIT);
        }

        #[tokio::test]
        async fn a_supplied_variable_wins_over_the_baseline_for_its_name() {
            // How a declared tool's credential gets to the program that needs
            // it, and how a manifest that sets `PATH` gets the one it asked for.
            let completion = execute(
                &sh("printf %s \"$BASIS_TEST_TOKEN:$TMPDIR\""),
                Path::new("."),
                [
                    (
                        "BASIS_TEST_TOKEN".to_string(),
                        "from-the-caller".to_string(),
                    ),
                    ("TMPDIR".to_string(), "/nowhere".to_string()),
                ],
                "",
                Duration::from_secs(5),
            )
            .await
            .expect("the process is supervised");

            assert_eq!(stdout_text(completion.stdout()), "from-the-caller:/nowhere");
        }

        #[tokio::test]
        async fn a_program_that_does_not_exist_is_an_error_not_a_verdict() {
            let error = execute(
                &["/definitely/not/a/real/program".to_string()],
                Path::new("."),
                [],
                "",
                Duration::from_secs(1),
            )
            .await
            .expect_err("cannot be started");

            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        }

        #[tokio::test]
        async fn a_relative_program_is_found_next_to_the_workspace() {
            // The primitive resolves a path with a directory part against the
            // working directory; this pins that basis still hands it one.
            let workspace = tempfile::tempdir().expect("tempdir");
            let script = workspace.path().join("hooks").join("answer.sh");
            std::fs::create_dir_all(script.parent().expect("has a parent")).expect("mkdir");
            std::fs::write(&script, "#!/bin/sh\nprintf found\n").expect("written");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");

            let completion = execute(
                &["./hooks/answer.sh".to_string()],
                workspace.path(),
                [],
                "",
                Duration::from_secs(5),
            )
            .await
            .expect("the process is supervised");

            assert_eq!(stdout_text(completion.stdout()), "found");
        }
    }
}
