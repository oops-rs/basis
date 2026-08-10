//! `lan watch` — the binary's half of the scheduler.
//!
//! A module of the binary rather than lines in `main.rs`, for the same reason
//! `run` and `acp` keep their config-building in one function each: everything
//! `watch` adds to the command line lives in one place, and the crate root
//! gains only a subcommand.
//!
//! The scheduler itself is [`lan::watch`]. What is here is argument parsing,
//! prose rendering, and the one thing a library must not do for its host —
//! take over Ctrl-C.

use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
};

use lan::{
    ApprovalPolicy, Event, RunConfig, ShellAccess, TerminalApprover, provider,
    watch::{
        Interval, IterationOutcome, RunReason, Shutdown, StopReason, WatchConfig, WatchEvent,
        WatchJsonlWriter, WatchSink, WatchSummary,
    },
};
use mentra::ModelSelector;

use crate::ApproveMode;

#[derive(Debug, clap::Args)]
pub(crate) struct WatchArgs {
    /// The prompt, run again on every interval. What the agent does is
    /// entirely this plus the workspace.
    prompt: String,

    /// How long to wait between iterations: 90s, 30m, 2h, 1d.
    ///
    /// Required, and with no default on purpose. How often a watch spends
    /// money is not something to guess at on someone's behalf.
    #[arg(long, value_name = "INTERVAL")]
    every: Interval,

    /// Workspace root. Defaults to the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// Emit the JSONL event stream on stdout instead of prose.
    #[arg(long)]
    json: bool,

    /// How hard the model should think: low, medium, high, xhigh, or max.
    /// Unsupported provider/model levels fail instead of being downgraded.
    #[arg(long, value_name = "LEVEL")]
    effort: Option<crate::EffortArg>,

    /// Give up on an iteration after this long: 90s, 30m, 2h.
    ///
    /// Defaults to the interval. A turn that outlives its own period is not
    /// converging, and the next tick is already due — so bounding it there
    /// costs a healthy run nothing and stops a stuck one from running until
    /// somebody notices.
    #[arg(long, value_name = "DURATION")]
    deadline: Option<Interval>,

    /// Cap how many tool calls one iteration may make.
    #[arg(long, value_name = "N")]
    tool_budget: Option<usize>,

    /// Cap the tokens one iteration may report using, input plus output.
    ///
    /// Soft: the round that crosses the line finishes, then the turn ends
    /// gracefully and keeps what it has. This is the bound that maps to money.
    #[arg(long, value_name = "N")]
    token_budget: Option<u64>,

    /// Run every interval even when the workspace has not changed.
    ///
    /// The default skips an iteration whose workspace is identical to what the
    /// last successful run left behind, since it would ask the same question
    /// of the same material. Use this when the answer depends on something the
    /// workspace cannot show — the clock, an upstream repository, a service.
    #[arg(long)]
    always: bool,

    /// Stop after this many iterations. Counts skipped ones too, so a bound
    /// always ends.
    #[arg(long, value_name = "N")]
    max_iterations: Option<u64>,

    /// Provider to use. Defaults to whichever API key is in the environment.
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,

    /// An OpenAI-compatible endpoint, e.g. http://127.0.0.1:3455/v1. LAN uses
    /// complete local replay instead of automatic previous_response_id chaining.
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// Model id. Defaults to the provider's newest available.
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// Let the agent run commands (shell, background tasks). See
    /// `lan run --help`; the same warning applies, unattended.
    #[arg(long)]
    allow_shell: bool,

    /// When to ask before the agent changes anything.
    ///
    /// Defaults to allowing, because a watch is unattended by construction.
    /// `prompt` needs a terminal that somebody is actually sitting at; with
    /// nothing on stdin every request is denied rather than granted.
    #[arg(long, value_name = "MODE", default_value = "always")]
    approve: ApproveMode,
}

pub(crate) async fn execute(args: WatchArgs) -> Result<ExitCode, String> {
    let json = args.json;
    let config = build(args)?;
    let shutdown = config.shutdown.clone();

    install_signal_handler(shutdown);

    let summary = if json {
        lan::watch::watch(config, WatchJsonlWriter::new(io::stdout()))
            .await
            .map_err(|error| error.to_string())?
    } else {
        lan::watch::watch(config, ProseSink::new())
            .await
            .map_err(|error| error.to_string())?
    };

    Ok(exit_code(&summary))
}

