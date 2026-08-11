//! The `lan` binary: a thin shell over [`lan_core`] and [`lan_acp`].
//!
//! Per ADR-0003 the library is the product and this is a wrapper; per ADR-0011
//! there are two of them, and the CLI is what sits over both. The grammar is
//! ADR-0015's, and it is five lines:
//!
//! ```text
//! lan                       -> ACP server on stdio, for an editor to spawn
//! lan "<prompt>"            -> shorthand: exactly `lan run "<prompt>"`
//! lan run "<prompt>"        -> headless one-shot; `-` reads the prompt from stdin
//! lan bridge                -> the same ACP server on a websocket, for a browser
//! lan fingerprint           -> the workspace's hash, for a caller's own loop
//! ```
//!
//! A positional argument that is not one of the four subcommands is a prompt,
//! so the human path carries no ceremony and the editor path is untouched. `--`
//! escapes a prompt that collides with a subcommand name: `lan -- run`.
//!
//! # Exit codes
//!
//! These are contract (ADR-0015): a script branches on them without parsing
//! anything. `--json` remains the structured detail.
//!
//! | Code | Meaning |
//! |---|---|
//! | [`EXIT_OK`] | the run finished |
//! | [`EXIT_FAILED`] | the run failed, or lan could not start it |
//! | [`EXIT_USAGE`] | the invocation was wrong |
//! | [`EXIT_BOUNDED`] | a bound tripped: `--deadline` or `--tool-budget` |
//!
//! A `--token-budget` is absent from the last row on purpose: crossing it ends
//! the run gracefully with everything it committed, so the run *succeeded* and
//! exits `0`.

mod approver;
mod bridge;
mod duration_arg;

use std::{
    ffi::{OsStr, OsString},
    io::{self, IsTerminal, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Parser, Subcommand};
use lan_acp::StdioError;
use lan_core::{
    AllowAll, Approver, DenyAll, Event, JsonlWriter, RunConfig, RunOutcome, RunReport, ShellAccess,
    Snapshot, provider,
    run::{EventSink, FnSink},
};
use mentra::ModelSelector;

use crate::{approver::TerminalApprover, duration_arg::DurationArg};

/// The run finished.
const EXIT_OK: u8 = 0;
/// The run failed, or lan could not start it.
const EXIT_FAILED: u8 = 1;
/// The invocation was wrong. clap's own code for a usage error, named here so
/// nothing else takes it and so the table above is complete.
const EXIT_USAGE: u8 = 2;
/// A bound tripped, which is not the same as failing: the run stopped because
/// it reached an allowance its caller set, and kept what it had.
const EXIT_BOUNDED: u8 = 3;

/// What bare `lan` says when the first thing on stdin is not a message.
///
/// The trap ADR-0015 names: an editor spawning lan and a shell pipe look
/// identical from here, so `cat prompt.txt | lan` cannot be detected as a
/// prompt without breaking every editor. What can be done is answer, rather
/// than wait silently, once the input proves it was never a client.
const NOT_A_CLIENT: &str = "expected an ACP client on stdio; did you mean 'lan run -'?";

/// The four subcommands plus clap's own, which is what makes a positional a
/// prompt: anything that is not one of these words is one.
const SUBCOMMANDS: [&str; 5] = ["run", "fingerprint", "acp", "bridge", "help"];

/// Flags the top level answers itself. Every other flag belongs to `run`,
/// which is what lets `lan --json "hi"` mean what it looks like.
const TOP_LEVEL_FLAGS: [&str; 4] = ["-h", "--help", "-V", "--version"];

#[derive(Debug, Parser)]
#[command(
    name = "lan",
    version,
    about = "Lightweight Agent Nucleus — an embeddable agent harness",
    long_about = None,
    after_help = "\
Shorthand:
  lan \"fix the failing test\"    the same as: lan run \"fix the failing test\"
  lan -- run                    a prompt that collides with a subcommand name
  lan run -                     read the prompt from stdin
  lan                           no arguments: the ACP server, on stdio

Exit codes:
  0  the run finished
  1  the run failed, or lan could not start it
  2  the invocation was wrong
  3  a bound tripped (--deadline, --tool-budget); committed work was kept",
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one prompt against a workspace and exit.
    Run(RunArgs),
    /// Print a hash of everything in the workspace a run could see.
    Fingerprint(FingerprintArgs),
    /// Serve the Agent Client Protocol on stdio. Same as no subcommand.
    Acp(AcpArgs),
    /// Serve the same protocol on a websocket, for a client that cannot spawn
    /// a process — a browser one. lan ships no web UI; adopt an ACP client.
    Bridge(BridgeArgs),
}

