//! The `lan` binary: a thin shell over [`lan_core`] and [`lan_acp`].
//!
//! Per ADR-0003 the library is the product and this is a wrapper; per ADR-0011
//! there are two of them, and the CLI is what sits over both. The grammar is
//! ADR-0015's, and it is five lines:
//!
//! ```text
//! lan "<prompt>"            -> shorthand: exactly `lan spawn "<prompt>"`
//! lan spawn "<prompt>"      -> headless one-shot; `-` reads the prompt from stdin
//! lan serve --acp           -> ACP server on stdio, for an editor to spawn
//! lan serve --bridge        -> the same ACP server on a websocket, for a browser
//! lan fingerprint           -> the workspace's hash, for a caller's own loop
//! ```
//!
//! A positional argument that is not one of the four subcommands is a prompt,
//! so the human path carries no ceremony and the editor path is untouched. `--`
//! escapes a prompt that collides with a subcommand name: `lan -- run`.
//!
//! # Where the parts are
//!
//! What is left in this file is the dispatch, and the two contracts above and
//! below it. Everything else is split by the question it answers, because the
//! four commands share almost nothing but the argv they came from:
//!
//! - [`shorthand`] rewrites the command line before clap sees it, which is the
//!   only place the five-line grammar is not simply declared.
//! - [`cli`] declares the rest of it: every type clap parses into.
//! - [`run`], [`serve`] and [`fingerprint`] are the commands themselves — one
//!   per line of the table, with `acp` and `bridge` sharing [`serve`] since
//!   they are one server on two transports.
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
mod run;
mod serve;
mod shorthand;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use crate::{
    cli::{Cli, Command},
    exit::{EXIT_FAILED, EXIT_USAGE},
    fingerprint::execute_fingerprint,
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
        Some(Command::Spawn(args)) => match execute_run(args).await {
            Ok(code) => code,
            Err(message) => {
                eprintln!("lan: {message}");
                ExitCode::from(EXIT_FAILED)
            }
        },
        Some(Command::Fingerprint(args)) => match execute_fingerprint(args) {
            Ok(code) => code,
            Err(message) => {
                eprintln!("lan: {message}");
                ExitCode::from(EXIT_FAILED)
            }
        },
        Some(Command::Serve(args)) if args.acp && args.has_bridge_options() => {
            eprintln!(
                "lan: bridge options require `lan serve --bridge`; next: remove the bridge flags or select `--bridge`"
            );
            ExitCode::from(EXIT_USAGE)
        }
        Some(Command::Serve(args)) if args.acp => serve_acp(args.acp_args).await,
        Some(Command::Serve(args)) => serve_bridge(args.acp_args, args.bridge_args).await,
        None => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "lan: a prompt or command is required; try `lan serve --acp` or `lan spawn <PROMPT>`"
    );
    eprintln!(
        "next: use `lan spawn <PROMPT>` for work, `lan fingerprint` for the workspace hash, or `lan serve --acp` for ACP"
    );
    eprintln!("{}", Cli::command().render_usage());
    ExitCode::from(EXIT_USAGE)
}
