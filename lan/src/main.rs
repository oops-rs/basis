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
    Event, JsonlWriter, RunConfig, RunOutcome, RunReport, ShellAccess, provider,
    run::{EventSink, FnSink},
    shell,
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

    /// An OpenAI-compatible endpoint, e.g. http://127.0.0.1:3455/v1. Paste the
    /// URL as published; a trailing /v1 is handled. Falls back to
    /// LAN_BASE_URL or OPENAI_BASE_URL; the key comes from LAN_API_KEY or
    /// OPENAI_API_KEY.
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// Model id. Defaults to the provider's newest available.
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// Let the agent run commands (shell, background tasks).
    ///
    /// Denied by default: an in-process path check cannot confine a process
    /// once it is running, so this grants real authority over anything your
    /// user account can reach. Sound when something outside the process is
    /// confining the workspace — the container image sets it for that reason.
    /// Also settable with LAN_ALLOW_SHELL=1.
    #[arg(long)]
    allow_shell: bool,
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
    if let Some(base_url) = args.base_url {
        config = config.with_base_url(base_url);
    }
    if let Some(model) = args.model {
        config = config.with_model(ModelSelector::Id(model));
    }

    // The flag grants; the variable grants; neither can revoke the other.
    let shell = if args.allow_shell {
        ShellAccess::Granted
    } else {
        ShellAccess::from_env()
    };
    config = config.with_shell(shell);

    if let Some(warning) = shell::unconfined_warning(shell) {
        eprintln!("lan: warning: {warning}");
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