/// Knobs for the websocket bridge: where to listen, and who to talk to.
#[derive(Debug, clap::Args)]
struct BridgeArgs {
    /// Address to listen on. Loopback unless --allow-non-loopback.
    #[arg(long, value_name = "ADDR", default_value_t = bridge::BridgeConfig::default().bind)]
    bind: SocketAddr,

    /// A web origin allowed to connect, e.g. http://localhost:5173.
    /// Repeatable, matched exactly.
    ///
    /// Without one, no page is served. A page can open a websocket to a
    /// loopback port without asking anyone — the same-origin policy does not
    /// apply to the handshake — so this list is what stands between a site the
    /// user happened to visit and this workspace.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    allow_origin: Vec<String>,

    /// Listen on an address other than loopback.
    ///
    /// Refused by default, and worth refusing: a reachable bridge gives anyone
    /// who can route to it an agent that writes to the workspace, with no
    /// authentication in the protocol to stop them.
    #[arg(long)]
    allow_non_loopback: bool,

    #[command(flatten)]
    acp: AcpArgs,
}

/// How hard the model should think, where the provider supports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum EffortArg {
    Low,
    Medium,
    High,
    #[value(name = "xhigh")]
    XHigh,
    Max,
}

impl From<EffortArg> for lan_core::Effort {
    fn from(effort: EffortArg) -> Self {
        match effort {
            EffortArg::Low => Self::Low,
            EffortArg::Medium => Self::Medium,
            EffortArg::High => Self::High,
            EffortArg::XHigh => Self::XHigh,
            EffortArg::Max => Self::Max,
        }
    }
}

/// Knobs for the ACP server.
///
/// Deliberately fewer than `run` has: an ACP client names the workspace itself
/// (`cwd` on `session/new`) and sends the prompt over the protocol, so what is
/// left is only what the client has no way to say.
#[derive(Debug, Default, clap::Args)]
struct AcpArgs {
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

    /// Stop the agent running commands.
    ///
    /// Commands are on by default: the host owns the boundary, and a harness
    /// that cannot run `cargo test` does very little (ADR-0013). This shuts the
    /// shell and background tools for a run meant to read and report — it
    /// narrows what this run does, it does not confine the process.
    #[arg(long)]
    no_shell: bool,

    /// How hard the model should think: low, medium, high, xhigh, or max.
    /// Unsupported provider/model levels fail instead of being downgraded.
    #[arg(long, value_name = "LEVEL")]
    effort: Option<EffortArg>,

    /// When to ask before the agent changes anything. Defaults to asking the
    /// ACP client, which is the point of a protocol with a permission request
    /// in it.
    #[arg(long, value_name = "MODE", default_value = "prompt")]
    approve: ApproveMode,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// The prompt. What the agent does is entirely this plus the workspace.
    ///
    /// `-` reads the prompt from stdin instead, which is how a generated or
    /// multi-line prompt arrives. A prompt that happens to be a subcommand
    /// name needs `--` in front of it: `lan -- run`.
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
    /// OPENAI_API_KEY. LAN uses complete local replay instead of automatic
    /// previous_response_id chaining on compatible endpoints.
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// Model id. Defaults to the provider's newest available.
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// Stop the agent running commands (shell, background tasks).
    ///
    /// Commands are on by default. A run holds whatever authority your user
    /// account holds, because an in-process path check cannot confine a
    /// process once it is running and lan will not pretend otherwise
    /// (ADR-0013). Confinement, where you want it, comes from the OS —
    /// docs/containerization.md has the patterns.
    ///
    /// This flag is the read-only posture, not a boundary: file writes still
    /// land. For a run that changes nothing, use --approve never.
    #[arg(long)]
    no_shell: bool,