fn build(args: WatchArgs) -> Result<WatchConfig, String> {
    let workspace = match args.workspace {
        Some(path) => path,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };

    let mut run = RunConfig::new(workspace, args.prompt).with_session_name("lan watch");

    if let Some(name) = &args.provider {
        run = run.with_provider(provider::parse(name).map_err(|error| error.to_string())?);
    }
    if let Some(base_url) = args.base_url {
        run = run.with_base_url(base_url);
    }
    if let Some(model) = args.model {
        run = run.with_model(ModelSelector::Id(model));
    }

    // The flag grants; the variable grants; neither can revoke the other.
    let shell = if args.allow_shell {
        ShellAccess::Granted
    } else {
        ShellAccess::from_env()
    };
    run = run
        .with_shell(shell)
        .with_approval(ApprovalPolicy::from(args.approve));

    if let Some(warning) = lan::shell::unconfined_warning(shell) {
        eprintln!("lan: warning: {warning}");
    }

    let mut config = WatchConfig::new(run, args.every)
        .with_always(args.always)
        .with_bounds(lan::watch::IterationBounds {
            deadline: args.deadline.map(|deadline| deadline.duration()),
            tool_budget: args.tool_budget,
            token_budget: args.token_budget,
        })
        // A terminal approver on every iteration. Under any policy but
        // `prompt` it is never consulted; under `prompt` it is what stops the
        // library refusing to start.
        .with_approver(TerminalApprover::new);

    if let Some(effort) = args.effort {
        config.run = config.run.with_effort(effort.into());
    }

    if let Some(max) = args.max_iterations {
        config = config.with_max_iterations(max);
    }

    Ok(config)
}

/// Hands Ctrl-C to the watch, and keeps the second one for the process.
///
/// The library deliberately installs no signal handler — that would steal the
/// signal from any host embedding it — so this is the binary's job.
///
/// Both arms matter. The first asks the watch to stop, which wakes it out of
/// its wait and abandons any turn in flight. The second exists because tokio's
/// handler replaces the process's default disposition for good: without it, a
/// turn that somehow ignored its cancellation token would leave a process that
/// no amount of Ctrl-C could end.
fn install_signal_handler(shutdown: Shutdown) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }

        eprintln!("lan: stopping — press Ctrl-C again to give up on the current iteration");
        shutdown.stop();

        if tokio::signal::ctrl_c().await.is_ok() {
            // 128 + SIGINT, which is what a shell reports for an interrupted
            // process and what a supervisor reads as "asked to stop".
            std::process::exit(130);
        }
    });
}

/// Success unless something actually failed.
///
/// A watch that was interrupted after doing its job cleanly is not a failure —
/// stopping is how a watch ends. An iteration that could not run is, however
/// many others succeeded, because the whole point is that it keeps happening.
fn succeeded(summary: &WatchSummary) -> bool {
    summary.failed == 0
}

