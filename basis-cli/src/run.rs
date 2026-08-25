//! The attended one-shot: the JSONL stream, and the prompt `-` reads from
//! stdin.
//!
//! This is [`Route::Attended`](crate::route::Route::Attended) and nothing
//! else. That route is granted for `--json` alone (ADR-0020), so the stream
//! *is* the rendering here: a `run_started` line, whatever happened, and a
//! `run_finished` line, on stdout, for a consumer that reads the bookends.
//! Every other spelling mints a checkpoint and goes through
//! [`local`](crate::local) instead, which is where a person's terminal is
//! rendered to.
//!
//! Nothing is minted here, so there is no handle and nothing to `send` to —
//! the price of a run that leaves no trace, paid deliberately for the one
//! contract that predates checkpoints.

use std::{
    io::{self, Read},
    process::ExitCode,
};

use basis::{JsonlWriter, RunSpec, Runtime, ShellAccess, Workspace, provider};
use mentra::ModelSelector;

use crate::{cli::RunArgs, exit::exit_code};

pub(crate) async fn execute_run(args: RunArgs) -> Result<ExitCode, String> {
    let prompt = prompt_from(args.prompt)?;
    // Refused before anything else happens: `Workspace::open` resolves a
    // provider and spawns every server `.mcp.json` names, and a refusal that
    // has already spawned processes is not a refusal. The free functions make
    // the same check first, for the same reason.
    if prompt.trim().is_empty() {
        return Err("prompt is empty".to_string());
    }

    let workspace = match args.workspace {
        Some(path) => path,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };

    // The same per-workspace store dir `basis spawn`'s attach path resolves
    // (`local::data_dir::DataDir::resolve_store_dir`, G4 of the whole-wave
    // review) — without it this route fell back to mentra's process-cwd
    // default, so a `basis "<prompt>"` and a `basis spawn ...` against the
    // same repository saw two conversation stores and two memory roots
    // instead of one. Resolved before the runtime is built for the same
    // reason `Workspace::open` resolves memory before acquiring one: a
    // directory a run's history depends on should fail loudly if it cannot
    // be established, not be silently substituted.
    let store_dir = crate::local::DataDir::discover()
        .map_err(|error| format!("open data directory: {error}"))?
        .resolve_store_dir(&workspace)?;

    // The process half seeds the private runtime the workspace builds
    // (ADR-0018): which provider answers, and at which endpoint.
    let mut runtime = Runtime::builder().with_store_dir(store_dir);
    if let Some(name) = &args.provider {
        runtime = runtime.with_provider(provider::parse(name).map_err(|error| error.to_string())?);
    }
    if let Some(base_url) = args.base_url {
        runtime = runtime.with_base_url(base_url);
    }

    let mut builder = Workspace::builder(workspace)
        .with_runtime_builder(runtime)
        .with_shell(ShellAccess::from_flag(!args.no_shell));
    if let Some(model) = args.model {
        builder = builder.with_model(ModelSelector::Id(model));
    }

    let mut spec = RunSpec::new(prompt);
    if let Some(effort) = args.effort {
        spec = spec.with_effort(effort.into());
    }

    // A bound that trips ends the run gracefully — the stream closes the way
    // it always does and committed work is kept — so setting one costs a
    // healthy run nothing.
    if let Some(deadline) = args.deadline {
        spec = spec.with_deadline(deadline.duration());
    }
    if let Some(tool_budget) = args.tool_budget {
        spec = spec.with_tool_budget(tool_budget);
    }
    if let Some(token_budget) = args.token_budget {
        spec = spec.with_token_budget(token_budget);
    }

    // Every consequential call is put to this one, and nothing else decides:
    // `always` allows, `never` refuses, `prompt` asks the person.
    let approver = args.approve.approver();

    // Held open past the mint: the workspace's hooks and MCP connections live
    // exactly as long as it does, and the turn below still needs them.
    let workspace = builder.open().await.map_err(|error| error.to_string())?;

    // The stream is the whole output: every fact a caller could want — the
    // outcome, the bound that tripped, the failure's words — is a line on it,
    // so there is nothing left for this function to say afterwards.
    let report = workspace
        .prepare(spec)
        .map_err(|error| error.to_string())?
        .execute_with_approver(JsonlWriter::new(io::stdout()), approver)
        .await
        .map_err(|error| error.to_string())?;
    Ok(ExitCode::from(exit_code(&report)))
}

/// The prompt as `spawn` (or its `run` alias) was given it, reading stdin when
/// it is `-`.
///
/// Explicit rather than detected: `basis serve --acp` owns stdin for the ACP
/// server, while `basis spawn -` owns stdin for a prompt. The caller says which
/// one this is, with the command and one character (ADR-0017).
pub(crate) fn prompt_from(argument: String) -> Result<String, String> {
    match argument.as_str() {
        "-" => read_prompt(io::stdin().lock()),
        _ => Ok(argument),
    }
}

/// Reads a whole prompt, refusing an empty one.
///
/// Whole, not a line: a generated prompt spans paragraphs, and a reader that
/// stopped at the first newline would silently run a fraction of what it was
/// given. An empty stdin is refused here rather than deeper, where the message
/// would say "prompt is empty" without saying where the prompt was looked for.
fn read_prompt(mut source: impl Read) -> Result<String, String> {
    let mut prompt = String::new();
    source
        .read_to_string(&mut prompt)
        .map_err(|error| format!("could not read the prompt from stdin: {error}"))?;

    if prompt.trim().is_empty() {
        return Err(
            "no prompt on stdin: `-` reads one from stdin, and nothing arrived".to_string(),
        );
    }

    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dash_reads_the_prompt_from_stdin() {
        let prompt = read_prompt(&b"fix the failing test\nthen push\n"[..])
            .expect("a prompt arrived on stdin");

        assert_eq!(
            prompt, "fix the failing test\nthen push\n",
            "a multi-line prompt must arrive whole, not truncated at the first newline"
        );
    }

    #[test]
    fn an_empty_stdin_says_where_the_prompt_was_looked_for() {
        let reason = read_prompt(&b"  \n"[..]).expect_err("whitespace is not a prompt");

        assert!(
            reason.contains("stdin"),
            "the reason must name stdin: {reason}"
        );
    }

    #[test]
    fn a_prompt_that_is_not_a_dash_is_taken_as_written() {
        assert_eq!(
            prompt_from("fix the failing test".to_string()).expect("a literal prompt"),
            "fix the failing test"
        );
    }

    /// The route table is what keeps this module's contract true: reaching
    /// [`execute_run`] without `--json` would put a JSONL stream where a
    /// person expected an answer.
    #[test]
    fn the_attended_route_is_reached_only_for_the_stream_it_renders() {
        use clap::Parser;

        use crate::{
            cli::{Cli, Command},
            route::{Route, route},
        };

        for flags in [
            vec![],
            vec!["--json"],
            vec!["--await"],
            vec!["--json", "--await"],
            vec!["--resumable"],
            vec!["--detached"],
        ] {
            let mut argv = vec!["basis", "spawn", "a prompt"];
            argv.extend_from_slice(&flags);
            let Some(Command::Spawn(args)) = Cli::try_parse_from(argv).expect("parses").command
            else {
                panic!("spawn parses");
            };

            for in_task in [false, true] {
                if route(&args, in_task) == Route::Attended {
                    assert!(args.json, "attended without --json: {flags:?}");
                }
            }
        }
    }
}
