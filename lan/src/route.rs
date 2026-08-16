//! Who drives a spawned agent: the environment × flags matrix (ADR-0020).
//!
//! `lan spawn` has three possible fates, and until ADR-0020 the choice between
//! them fell out of `--json` — a *rendering* flag deciding whether work ran at
//! all. That put the human on the worst cell of the grid: `lan "hi"` minted a
//! checkpoint nothing was driving and printed a handle, while `lan --json "hi"`
//! answered. The machine got the attended run and the person got homework.
//!
//! The rule here is that the **environment** picks the default and **flags**
//! override it, so rendering can go back to meaning rendering:
//!
//! | `LAN_TASK_ID` | flags | route |
//! |---|---|---|
//! | unset | *(none)* | [`Route::Attach`] |
//! | unset | `--json` | [`Route::Attended`] |
//! | unset | `--await` | [`Route::Attach`] |
//! | unset | `--json --await` | [`Route::Attach`] |
//! | unset | `--resumable` | [`Route::Resumable`] |
//! | set | *(none)* | [`Route::Resumable`] |
//! | set | `--json` | [`Route::Resumable`] |
//! | set | `--await` | [`Route::Attach`] |
//! | set | `--resumable` | [`Route::Resumable`] |
//!
//! The environment is the right axis because it answers the only question that
//! matters: *is there someone with nothing better to do than wait?* A shell has
//! exactly that — it is blocked on this process either way. A parent model turn
//! is the opposite: it holds a session another agent may need, so its child
//! must come back as a handle (ADR-0017).
//!
//! `--detached` is deliberately absent from the table. It answers a different
//! question — whether the new agent hangs off the caller or starts its own root
//! — and conflating "no parent" with "nobody driving" is what made it a
//! near-no-op at a shell, where there was no parent to detach from anyway.

use crate::cli::RunArgs;

/// Which of the three spawn paths an invocation takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// Run in this process and render the stream as it arrives. Mints no
    /// checkpoint, so there is no handle and nothing to `send` to later — this
    /// is the path that owns the `run_started`/`run_finished` JSONL contract.
    Attended,
    /// Mint the checkpoint, print its handle, drive nothing. Progress happens
    /// when something attaches.
    Resumable,
    /// Mint the checkpoint and drive it here until it settles. The handle is
    /// durable, so the work survives this process; the answer is printed
    /// because this process stayed for it.
    Attach,
}

/// Reads the matrix above. `in_lan_task` is whether `LAN_TASK_ID` named a task
/// — that is, whether a parent turn is waiting on this process to return.
pub(crate) fn route(args: &RunArgs, in_lan_task: bool) -> Route {
    // A parent's turn is not a spare thread: blocking it is what ADR-0017's
    // wait-for cycles are made of. Inside a task the handle comes back
    // immediately unless the caller took explicit responsibility for waiting.
    if in_lan_task {
        return if args.await_result {
            Route::Attach
        } else {
            Route::Resumable
        };
    }

    if args.resumable {
        return Route::Resumable;
    }

    // `--json` alone keeps the attended JSONL contract it has always had: the
    // first line is `run_started` and the last is `run_finished`. Asking to
    // wait asks for the lifecycle result instead, which is one JSON object.
    if args.json && !args.await_result {
        return Route::Attended;
    }

    Route::Attach
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    use crate::cli::{Cli, Command};

    fn args(flags: &[&str]) -> RunArgs {
        let mut argv = vec!["lan", "spawn", "a prompt"];
        argv.extend_from_slice(flags);
        let cli = Cli::try_parse_from(argv).expect("flags parse");
        let Some(Command::Spawn(args)) = cli.command else {
            panic!("spawn parses");
        };
        args
    }

    /// The module docs' table, asserted cell by cell. A change to the grammar
    /// that is not also a change here is a change nobody decided.
    #[test]
    fn the_matrix_is_what_the_docs_say_it_is() {
        let matrix = [
            (false, vec![], Route::Attach),
            (false, vec!["--json"], Route::Attended),
            (false, vec!["--await"], Route::Attach),
            (false, vec!["--json", "--await"], Route::Attach),
            (false, vec!["--resumable"], Route::Resumable),
            (false, vec!["--resumable", "--json"], Route::Resumable),
            (false, vec!["--detached"], Route::Attach),
            (true, vec![], Route::Resumable),
            (true, vec!["--json"], Route::Resumable),
            (true, vec!["--await"], Route::Attach),
            (true, vec!["--json", "--await"], Route::Attach),
            (true, vec!["--resumable"], Route::Resumable),
            (true, vec!["--detached"], Route::Resumable),
        ];

        for (in_lan_task, flags, expected) in matrix {
            assert_eq!(
                route(&args(&flags), in_lan_task),
                expected,
                "LAN_TASK_ID {}, flags {flags:?}",
                if in_lan_task { "set" } else { "unset" }
            );
        }
    }

    /// The regression this module exists for: at a shell, a bare prompt has to
    /// answer. Printing a handle for work nothing is driving is the bug.
    #[test]
    fn a_bare_prompt_at_a_shell_is_driven_here() {
        assert_eq!(route(&args(&[]), false), Route::Attach);
    }

    /// `--json` picks a renderer. It must not also decide whether the agent
    /// runs, which is what it used to do for every cell of the grid.
    #[test]
    fn json_never_changes_the_lifecycle_it_only_changes_the_rendering() {
        for flags in [vec![], vec!["--await"], vec!["--resumable"]] {
            let mut with_json = flags.clone();
            with_json.push("--json");
            for in_lan_task in [false, true] {
                // The one documented exception: `--json` with nothing else at a
                // shell is the attended JSONL one-shot, which is a different
                // lifecycle by design and predates this module.
                if flags.is_empty() && !in_lan_task {
                    continue;
                }
                assert_eq!(
                    route(&args(&flags), in_lan_task),
                    route(&args(&with_json), in_lan_task),
                    "flags {flags:?} in_lan_task {in_lan_task}"
                );
            }
        }
    }

    /// `--await` and `--resumable` are opposite answers to one question, so
    /// clap rejects the pair rather than letting a precedence rule decide.
    #[test]
    fn waiting_and_not_waiting_cannot_both_be_asked_for() {
        let cli = Cli::try_parse_from(["lan", "spawn", "p", "--await", "--resumable"]);
        assert!(cli.is_err(), "--await --resumable must not parse");
    }
}
