//! The `lan` binary: a thin shell over [`lan_core`] and [`lan_acp`].
//!
//! Per ADR-0003 the library is the product and this is a wrapper; per ADR-0011
//! there are two of them, and the CLI is what sits over both. The grammar is
//! ADR-0015 and ADR-0017's:
//!
//! ```text
//! lan "<prompt>"            -> shorthand: exactly `lan spawn "<prompt>"`
//! lan spawn "<prompt>"      -> enqueue one task and print its durable handle
//! lan send <ID> "<message>" -> enqueue a later turn for a running task
//! lan ask <ID> "<question>"  -> enqueue a turn and await its reply
//! lan wait <ID>              -> observe a repeatable terminal result
//! lan wait <ID> --message <MID>
//!                              -> retry one correlated message reply
//! lan cancel <ID>            -> request downward cancellation
//! lan watch <ID>             -> follow progress without owning completion
//! lan inbox [ID]             -> inspect accepted messages
//! lan serve --acp           -> ACP server on stdio, for an editor to spawn
//! lan serve --bridge        -> the same ACP server on a websocket, for a browser
//! lan fingerprint           -> the workspace's hash, for a caller's own loop
//! ```
//!
//! A positional argument that is not a subcommand is a prompt,
//! so the human path carries no ceremony and the editor path is untouched. `--`
//! escapes a prompt that collides with a subcommand name: `lan -- run`.
//!
//! # Where the parts are
//!
//! What is left in this file is dispatch. Everything else is split by the
//! question it answers:
//!
//! - [`shorthand`] rewrites the command line before clap sees it, which is the
//!   only place the shorthand grammar is not simply declared.
//! - [`cli`] declares the rest of it: every type clap parses into.
//! - [`local`] owns the durable lifecycle adapter; [`serve`] owns ACP and its
//!   websocket bridge; [`fingerprint`] owns workspace hashing.
//! - [`exit`] is the table below, and the one function that reads it.
//!
//! The error handling here is the seam between two conventions: a command that
//! returns `Result` says what went wrong and lets this function print it with
//! lan's prefix and exit [`EXIT_FAILED`](exit::EXIT_FAILED), while the two
//! servers report and choose their own code, because "the port was taken" is
//! not the same kind of failure as "the run did not finish".
//!
//! # Exit codes
//!
//! These are contract (ADR-0015): a script branches on them without parsing
//! anything. `--json` remains the structured detail.
//!
//! | Code | Meaning |
//! |---|---|
//! | [`EXIT_OK`](exit::EXIT_OK) | the run finished |
//! | [`EXIT_FAILED`](exit::EXIT_FAILED) | the run failed, or lan could not start it |
//! | [`EXIT_USAGE`](exit::EXIT_USAGE) | the invocation was wrong |
//! | [`EXIT_BOUNDED`](exit::EXIT_BOUNDED) | a bound tripped: `--deadline`, `--tool-budget`, or `--token-budget` |
//!
//! `--token-budget` is the one that can land on a run which *answered*: it ends
//! the run gracefully at a round boundary, so the prose is real and reaches
//! stdout, and the code is what says the allowance rather than the model
//! decided there would be no more of it.

mod approver;
mod bridge;
mod cli;
mod duration_arg;
mod exit;
mod fingerprint;
mod local;
mod route;
mod run;
mod serve;
mod shorthand;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use crate::{
    cli::{Cli, Command},
    exit::{EXIT_FAILED, EXIT_USAGE},
    fingerprint::execute_fingerprint,
    local::ClientError,
    route::{Route, route},
    run::execute_run,
    serve::{serve_acp, serve_bridge},
    shorthand::normalize,
};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse_from(normalize(std::env::args_os())) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            eprint!("{error}");
            if code == i32::from(EXIT_USAGE) {
                eprintln!("next: use `lan spawn <PROMPT>` for work or `lan serve --acp` for ACP");
            }
            return ExitCode::from(code as u8);
        }
    };

    match cli.command {
        Some(Command::Spawn(args)) => match route(&args, local::has_current_task()) {
            Route::Attended => match execute_run(args).await {
                Ok(code) => code,
                Err(message) => {
                    eprintln!("lan: {message}");
                    eprintln!("next: retry with `lan spawn <PROMPT>` or use `lan --help`");
                    ExitCode::from(EXIT_FAILED)
                }
            },
            route => {
                let structured = args.json;
                match local::spawn(args, route == Route::Attach).await {
                    Ok(code) => code,
                    Err(error) => error.render(structured, "lan spawn <PROMPT>"),
                }
            }
        },
        Some(Command::Send(args)) => {
            let structured = args.json;
            lifecycle_result(
                local::send(args).await,
                "lan send <ID> <MESSAGE>",
                structured,
            )
        }
        Some(Command::Ask(args)) => {
            let structured = args.json;
            lifecycle_result(local::ask(args).await, "lan ask <ID> <MESSAGE>", structured)
        }
        Some(Command::Wait(args)) => {
            let structured = args.json;
            lifecycle_result(local::wait(args).await, "lan wait <ID>", structured)
        }
        Some(Command::Cancel(args)) => {
            let structured = args.json;
            lifecycle_result(local::cancel(args).await, "lan cancel <ID>", structured)
        }
        Some(Command::Watch(args)) => {
            let structured = args.json;
            lifecycle_result(local::watch(args).await, "lan watch <ID>", structured)
        }
        Some(Command::Inbox(args)) => {
            let structured = args.json;
            lifecycle_result(local::inbox(args).await, "lan inbox <ID>", structured)
        }
        Some(Command::Fingerprint(args)) => match execute_fingerprint(args) {
            Ok(code) => code,
            Err(message) => {
                eprintln!("lan: {message}");
                eprintln!(
                    "next: retry with `lan fingerprint -C <DIR>` after checking the workspace path"
                );
                ExitCode::from(EXIT_FAILED)
            }
        },
        Some(Command::Serve(args)) if args.acp && args.has_bridge_options() => {
            eprintln!("lan: bridge options cannot be used with `lan serve --acp`");
            eprintln!("next: remove the bridge flags or use `lan serve --bridge`");
            ExitCode::from(EXIT_USAGE)
        }
        Some(Command::Serve(args)) if args.acp => serve_acp(args.acp_args).await,
        Some(Command::Serve(args)) => serve_bridge(args.acp_args, args.bridge_args).await,
        None => usage(),
    }
}

fn lifecycle_result(
    result: Result<ExitCode, ClientError>,
    command: &str,
    structured: bool,
) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => error.render(structured, command),
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "lan: a prompt or command is required; try `lan serve --acp` or `lan spawn <PROMPT>`"
    );
    eprintln!("{}", Cli::command().render_usage());
    eprintln!(
        "next: use `lan spawn <PROMPT>` for work, `lan fingerprint` for the workspace hash, or `lan serve --acp` for ACP"
    );
    ExitCode::from(EXIT_USAGE)
}