fn exit_code(summary: &WatchSummary) -> ExitCode {
    if succeeded(summary) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Renders a watch as prose: the assistant's text on stdout, everything about
/// the schedule on stderr, so piping stdout still yields just the answers.
struct ProseSink {
    /// Per-tool chatter is suppressed when stderr is not a terminal, the same
    /// rule `lan run` uses. The scheduler's own lines are never suppressed —
    /// under a supervisor they are the log.
    quiet: bool,
}

impl ProseSink {
    fn new() -> Self {
        Self {
            quiet: !io::stderr().is_terminal(),
        }
    }
}

impl WatchSink for ProseSink {
    fn watch_event(&mut self, event: WatchEvent) -> io::Result<()> {
        match event {
            WatchEvent::WatchStarted {
                workspace,
                every_ms,
                change_detection,
                ..
            } => {
                let cadence = Interval::from_duration(std::time::Duration::from_millis(every_ms));
                let detection = if change_detection {
                    ", skipping when unchanged"
                } else {
                    ""
                };
                eprintln!(
                    "lan: watching {} every {cadence}{detection}",
                    workspace.display()
                );
            }
            WatchEvent::IterationStarted { iteration, reason } => {
                eprintln!("lan: iteration {iteration} ({})", describe(reason));
            }
            WatchEvent::IterationSkipped { iteration, .. } => {
                eprintln!("lan: iteration {iteration} skipped — workspace unchanged");
            }
            WatchEvent::IterationFinished { iteration, outcome } => match outcome {
                IterationOutcome::Ok => {}
                IterationOutcome::Error { message } => {
                    eprintln!("lan: iteration {iteration} failed: {message}");
                }
                IterationOutcome::SetupFailed { message } => {
                    eprintln!("lan: iteration {iteration} could not start: {message}");
                }
            },
            WatchEvent::WatchStopped {
                reason,
                ran,
                skipped,
                failed,
                ..
            } => {
                let ending = match reason {
                    StopReason::Interrupted => "stopped",
                    StopReason::Completed => "finished",
                };
                eprintln!("lan: {ending} — {ran} run(s), {skipped} skipped, {failed} failed");
            }
        }

        Ok(())
    }

    fn run_event(&mut self, _iteration: u64, event: Event) -> io::Result<()> {
        match event {
            Event::AssistantDelta { text } => {
                print!("{text}");
                io::stdout().flush()?;
            }
            Event::ToolStarted { tool_name, .. } if !self.quiet => {
                eprintln!("  · {tool_name}");
            }
            Event::ToolCompleted {
                tool_call_id,
                tool_name,
                summary,
                is_error,
            } if is_error => {
                let label = if tool_name.is_empty() {
                    tool_call_id
                } else {
                    tool_name
                };
                eprintln!("  ! {label}: {summary}");
            }
            Event::Notice { message, .. } | Event::Error { message, .. } => {
                eprintln!("lan: {message}");
            }
            Event::RunFinished { .. } => println!(),
            _ => {}
        }

        Ok(())
    }
}

fn describe(reason: RunReason) -> &'static str {
    match reason {
        RunReason::First => "first",
        RunReason::Changed => "workspace changed",
        RunReason::Unknown => "workspace state unknown",
        RunReason::Always => "change detection off",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// The subcommand as clap sees it, so parsing can be exercised without the
    /// rest of the binary.
    #[derive(Debug, Parser)]
    struct Harness {
        #[command(subcommand)]
        command: Wrapper,
    }

    #[derive(Debug, clap::Subcommand)]
    enum Wrapper {
        Watch(WatchArgs),
    }

    fn parse(args: &[&str]) -> Result<WatchArgs, clap::Error> {
        let mut argv = vec!["lan", "watch"];
        argv.extend_from_slice(args);

        Harness::try_parse_from(argv).map(|harness| match harness.command {
            Wrapper::Watch(args) => args,
        })
    }

    #[test]
    fn an_interval_is_required() {
        // Nothing here should invent a schedule on the operator's behalf.
        assert!(parse(&["do the thing"]).is_err());
    }

    #[test]
    fn a_bad_interval_is_rejected_at_the_command_line() {
        let error = parse(&["do the thing", "--every", "30"]).expect_err("refused");

        assert!(
            error.to_string().contains("30m"),
            "the message must show an accepted form: {error}"
        );
    }

    #[test]
    fn the_defaults_skip_and_never_stop() {
        let args = parse(&["do the thing", "--every", "30m"]).expect("parses");

        assert!(!args.always);
        assert_eq!(args.max_iterations, None);
        assert_eq!(args.approve, ApproveMode::Always);
        assert_eq!(args.every.to_string(), "30m");
    }

    #[test]
    fn the_escape_hatch_is_spelled_always() {
        let args = parse(&["do it", "--every", "2h", "--always"]).expect("parses");

        assert!(args.always);
    }

    #[test]
    fn watch_accepts_the_five_effort_spellings() {
        for (value, expected) in [
            ("low", crate::EffortArg::Low),
            ("medium", crate::EffortArg::Medium),
            ("high", crate::EffortArg::High),
            ("xhigh", crate::EffortArg::XHigh),
            ("max", crate::EffortArg::Max),
        ] {
            let args = parse(&["do the thing", "--every", "30m", "--effort", value])
                .expect("effort should parse");

            assert_eq!(args.effort, Some(expected));
        }
    }

    #[test]
    fn a_failed_iteration_is_a_failed_process() {
        let clean = WatchSummary {
            iterations: 3,
            ran: 1,
            skipped: 2,
            failed: 0,
            stop: StopReason::Interrupted,
        };
        assert!(
            succeeded(&clean),
            "being interrupted is how a watch ends, not a failure"
        );

        assert!(!succeeded(&WatchSummary { failed: 1, ..clean }));
    }
}