    /// How hard the model should think: low, medium, high, xhigh, or max.
    /// Unsupported provider/model levels fail instead of being downgraded.
    #[arg(long, value_name = "LEVEL")]
    effort: Option<EffortArg>,

    /// When to ask before the agent changes anything: always allow, ask each
    /// time, or refuse. Asking needs a terminal on stdin; without one, a
    /// request is denied rather than silently granted.
    #[arg(long, value_name = "MODE", default_value = "always")]
    approve: ApproveMode,

    /// Give up on the run after this long: 90s, 30m, 2h.
    ///
    /// Unset by default. An attended run has a person watching, and a person
    /// tells "thinking hard" from "stuck" in a way no timer can — so this is
    /// for the run nobody is watching, which has to say so itself.
    #[arg(long, value_name = "DURATION")]
    deadline: Option<DurationArg>,

    /// Cap how many tool calls the run may make.
    #[arg(long, value_name = "N")]
    tool_budget: Option<usize>,

    /// Cap the tokens the run may report using, input plus output.
    ///
    /// Soft: the round that crosses the line finishes, then the run ends
    /// gracefully and keeps what it has. This is the bound that maps to money.
    #[arg(long, value_name = "N")]
    token_budget: Option<u64>,
}

/// Knobs for the fingerprint, which is only ever asked about one directory.
#[derive(Debug, clap::Args)]
struct FingerprintArgs {
    /// Workspace root. Defaults to the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ApproveMode {
    /// Never ask. What an unattended run needs, since nobody is there to
    /// answer and a question nothing answers is a hang.
    Always,
    /// Ask before each consequential call. The default over ACP, where there
    /// is a client to ask.
    #[default]
    Prompt,
    /// Refuse anything that changes state outside the process.
    Never,
}

impl ApproveMode {
    /// The approver this mode installs.
    ///
    /// The three words on the command line are three implementations of one
    /// trait, which is all approval is since ADR-0010. Boxed because they are
    /// different types and the choice is made at runtime — a host writing Rust
    /// names the one it wants and skips this.
    fn approver(self) -> Box<dyn Approver> {
        match self {
            Self::Always => Box::new(AllowAll),
            // Asks on stderr and reads stdin, so it works under either
            // renderer — and denies outright when stdin is not a terminal,
            // which is what makes `--approve prompt` safe to leave in a script
            // that turns out to have nobody watching it.
            Self::Prompt => Box::new(TerminalApprover::new()),
            Self::Never => Box::new(DenyAll),
        }
    }
}

