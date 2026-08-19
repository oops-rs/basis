//! Running one hook process, with a deadline.
//!
//! A hook is somebody else's program, so the two things that matter here are
//! that it cannot hang the turn and that whatever it did say survives being
//! killed. Everything is `std::process`: the interception point mentra offers
//! is a synchronous trait method, so there is no async process to await.
//!
//! # The deadline covers reading, not just waiting
//!
//! Killing a hook does not kill what the hook started, and a grandchild
//! inherits the pipes. So a script whose last line is `sleep 60` leaves the
//! read end open long after the shell is gone, and `read_to_end` would sit
//! there for the full minute — the timeout would have killed the process and
//! still lost the turn. Output is therefore collected over a channel with the
//! same deadline, and the reader threads are detached rather than joined: they
//! end when the pipes finally close, and nothing waits for that.
//!
//! What basis does *not* do is kill the hook's descendants. A hook that
//! backgrounds work leaves that work running; bounding it is the job of
//! whatever confines the process (ADR-0004), the same as for any other command.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// How much of a hook's stderr is kept for the failure message.
///
/// Enough for a stack trace's first frames, bounded because the text ends up
/// in a denial the model reads.
const STDERR_CAPTURE_LIMIT: usize = 2048;

/// How much of a hook's stdout is quoted back when it was not a decision.
///
/// Shorter than stderr: the point is to let someone recognize what their hook
/// printed, not to reproduce it.
const OUTPUT_QUOTE_LIMIT: usize = 512;

/// Quotes a hook's stdout for a failure message.
pub(super) fn truncated_output(stdout: &str) -> String {
    truncate(stdout.trim(), OUTPUT_QUOTE_LIMIT)
}

/// The longest basis waits between checks on a running hook. The poll starts far
/// tighter, so the common case — a script that answers in milliseconds — is not
/// made slow by the ceiling that keeps a slow one cheap.
const MAX_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a hook that exited on time still gets for its pipes to drain.
///
/// A hook that answers with a millisecond to spare should not be failed for the
/// scheduling latency between its exit and its output arriving.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// How a hook process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Completion {
    Exited {
        /// `None` when a signal ended it, which is a failure like any other.
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    /// Killed for exceeding its budget. Anything it had printed is discarded:
    /// a half-written answer is not an answer.
    TimedOut,
}

/// Runs `command` in `working_dir`, feeding it `payload` on stdin.
///
/// Returns `Err` only when the process could not be started or supervised at
/// all. A hook that ran and misbehaved is a [`Completion`], because the caller
/// decides what misbehavior means.
pub(super) fn execute(
    command: &[String],
    working_dir: &Path,
    payload: &str,
    timeout: Duration,
) -> io::Result<Completion> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| io::Error::other("hook has no command to run"))?;

    // Started before the spawn, because forking a process is part of what the
    // hook is being given time for.
    let deadline = Instant::now() + timeout;

    let mut child = Command::new(resolve_program(program, working_dir))
        .args(args)
        .current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Every pipe gets its own thread. Writing inline would deadlock on a hook
    // that answers without reading its stdin once the payload outgrows the
    // pipe buffer, and reading inline would deadlock the mirror image. None of
    // the three is ever joined — see the module docs: a descendant holding a
    // pipe open would turn a join into exactly the wait the deadline exists to
    // prevent.
    let mut stdin = child.stdin.take().ok_or_else(|| pipe_missing("stdin"))?;
    let mut stdout = child.stdout.take().ok_or_else(|| pipe_missing("stdout"))?;
    let mut stderr = child.stderr.take().ok_or_else(|| pipe_missing("stderr"))?;

    let owned_payload = payload.to_string();
    thread::spawn(move || {
        // A broken pipe here is not an error: `echo '{"decision":"allow"}'` is
        // a legitimate hook, and it exits without ever reading. What the hook
        // printed and how it exited answer the question; this only offers.
        let _ = stdin.write_all(owned_payload.as_bytes());
        let _ = stdin.flush();
    });

    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn(move || out_tx.send(read_all(&mut stdout)));
    thread::spawn(move || err_tx.send(read_all(&mut stderr)));

    let code = match supervise(&mut child, deadline)? {
        Supervised::Exited(code) => code,
        Supervised::Killed => return Ok(Completion::TimedOut),
    };

    // The child is gone, so the pipes should be closing; the grace is for the
    // moment that takes, and the deadline still caps a descendant holding on.
    let drain = (Instant::now() + DRAIN_GRACE).max(deadline);
    let (Some(stdout), Some(stderr)) = (collect(&out_rx, drain)?, collect(&err_rx, drain)?) else {
        // Output that never arrived is the same failure as a hook that never
        // finished: its process tree outlived the budget.
        return Ok(Completion::TimedOut);
    };

    Ok(Completion::Exited {
        code,
        stdout,
        stderr: truncate(&stderr, STDERR_CAPTURE_LIMIT),
    })
}

enum Supervised {
    Exited(Option<i32>),
    Killed,
}

