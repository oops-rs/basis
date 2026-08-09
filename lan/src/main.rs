//! The `lan` binary: a thin shell over the [`lan`] crate.
//!
//! Per ADR-0003 the library is the product and this is a wrapper. Modes, from
//! `docs/ARCHITECTURE.md` §2:
//!
//! ```text
//! lan                       -> ACP server on stdio (P2)
//! lan run "<prompt>"        -> headless one-shot, JSONL events with --json
//! lan watch "<prompt>"      -> recurring headless runs (P4)
//! ```

use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Parser, Subcommand};
use lan::{
    Event, JsonlWriter, RunConfig, RunOutcome, RunReport, provider,
    run::{EventSink, FnSink},
};
use mentra::ModelSelector;

#[derive(Debug, Parser)]
#[command(
    name = "lan",
    version,
    about = "Lightweight Agent Nucleus — an embeddable agent harness",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one prompt against a workspace and exit.
    Run(RunArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// The prompt. What the agent does is entirely this plus the workspace.
    prompt: String,

    /// Workspace root. Defaults to the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// Emit the JSONL event stream on stdout instead of prose.
    #[arg(long)]
    json: bool,

    /// Provider to use. Defaults to whichever API key is in the environment.
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,

    /// Model id. Defaults to the provider's newest available.
    #[arg(long, value_name = "ID")]
    model: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Run(args)) => match execute_run(args).await {
            Ok(code) => code,
            Err(message) => {
                eprintln!("lan: {message}");
                ExitCode::FAILURE
            }
        },
        // The default mode is the ACP server, which arrives in P2. Saying so
        // is more useful than a generic usage dump.
        None => {
            eprintln!("lan: the ACP server (default mode) is not implemented yet; use `lan run`");
            ExitCode::FAILURE
        }
    }
}

async fn execute_run(args: RunArgs) -> Result<ExitCode, String> {
    let workspace = match args.workspace {
        Some(path) => path,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };

    let mut config = RunConfig::new(workspace, args.prompt);

    if let Some(name) = &args.provider {
        config = config.with_provider(provider::parse(name).map_err(|error| error.to_string())?);
    }
    if let Some(model) = args.model {
        config = config.with_model(ModelSelector::Id(model));
    }

    if args.json {
        let report = lan::run(config, JsonlWriter::new(io::stdout()))
            .await
            .map_err(|error| error.to_string())?;
        return Ok(exit_code(&report));
    }

    // Without --json the run is still driven by the same event stream; only
    // the rendering differs. Streaming the assistant's text as it arrives is
    // what makes an interactive invocation feel live.
    let report = lan::run(config, prose_sink())
        .await
        .map_err(|error| error.to_string())?;

    if let RunOutcome::Error { message } = &report.outcome {
        eprintln!("lan: run failed: {message}");
    }

    Ok(exit_code(&report))
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
                    eprintln!("lan: {model}, {} context file(s)", context_files.len());
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
                // mentra leaves `tool_name` empty on completion
                // (oops-rs/mentra#9); fall back to the id rather than
                // printing a blank. Nothing to remove when that lands — the
                // name simply starts arriving.
                let label = if tool_name.is_empty() {
                    &tool_call_id
                } else {
                    &tool_name
                };
                eprintln!("  ! {label}: {summary}");
            }
            Event::Notice { message, .. } => eprintln!("lan: {message}"),
            Event::Error { message, .. } => eprintln!("lan: {message}"),
            Event::RunFinished { .. } => println!(),
            _ => {}
        }
        Ok(())
    })
}

fn exit_code<S>(report: &RunReport<S>) -> ExitCode {
    if report.succeeded() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