impl From<ApproveMode> for lan_acp::ApprovalMode {
    fn from(mode: ApproveMode) -> Self {
        match mode {
            ApproveMode::Always => Self::Always,
            ApproveMode::Prompt => Self::Prompt,
            ApproveMode::Never => Self::Never,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse_from(normalize(std::env::args_os()));

    match cli.command {
        Some(Command::Run(args)) => match execute_run(args).await {
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
        // No subcommand serves ACP: the embedded case is the primary case
        // (ADR-0002, ADR-0003), so it is what you get by default.
        Some(Command::Acp(args)) => serve_acp(args).await,
        Some(Command::Bridge(args)) => serve_bridge(args).await,
        None => serve_acp(AcpArgs::default()).await,
    }
}

/// Rewrites `lan "<prompt>"` as `lan run "<prompt>"`.
///
/// Done here rather than in the parser because the shorthand is a statement
/// about one token — the first — and clap has no way to express "this
/// positional is a subcommand unless it isn't". Deciding on the first argument
/// alone is what makes the rule total: a flag's *value* can look like anything
/// (`lan --model gpt-5 "hi"`), and scanning further would have to know every
/// flag's arity to avoid mistaking `gpt-5` for the prompt.
fn normalize(argv: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = argv.into_iter().collect();

    // Bare `lan`: the ACP server, byte-identical to what an editor spawns.
    let Some(first) = argv.get(1) else {
        return argv;
    };

    if starts_a_run(first) {
        argv.insert(1, OsString::from("run"));
    }

    argv
}

/// Whether the first argument opens a run, rather than naming a subcommand or
/// asking the top level for help.
///
/// `--` lands here as a run too, which is what makes it the escape: `lan -- run`
/// becomes `lan run -- run`, and the word arrives as a prompt.
fn starts_a_run(first: &OsStr) -> bool {
    match first.to_str() {
        // Not UTF-8, so it is neither a reserved word nor a flag lan defines.
        // `run` takes it as a prompt and clap reports the encoding.
        None => true,
        Some(word) => !SUBCOMMANDS.contains(&word) && !TOP_LEVEL_FLAGS.contains(&word),
    }
}

async fn serve_acp(args: AcpArgs) -> ExitCode {
    let config = match acp_config(args) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("lan: {message}");
            return ExitCode::from(EXIT_FAILED);
        }
    };

    match lan_acp::serve_stdio(config).await {
        Ok(()) => ExitCode::from(EXIT_OK),
        // Not an error the server had — an invocation that was never going to
        // work. Said in the vocabulary of the command line, because that is
        // where the fix is.
        Err(StdioError::NotAClient) => {
            eprintln!("lan: {NOT_A_CLIENT}");
            ExitCode::from(EXIT_USAGE)
        }
        Err(error) => {
            eprintln!("lan: acp: {error}");
            ExitCode::from(EXIT_FAILED)
        }
    }
}

/// Serves the ACP server on a websocket instead of stdio.
///
/// The bound address is printed before serving: with `--bind 127.0.0.1:0` it
/// is the only way to learn the port, and with any bind it is the URL a client
/// is configured with.
async fn serve_bridge(args: BridgeArgs) -> ExitCode {
    let config = match acp_config(args.acp) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("lan: {message}");
            return ExitCode::from(EXIT_FAILED);
        }
    };

    let mut bridge = bridge::BridgeConfig::new(args.bind).with_origins(args.allow_origin);
    if args.allow_non_loopback {
        bridge = bridge.allowing_non_loopback();
    }
    let serves_no_page = bridge.allowed_origins.is_empty();

    let bridge = match bridge::Bridge::bind(bridge).await {
        Ok(bridge) => bridge,
        Err(error) => {
            eprintln!("lan: bridge: {error}");
            return ExitCode::from(EXIT_FAILED);
        }
    };

    match bridge.local_addr() {
        Ok(address) => eprintln!("lan: bridge listening on ws://{address}"),
        Err(error) => {
            eprintln!("lan: bridge: {error}");
            return ExitCode::from(EXIT_FAILED);
        }
    }

    // Said after the address, not before: it explains why a browser client
    // that is about to be pointed here will be turned away, and it would be
    // noise on a bind that never happened.
    if serves_no_page {
        eprintln!(
            "lan: bridge: no web origin allowed, so no page is served. \
             Pass --allow-origin <ORIGIN> for a browser client."
        );
    }

    match bridge.serve(config).await {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            eprintln!("lan: bridge: {error}");
            ExitCode::from(EXIT_FAILED)
        }
    }
}

/// Builds the template each ACP session is configured from.
///
/// The workspace is a placeholder: every session replaces it with the `cwd`
/// the client sends. It has to be *something* because `RunConfig` requires
/// one, and the current directory is the least surprising stand-in.
fn acp_config(args: AcpArgs) -> Result<lan_acp::ServeConfig, String> {
    let workspace =
        std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?;

    let mut config = RunConfig::new(workspace, "").with_session_name("lan acp");

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

    Ok(lan_acp::ServeConfig::new(config).with_initial_mode(args.approve.into()))
}

async fn execute_run(args: RunArgs) -> Result<ExitCode, String> {
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
        let report = lan_core::run_with_approver(config, JsonlWriter::new(io::stdout()), approver)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(ExitCode::from(exit_code(&report)));
    }

    // Without --json the run is still driven by the same event stream; only
    // the rendering differs. Streaming the assistant's text as it arrives is
    // what makes an interactive invocation feel live.
    let report = lan_core::run_with_approver(config, prose_sink(), approver)
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
        eprintln!("lan: {what}: {message}");
    }

    Ok(ExitCode::from(exit_code(&report)))
}