/// Waits for the child, killing it at the deadline.
///
/// Polling rather than blocking on `wait()`: killing needs the handle that
/// `wait()` borrows, and the alternatives are a signal-handling dance or a
/// dependency, neither of which buys anything at this timescale.
fn supervise(child: &mut std::process::Child, deadline: Instant) -> io::Result<Supervised> {
    let mut interval = Duration::from_millis(1);

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Supervised::Exited(status.code()));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // The whole reason a hook has a budget: a hanging one must cost the
            // turn its timeout, not the turn itself.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Supervised::Killed);
        }

        thread::sleep(interval.min(remaining));
        interval = (interval * 2).min(MAX_POLL_INTERVAL);
    }
}

/// Where a relative hook program lives.
///
/// A path — anything with a directory part — is relative to the workspace, so
/// `./.basis/hooks/guard.sh` means what the file says it means regardless of
/// where basis was started. A bare name is left alone for `PATH` to answer, which
/// is what someone writing `python3` expects.
fn resolve_program(program: &str, working_dir: &Path) -> PathBuf {
    let path = Path::new(program);
    let has_directory = path
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty());

    if path.is_absolute() || !has_directory {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

fn read_all(source: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    source.read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// Takes a stream's contents, or `None` if it has not arrived by `deadline`.
///
/// A reader thread that vanished without sending is treated the same as one
/// still blocked: nothing to report, and nothing to wait for.
fn collect(
    stream: &mpsc::Receiver<io::Result<Vec<u8>>>,
    deadline: Instant,
) -> io::Result<Option<String>> {
    let remaining = deadline.saturating_duration_since(Instant::now());

    match stream.recv_timeout(remaining) {
        // Lossy rather than an error: a hook that prints one bad byte on
        // stderr should still be able to have its stdout read.
        Ok(Ok(bytes)) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(None),
    }
}

fn pipe_missing(stream: &str) -> io::Error {
    io::Error::other(format!("hook {stream} pipe was not created"))
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

// Gated to unix: these spawn `/bin/sh` scripts, which is the cheapest way
// to exercise a real subprocess. The code under test is portable; the
// fixtures are not, and inventing a Windows shell script per case would test
// the fixture rather than the runner.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
    }

    fn run(script: &str, payload: &str, timeout: Duration) -> Completion {
        execute(&sh(script), Path::new("."), payload, timeout).expect("the process is supervised")
    }

    #[test]
    fn stdin_reaches_the_hook_and_stdout_comes_back() {
        let completion = run("cat", "hello", Duration::from_secs(5));

        assert_eq!(
            completion,
            Completion::Exited {
                code: Some(0),
                stdout: "hello".to_string(),
                stderr: String::new(),
            }
        );
    }

    #[test]
    fn a_hook_that_never_reads_stdin_still_answers() {
        // The deadlock this guards against needs a payload larger than the pipe
        // buffer; 256 KiB is comfortably past every platform's.
        let payload = "x".repeat(256 * 1024);

        let completion = run("echo done", &payload, Duration::from_secs(5));

        assert_eq!(
            completion,
            Completion::Exited {
                code: Some(0),
                stdout: "done\n".to_string(),
                stderr: String::new(),
            }
        );
    }

    #[test]
    fn an_exit_code_and_stderr_survive() {
        let completion = run("echo trouble >&2; exit 3", "", Duration::from_secs(5));

        match completion {
            Completion::Exited {
                code,
                stdout,
                stderr,
            } => {
                assert_eq!(code, Some(3));
                assert!(stdout.is_empty());
                assert_eq!(stderr, "trouble\n");
            }
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[test]
    fn a_hanging_hook_is_killed_at_the_deadline() {
        let started = Instant::now();

        let completion = run("sleep 30", "", Duration::from_millis(150));

        assert_eq!(completion, Completion::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline, not the hook, decides how long this takes"
        );
    }

    #[test]
    fn a_descendant_holding_the_pipe_cannot_outlast_the_deadline() {
        // The hook answers and exits immediately, but leaves a child holding
        // the stdout pipe open. Reading to EOF would wait for that child; the
        // budget has to cover reading as well as waiting.
        let started = Instant::now();

        let completion = run(
            r#"sleep 30 & echo '{"decision":"allow"}'"#,
            "",
            Duration::from_millis(300),
        );

        assert_eq!(
            completion,
            Completion::TimedOut,
            "an answer that will not arrive is not an answer"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_program_that_does_not_exist_is_an_error_not_a_verdict() {
        let error = execute(
            &["/definitely/not/a/real/program".to_string()],
            Path::new("."),
            "",
            Duration::from_secs(1),
        )
        .expect_err("cannot be started");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_relative_program_is_found_next_to_the_workspace() {
        let workspace = Path::new("/repo");

        assert_eq!(
            resolve_program("./hooks/guard.sh", workspace),
            PathBuf::from("/repo/./hooks/guard.sh")
        );
        assert_eq!(
            resolve_program("/bin/sh", workspace),
            PathBuf::from("/bin/sh")
        );
        assert_eq!(
            resolve_program("python3", workspace),
            PathBuf::from("python3"),
            "a bare name belongs to PATH"
        );
    }

    #[test]
    fn long_stderr_is_cut_on_a_character_boundary() {
        let text = "é".repeat(100);

        let cut = truncate(&text, 15);

        assert!(cut.starts_with("ééééééé"));
        assert!(cut.contains("200 bytes total"));
    }
}
