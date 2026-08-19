//! The headless one-shot: `basis spawn "<prompt>"` (`basis run` is an alias).
//!
//! Everything between a parsed [`RunArgs`] and an exit code — the config the
//! flags build, the prompt `-` reads from stdin, and the prose the event stream
//! is rendered as.
//!
//! The renderer lives here rather than next to [`basis`]'s events because it
//! is a decision about *this command's* output, not about the stream: `--json`
//! swaps it for a [`JsonlWriter`] and nothing else about the run changes. That
//! is also why both branches end at the same [`exit_code`] call — the code a
//! script reads must not depend on which renderer was asked for.

use std::{
    io::{self, IsTerminal, Read, Write},
    process::ExitCode,
};

use basis::{
    Bound, Event, JsonlWriter, RunConfig, RunOutcome, ShellAccess, provider,
    run::{EventSink, FnSink},
};
use mentra::ModelSelector;

use crate::{cli::RunArgs, exit::exit_code};

pub(crate) async fn execute_run(args: RunArgs) -> Result<ExitCode, String> {
    let workspace = match args.workspace {
        Some(path) => path,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };

    let mut config = RunConfig::new(workspace, prompt_from(args.prompt)?);

    if let Some(name) = &args.provider {
        config = config.with_provider(provider::parse(name).map_err(|error| error.to_string())?);
    }
    if let Some(base_url) = args.base_url {
        config = config.with_base_url(base_url);
    }
    if let Some(model) = args.model {
        config = config.with_model(ModelSelector::Id(model));
    }

    config = config.with_shell(ShellAccess::from_flag(!args.no_shell));

    if let Some(effort) = args.effort {
        config = config.with_effort(effort.into());
    }

    // A bound that trips ends the run gracefully — the stream closes the way
    // it always does and committed work is kept — so setting one costs a
    // healthy run nothing.
    if let Some(deadline) = args.deadline {
        config = config.with_deadline(deadline.duration());
    }
    if let Some(tool_budget) = args.tool_budget {
        config = config.with_tool_budget(tool_budget);
    }
    if let Some(token_budget) = args.token_budget {
        config = config.with_token_budget(token_budget);
    }

    // Every consequential call is put to this one, and nothing else decides:
    // `always` allows, `never` refuses, `prompt` asks the person.
    let approver = args.approve.approver();

    if args.json {
        let report = basis::run_with_approver(config, JsonlWriter::new(io::stdout()), approver)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(ExitCode::from(exit_code(&report)));
    }

    // Without --json the run is still driven by the same event stream; only
    // the rendering differs. Streaming the assistant's text as it arrives is
    // what makes an interactive invocation feel live.
    let report = basis::run_with_approver(config, prose_sink(), approver)
        .await
        .map_err(|error| error.to_string())?;

    if let RunOutcome::Error { message } = &report.outcome {
        // A tripped bound is not a failure, and calling it one would send
        // someone looking for a broken model when the answer is a smaller
        // task or a larger allowance.
        let what = match report.stopped_by {
            Some(_) => "run stopped",
            None => "run failed",
        };
        eprintln!("basis: {what}: {message}");
    }
    eprintln!("{}", next_hint(&report.outcome, report.stopped_by));

    Ok(ExitCode::from(exit_code(&report)))
}

/// One actionable line after a human-readable result. JSONL does not use this
/// renderer, so its event contract remains a stream of JSON objects only.
fn next_hint(outcome: &RunOutcome, stopped_by: Option<Bound>) -> &'static str {
    if stopped_by.is_some() {
        return "next: retry with `basis spawn <PROMPT>` using a narrower prompt or a larger bound";
    }

    match outcome {
        RunOutcome::Ok => {
            "next: use `basis fingerprint` to inspect workspace changes or `basis spawn <PROMPT>` for another task"
        }
        RunOutcome::Error { .. } => {
            "next: retry with `basis spawn <PROMPT>` after addressing the reported failure"
        }
    }
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

/// Renders events as prose on stdout: assistant text streams through, tool
/// calls and failures are announced on stderr so piping stdout still yields
/// just the answer.
fn prose_sink() -> impl EventSink {
    let quiet = !io::stderr().is_terminal();

    FnSink::new(move |event| {
        match event {
            Event::RunStarted {
                model,
                context_files,
                ..
            } => {
                if !quiet {
                    eprintln!("basis: {model}, {} context file(s)", context_files.len());
                }
            }
            Event::AssistantDelta { text } => {
                print!("{text}");
                io::stdout().flush()?;
            }
            Event::ToolStarted { tool_name, .. } => {
                if !quiet {
                    eprintln!("  · {tool_name}");
                }
            }
            Event::ToolCompleted {
                tool_call_id,
                tool_name,
                summary,
                is_error,
            } if is_error => {
                // The name normally arrives; it is empty only for a result
                // whose call this session never saw. Fall back to the id
                // rather than printing a blank label.
                let label = if tool_name.is_empty() {
                    &tool_call_id
                } else {
                    &tool_name
                };
                eprintln!("  ! {label}: {summary}");
            }
            Event::Notice { message, .. } => eprintln!("basis: {message}"),
            Event::Error { message, .. } => eprintln!("basis: {message}"),
            Event::RunFinished { .. } => println!(),
            _ => {}
        }
        Ok(())
    })
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

    #[test]
    fn a_success_hint_names_two_commands_that_exist() {
        assert_eq!(
            next_hint(&RunOutcome::Ok, None),
            "next: use `basis fingerprint` to inspect workspace changes or `basis spawn <PROMPT>` for another task"
        );
    }

    #[test]
    fn a_bound_hint_suggests_changing_the_bound_or_the_work() {
        assert_eq!(
            next_hint(
                &RunOutcome::Error {
                    message: "deadline exceeded".to_string(),
                },
                Some(Bound::Deadline),
            ),
            "next: retry with `basis spawn <PROMPT>` using a narrower prompt or a larger bound"
        );
    }

    #[test]
    fn a_failure_hint_does_not_guess_the_remedy() {
        assert_eq!(
            next_hint(
                &RunOutcome::Error {
                    message: "provider failed".to_string(),
                },
                None,
            ),
            "next: retry with `basis spawn <PROMPT>` after addressing the reported failure"
        );
    }
}