/// The prompt as `run` was given it, reading stdin when it is `-`.
///
/// Explicit rather than detected: bare `lan` already owns stdin for the ACP
/// server, and no amount of sniffing can tell an editor's pipe from a shell's
/// (ADR-0015). So the caller says which one this is, with one character.
fn prompt_from(argument: String) -> Result<String, String> {
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

/// Prints the workspace fingerprint, or says why there is none.
///
/// The point is two lines of somebody else's loop: keep the last hash, compare
/// it to this one, and skip the model when they match. So a workspace that
/// cannot be fingerprinted prints *nothing* and exits nonzero — an empty string
/// compared against a previous empty string is the false "unchanged" that stops
/// such a loop working with nothing in the output to say so.
fn execute_fingerprint(args: FingerprintArgs) -> Result<ExitCode, String> {
    let workspace = match args.workspace {
        Some(path) => path,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };

    println!("{}", fingerprint_line(&workspace)?);

    Ok(ExitCode::from(EXIT_OK))
}

/// The one line stdout gets, or the reason stdout gets nothing.
fn fingerprint_line(workspace: &Path) -> Result<String, String> {
    match lan_core::fingerprint::snapshot(workspace) {
        Snapshot::Known(fingerprint) => Ok(fingerprint.hex()),
        Snapshot::Unknown { reason } => Err(reason),
    }
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

/// The exit code a finished run earns.
///
/// A tripped bound is checked first because it also failed: the outcome on the
/// stream is an error either way, and "you ran out of the time you set" is the
/// more useful of the two things that are true (ADR-0015).
fn exit_code<S>(report: &RunReport<S>) -> u8 {
    match report.stopped_by {
        Some(_) => EXIT_BOUNDED,
        None if report.succeeded() => EXIT_OK,
        None => EXIT_FAILED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EFFORTS: [(&str, EffortArg); 5] = [
        ("low", EffortArg::Low),
        ("medium", EffortArg::Medium),
        ("high", EffortArg::High),
        ("xhigh", EffortArg::XHigh),
        ("max", EffortArg::Max),
    ];

    #[test]
    fn run_accepts_exactly_the_five_effort_spellings() {
        for (value, expected) in EFFORTS {
            let cli = Cli::try_parse_from(["lan", "run", "prompt", "--effort", value])
                .expect("effort should parse");
            let Some(Command::Run(args)) = cli.command else {
                panic!("run command should parse");
            };
            assert_eq!(args.effort, Some(expected));
        }

        for invalid in ["x-high", "x_high", "none", "minimal", "ultra"] {
            assert!(
                Cli::try_parse_from(["lan", "run", "prompt", "--effort", invalid]).is_err(),
                "unexpectedly accepted {invalid}"
            );
        }
    }

    #[test]
    fn acp_accepts_the_same_five_effort_spellings() {
        for (value, expected) in EFFORTS {
            let cli = Cli::try_parse_from(["lan", "acp", "--effort", value])
                .expect("effort should parse");
            let Some(Command::Acp(args)) = cli.command else {
                panic!("ACP command should parse");
            };
            assert_eq!(args.effort, Some(expected));
        }
    }

    fn run_args(args: &[&str]) -> RunArgs {
        let mut argv = vec!["lan", "run", "prompt"];
        argv.extend_from_slice(args);

        let Some(Command::Run(parsed)) = Cli::try_parse_from(argv).expect("parses").command else {
            panic!("run command should parse");
        };
        parsed
    }

    #[test]
    fn a_run_states_its_own_bounds_or_has_none() {
        // ADR-0014: nothing here defaults a bound on an operator's behalf,
        // because there is no interval left to guess one from.
        let plain = run_args(&[]);

        assert_eq!(plain.deadline, None);
        assert_eq!(plain.tool_budget, None);
        assert_eq!(plain.token_budget, None);
    }

    #[test]
    fn the_three_bounds_parse_off_the_command_line() {
        let bounded = run_args(&[
            "--deadline",
            "10m",
            "--tool-budget",
            "40",
            "--token-budget",
            "200000",
        ]);

        assert_eq!(
            bounded.deadline.map(DurationArg::duration),
            Some(std::time::Duration::from_secs(600))
        );
        assert_eq!(bounded.tool_budget, Some(40));
        assert_eq!(bounded.token_budget, Some(200_000));
    }

    #[test]
    fn a_deadline_without_a_unit_is_refused_at_the_command_line() {
        let error =
            Cli::try_parse_from(["lan", "run", "prompt", "--deadline", "30"]).expect_err("refused");

        assert!(
            error.to_string().contains("30m"),
            "the message must show an accepted form: {error}"
        );
    }

    #[test]
    fn fingerprint_defaults_to_the_current_directory() {
        let Some(Command::Fingerprint(args)) = Cli::try_parse_from(["lan", "fingerprint"])
            .expect("parses")
            .command
        else {
            panic!("fingerprint command should parse");
        };

        assert_eq!(args.workspace, None);
    }

    #[test]
    fn fingerprint_takes_a_workspace_the_same_way_run_does() {
        let Some(Command::Fingerprint(args)) =
            Cli::try_parse_from(["lan", "fingerprint", "-C", "/repo"])
                .expect("parses")
                .command
        else {
            panic!("fingerprint command should parse");
        };

        assert_eq!(args.workspace, Some(PathBuf::from("/repo")));
    }

    #[test]
    fn a_workspace_that_cannot_be_fingerprinted_prints_nothing() {
        // The failure this guards against is a shell loop comparing one empty
        // string against another and concluding "unchanged" forever. There is
        // no `Ok` here to print, so nothing reaches stdout.
        let reason = fingerprint_line(Path::new("/definitely/not/a/real/path"))
            .expect_err("an absent workspace has no fingerprint");

        assert!(
            reason.contains("/definitely/not/a/real/path"),
            "the reason must name the workspace: {reason}"
        );
    }

    #[test]
    fn a_fingerprintable_workspace_prints_one_stable_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "one").expect("write");

        let printed = fingerprint_line(dir.path()).expect("a workspace with a file in it");

        assert_eq!(printed.len(), 16);
        assert_eq!(
            printed,
            fingerprint_line(dir.path()).expect("still fingerprints"),
            "a workspace nobody touched must print the same line twice"
        );
    }

    /// The command line as lan sees it after the shorthand is resolved.
    fn normalized(argv: &[&str]) -> Vec<String> {
        normalize(argv.iter().map(OsString::from))
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    /// The `run` the shorthand produced, so a test can check what it carries.
    fn shorthand(argv: &[&str]) -> RunArgs {
        let parsed = Cli::try_parse_from(normalize(argv.iter().map(OsString::from)))
            .expect("the shorthand should parse");

        let Some(Command::Run(args)) = parsed.command else {
            panic!("a prompt should have become a run: {argv:?}");
        };
        args
    }

    #[test]
    fn a_positional_that_is_not_a_subcommand_is_a_prompt() {
        assert_eq!(
            shorthand(&["lan", "fix the failing test"]).prompt,
            "fix the failing test"
        );
    }

    #[test]
    fn the_shorthand_is_exactly_the_run_subcommand() {
        assert_eq!(
            normalized(&["lan", "fix the failing test"]),
            ["lan", "run", "fix the failing test"]
        );
    }

    #[test]
    fn flags_pass_through_the_shorthand() {
        // The interesting half is `--model gpt-5`: a flag's value looks like a
        // positional, so anything that scanned past the first argument would
        // take "gpt-5" for the prompt.
        let args = shorthand(&["lan", "--json", "--model", "gpt-5", "hi"]);

        assert!(args.json);
        assert_eq!(args.model.as_deref(), Some("gpt-5"));
        assert_eq!(args.prompt, "hi");
    }

    #[test]
    fn a_prompt_that_starts_with_a_dash_is_still_a_prompt() {
        assert_eq!(shorthand(&["lan", "-"]).prompt, "-");
        assert_eq!(
            shorthand(&["lan", "-C", "/repo", "hi"]).workspace,
            Some(PathBuf::from("/repo"))
        );
    }

    #[test]
    fn a_prompt_that_names_a_subcommand_needs_the_escape() {
        // Without `--`, `lan run` is the subcommand with its prompt missing —
        // clap says so rather than lan guessing which was meant.
        let ambiguous = Cli::try_parse_from(normalize(["lan", "run"].iter().map(OsString::from)))
            .expect_err("a bare subcommand name is not a prompt");
        assert_eq!(ambiguous.exit_code(), i32::from(EXIT_USAGE));

        assert_eq!(shorthand(&["lan", "--", "run"]).prompt, "run");
        assert_eq!(shorthand(&["lan", "--", "bridge"]).prompt, "bridge");
    }

    #[test]
    fn a_prompt_that_merely_begins_with_a_subcommand_name_needs_nothing() {
        assert_eq!(
            shorthand(&["lan", "run the tests and summarize"]).prompt,
            "run the tests and summarize"
        );
    }

    #[test]
    fn bare_lan_is_left_alone_for_the_editor_that_spawns_it() {
        assert_eq!(normalized(&["lan"]), ["lan"]);

        let parsed = Cli::try_parse_from(["lan"]).expect("parses");
        assert!(
            parsed.command.is_none(),
            "no subcommand is the ACP server; inserting one would break every editor"
        );
    }

    #[test]
    fn every_subcommand_still_reaches_itself() {
        for subcommand in SUBCOMMANDS {
            assert_eq!(
                normalized(&["lan", subcommand]),
                ["lan", subcommand],
                "{subcommand} must not be rewritten as a prompt"
            );
        }
    }

    #[test]
    fn the_top_level_still_answers_for_help_and_version() {
        for flag in TOP_LEVEL_FLAGS {
            assert_eq!(normalized(&["lan", flag]), ["lan", flag]);
        }
    }

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

    /// A report with nothing in it but the two fields the exit code reads.
    fn report(outcome: RunOutcome, stopped_by: Option<lan_core::Bound>) -> RunReport<()> {
        RunReport {
            session_id: "s1".to_string(),
            model: "gpt-5".to_string(),
            provider: "openai".to_string(),
            final_message: None,
            outcome,
            stopped_by,
            sink: (),
        }
    }

    #[test]
    fn a_finished_run_exits_zero() {
        assert_eq!(exit_code(&report(RunOutcome::Ok, None)), EXIT_OK);
    }

    #[test]
    fn a_tripped_bound_is_told_apart_from_a_failure_by_the_exit_code() {
        // The whole point of the contract: `lan run --deadline 10m …; case $? in`
        // has to be able to retry a bounded run and escalate a failed one.
        let failed = report(
            RunOutcome::Error {
                message: "provider refused the request".to_string(),
            },
            None,
        );
        let bounded = report(
            RunOutcome::Error {
                message: "deadline exceeded".to_string(),
            },
            Some(lan_core::Bound::Deadline),
        );

        assert_eq!(exit_code(&failed), EXIT_FAILED);
        assert_eq!(exit_code(&bounded), EXIT_BOUNDED);
        assert_ne!(
            exit_code(&failed),
            exit_code(&bounded),
            "a shell script must be able to tell the two apart"
        );
    }

    #[test]
    fn the_signpost_names_the_invocation_that_would_have_worked() {
        // A silent wait was the old failure. The message replaces it, and the
        // only part that matters is that it says what to type instead.
        assert!(
            NOT_A_CLIENT.contains("lan run -"),
            "the signpost must name the fix: {NOT_A_CLIENT}"
        );
    }

    #[test]
    fn commands_are_on_unless_the_run_says_no_shell() {
        assert!(!run_args(&[]).no_shell, "ADR-0013: on by default");
        assert!(run_args(&["--no-shell"]).no_shell);

        let Some(Command::Acp(acp)) = Cli::try_parse_from(["lan", "acp", "--no-shell"])
            .expect("parses")
            .command
        else {
            panic!("ACP command should parse");
        };
        assert!(acp.no_shell);
    }

    #[test]
    fn the_retired_grant_is_gone_rather_than_quietly_ignored() {
        // Someone with `--allow-shell` in a script should be told it no longer
        // exists, not have it silently accepted or, worse, taken as a prompt.
        let error =
            Cli::try_parse_from(["lan", "run", "prompt", "--allow-shell"]).expect_err("refused");

        assert_eq!(error.exit_code(), i32::from(EXIT_USAGE));
    }
}
