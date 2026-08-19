//! The grammar, as clap parses it.
//!
//! One module for all of it, because ADR-0017 defines it as one thing: a
//! prompt shorthand, explicit lifecycle verbs, and explicit server modes.
//! "What does basis accept" is a question worth answering by reading one file,
//! and the tests at the bottom check it the same way — as a grammar, not as
//! unrelated argument lists.
//!
//! Only the shape lives here. What each command then *does* is in
//! [`run`](crate::run), [`serve`](crate::serve) and
//! [`fingerprint`](crate::fingerprint), which is why the fields are
//! `pub(crate)`: those modules read them, and nothing outside this crate ever
//! sees them. The rewrite that turns a bare prompt into a `spawn` happens before
//! any of this, in [`shorthand`](crate::shorthand).
//!
//! The two enums are the exception to "only the shape". [`EffortArg`] and
//! [`ApproveMode`] each convert into a type in the layer below, and both are
//! named by two commands — keeping the conversion next to the spelling is what
//! stops `spawn` and `serve` drifting into meaning different things by the same
//! word.

use std::{net::SocketAddr, path::PathBuf};

use basis::{AllowAll, Approver, DenyAll};

use crate::approver::TerminalApprover;
use crate::duration_arg::DurationArg;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "basis",
    version,
    about = "basis — an embeddable agent harness",
    long_about = None,
    after_help = "\
Shorthand:
  basis \"fix the failing test\"    the same as: basis spawn \"fix the failing test\"
  basis -- run                    a prompt that collides with a subcommand name
  basis spawn -                   read the prompt from stdin
  basis spawn \"task\" --await     wait for the task's terminal result
  basis wait <ID>                 wait again using the durable task handle
  basis wait <ID> --message <MID> retry a specific message reply
  basis send <ID> \"message\"      enqueue a follow-up turn
  basis ask <ID> \"question\"      enqueue and await that message's reply
  basis serve --acp               serve ACP on stdio
  basis serve --bridge            serve ACP over a websocket

Exit codes:
  0  the run finished
  1  the run failed, or basis could not start it
  2  the invocation was wrong
  3  a bound tripped (--deadline, --tool-budget, --token-budget); committed work was kept",
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Submit one prompt and return its durable task handle.
    #[command(name = "spawn", alias = "run")]
    Spawn(RunArgs),
    /// Print a hash of everything in the workspace a run could see.
    Fingerprint(FingerprintArgs),
    /// Enqueue a message for a running task.
    Send(SendArgs),
    /// Enqueue a message and wait for its correlated reply.
    Ask(AskArgs),
    /// Wait for a task's durable terminal result.
    Wait(WaitArgs),
    /// Request cancellation of a task and its attached descendants.
    Cancel(CancelArgs),
    /// Follow a task's progress until it reaches a terminal state.
    Watch(WatchArgs),
    /// List the messages accepted by a task.
    Inbox(InboxArgs),
    /// Serve one explicit protocol transport.
    Serve(ServeArgs),
}

#[derive(Debug, clap::Args)]
pub(crate) struct SendArgs {
    /// Opaque task handle printed by `basis spawn`.
    pub(crate) task: String,
    /// Message to enqueue. `-` reads the whole message from stdin.
    pub(crate) message: String,
    /// Wait for this message's correlated reply after enqueueing it.
    #[arg(long = "await")]
    pub(crate) await_result: bool,
    /// Bound the client wait. The task keeps running if the wait expires.
    #[arg(long, value_name = "DURATION", requires = "await_result")]
    pub(crate) timeout: Option<DurationArg>,
    /// Emit one JSON object instead of human-readable output.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct AskArgs {
    /// Opaque task handle printed by `basis spawn`.
    pub(crate) task: String,
    /// Question to enqueue. `-` reads the whole message from stdin.
    pub(crate) message: String,
    /// Bound the reply wait. The task keeps running if the wait expires.
    #[arg(long, value_name = "DURATION")]
    pub(crate) timeout: Option<DurationArg>,
    /// Emit one JSON object instead of human-readable output.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct WaitArgs {
    /// Opaque task handle printed by `basis spawn`.
    pub(crate) task: String,
    /// Retry a specific queued message reply instead of the task terminal.
    #[arg(long, value_name = "MESSAGE_ID")]
    pub(crate) message: Option<String>,
    /// Bound this wait. Defaults to 30m; retrying never reruns the task.
    #[arg(long, value_name = "DURATION")]
    pub(crate) timeout: Option<DurationArg>,
    /// Emit one JSON object instead of human-readable output.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct CancelArgs {
    /// Opaque task handle printed by `basis spawn`.
    pub(crate) task: String,
    /// Emit one JSON object instead of human-readable output.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct WatchArgs {
    /// Opaque task handle printed by `basis spawn`.
    pub(crate) task: String,
    /// Stop following after this long. The task itself keeps running.
    #[arg(long, value_name = "DURATION")]
    pub(crate) timeout: Option<DurationArg>,
    /// Emit progress as JSONL instead of human-readable output.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct InboxArgs {
    /// Task whose accepted messages to list. Defaults to the current basis task.
    pub(crate) task: Option<String>,
    /// Emit one JSON object instead of human-readable output.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Selects which server transport to expose.
#[derive(Debug, clap::Args)]
pub(crate) struct ServeArgs {
    /// Serve ACP over stdio.
    #[arg(long, conflicts_with = "bridge", required_unless_present = "bridge")]
    pub(crate) acp: bool,

    /// Serve ACP over a websocket.
    #[arg(long, conflicts_with = "acp", required_unless_present = "acp")]
    pub(crate) bridge: bool,

    #[command(flatten)]
    pub(crate) acp_args: AcpArgs,

    #[command(flatten)]
    pub(crate) bridge_args: BridgeArgs,
}

/// Knobs for the websocket bridge. These fields are only valid with
/// `serve --bridge`; clap enforces that relation at the command boundary.
#[derive(Debug, clap::Args)]
pub(crate) struct BridgeArgs {
    /// Address to listen on. Loopback unless --allow-non-loopback.
    #[arg(long, value_name = "ADDR", requires = "bridge")]
    pub(crate) bind: Option<SocketAddr>,

    /// A web origin allowed to connect, e.g. http://localhost:5173.
    /// Repeatable, matched exactly.
    #[arg(long = "allow-origin", value_name = "ORIGIN", requires = "bridge")]
    pub(crate) allow_origin: Vec<String>,

    /// Listen on an address other than loopback.
    ///
    #[arg(long, requires = "bridge")]
    pub(crate) allow_non_loopback: bool,
}

impl ServeArgs {
    pub(crate) fn has_bridge_options(&self) -> bool {
        self.bridge_args.bind.is_some()
            || !self.bridge_args.allow_origin.is_empty()
            || self.bridge_args.allow_non_loopback
    }
}

/// How hard the model should think, where the provider supports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum EffortArg {
    Low,
    Medium,
    High,
    #[value(name = "xhigh")]
    XHigh,
    Max,
}

impl From<EffortArg> for basis::Effort {
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
pub(crate) struct AcpArgs {
    /// Provider to use. Defaults to whichever API key is in the environment.
    #[arg(long, value_name = "NAME")]
    pub(crate) provider: Option<String>,

    /// An OpenAI-compatible endpoint, e.g. http://127.0.0.1:3455/v1. basis uses
    /// complete local replay instead of automatic previous_response_id chaining.
    #[arg(long, value_name = "URL")]
    pub(crate) base_url: Option<String>,

    /// Model id. Defaults to the provider's newest available.
    #[arg(long, value_name = "ID")]
    pub(crate) model: Option<String>,

    /// Stop the agent running commands.
    ///
    /// Commands are on by default: the host owns the boundary, and a harness
    /// that cannot run `cargo test` does very little (ADR-0013). This shuts the
    /// shell and background tools for a run meant to read and report — it
    /// narrows what this run does, it does not confine the process.
    #[arg(long)]
    pub(crate) no_shell: bool,

    /// How hard the model should think: low, medium, high, xhigh, or max.
    /// Unsupported provider/model levels fail instead of being downgraded.
    #[arg(long, value_name = "LEVEL")]
    pub(crate) effort: Option<EffortArg>,

    /// When to ask before the agent changes anything. Defaults to asking the
    /// ACP client, which is the point of a protocol with a permission request
    /// in it.
    #[arg(long, value_name = "MODE", default_value = "prompt")]
    pub(crate) approve: ApproveMode,
}

#[derive(Debug, clap::Args)]
pub(crate) struct RunArgs {
    /// The prompt. What the agent does is entirely this plus the workspace.
    ///
    /// `-` reads the prompt from stdin instead, which is how a generated or
    /// multi-line prompt arrives. A prompt that happens to be a subcommand
    /// name needs `--` in front of it: `basis -- run`.
    pub(crate) prompt: String,

    /// Create a new root even when this command runs inside another basis task.
    #[arg(long)]
    pub(crate) detached: bool,

    /// Wait for the task's terminal result instead of returning its handle.
    ///
    /// Implied at a shell, where this process is what drives the agent
    /// (ADR-0020). Inside another basis task it is the explicit opt-in, because
    /// a parent that blocks on a child is how a wait-for cycle starts.
    #[arg(long = "await")]
    pub(crate) await_result: bool,

    /// Print the handle without driving the agent. Progress happens when
    /// something attaches to it — `basis wait`, or any process you background
    /// yourself.
    ///
    /// This is the default inside another basis task and the opt-in at a shell.
    #[arg(long, conflicts_with = "await_result")]
    pub(crate) resumable: bool,

    /// Bound the client wait. The task keeps running if the wait expires.
    #[arg(long, value_name = "DURATION", requires = "await_result")]
    pub(crate) timeout: Option<DurationArg>,

    /// Workspace root. Defaults to the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    pub(crate) workspace: Option<PathBuf>,

    /// Emit one structured task result instead of human-readable output.
    #[arg(long)]
    pub(crate) json: bool,

    /// Provider to use. Defaults to whichever API key is in the environment.
    #[arg(long, value_name = "NAME")]
    pub(crate) provider: Option<String>,

    /// An OpenAI-compatible endpoint, e.g. http://127.0.0.1:3455/v1. Paste the
    /// URL as published; a trailing /v1 is handled. Falls back to
    /// BASIS_BASE_URL or OPENAI_BASE_URL; the key comes from BASIS_API_KEY or
    /// OPENAI_API_KEY. basis uses complete local replay instead of automatic
    /// previous_response_id chaining on compatible endpoints.
    #[arg(long, value_name = "URL")]
    pub(crate) base_url: Option<String>,

    /// Model id. Defaults to the provider's newest available.
    #[arg(long, value_name = "ID")]
    pub(crate) model: Option<String>,

    /// Stop the agent running commands (shell, background tasks).
    ///
    /// Commands are on by default. A run holds whatever authority your user
    /// account holds, because an in-process path check cannot confine a
    /// process once it is running and basis will not pretend otherwise
    /// (ADR-0013). Confinement, where you want it, comes from the OS —
    /// docs/containerization.md has the patterns.
    ///
    /// This flag is the read-only posture, not a boundary: file writes still
    /// land. For a run that changes nothing, use --approve never.
    #[arg(long)]
    pub(crate) no_shell: bool,

    /// How hard the model should think: low, medium, high, xhigh, or max.
    /// Unsupported provider/model levels fail instead of being downgraded.
    #[arg(long, value_name = "LEVEL")]
    pub(crate) effort: Option<EffortArg>,

    /// Approval policy for consequential calls. `prompt` asks at the terminal
    /// of whichever process is driving the agent, so it needs both a terminal
    /// on stdin and a route that drives one (ADR-0020). It is rejected for
    /// `--resumable` work, which has nobody to ask.
    #[arg(long, value_name = "MODE", default_value = "always")]
    pub(crate) approve: ApproveMode,

    /// Give up on the run after this long: 90s, 30m, 2h.
    ///
    /// Local tasks default to 30m because their submitting process exits and
    /// no daemon is left holding the clock. Setting this narrows that default;
    /// attached children can only narrow their parent's inherited deadline.
    #[arg(long, value_name = "DURATION")]
    pub(crate) deadline: Option<DurationArg>,

    /// Cap how many tool calls the run may make.
    #[arg(long, value_name = "N")]
    pub(crate) tool_budget: Option<usize>,

    /// Cap the tokens the run may report using, input plus output.
    ///
    /// Soft: the round that crosses the line finishes, then the run ends
    /// gracefully and keeps what it has. This is the bound that maps to money.
    #[arg(long, value_name = "N")]
    pub(crate) token_budget: Option<u64>,
}

/// Knobs for the fingerprint, which is only ever asked about one directory.
#[derive(Debug, clap::Args)]
pub(crate) struct FingerprintArgs {
    /// Workspace root. Defaults to the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    pub(crate) workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ApproveMode {
    /// Allow consequential calls without asking.
    Always,
    /// Ask the protocol client. Valid for ACP; rejected for local async work.
    #[default]
    Prompt,
    /// Refuse anything that changes state outside the process.
    Never,
}

impl ApproveMode {
    /// Installs the binary approver for the legacy attended JSONL path.
    pub(crate) fn approver(self) -> Box<dyn Approver> {
        match self {
            Self::Always => Box::new(AllowAll),
            Self::Prompt => Box::new(TerminalApprover::new()),
            Self::Never => Box::new(DenyAll),
        }
    }
}

impl From<ApproveMode> for basis_acp::ApprovalMode {
    fn from(mode: ApproveMode) -> Self {
        match mode {
            ApproveMode::Always => Self::Always,
            ApproveMode::Prompt => Self::Prompt,
            ApproveMode::Never => Self::Never,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::exit::EXIT_USAGE;

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
            let cli = Cli::try_parse_from(["basis", "run", "prompt", "--effort", value])
                .expect("effort should parse");
            let Some(Command::Spawn(args)) = cli.command else {
                panic!("run command should parse");
            };
            assert_eq!(args.effort, Some(expected));
        }

        for invalid in ["x-high", "x_high", "none", "minimal", "ultra"] {
            assert!(
                Cli::try_parse_from(["basis", "run", "prompt", "--effort", invalid]).is_err(),
                "unexpectedly accepted {invalid}"
            );
        }
    }

    #[test]
    fn acp_accepts_the_same_five_effort_spellings() {
        for (value, expected) in EFFORTS {
            let cli = Cli::try_parse_from(["basis", "serve", "--acp", "--effort", value])
                .expect("effort should parse");
            let Some(Command::Serve(args)) = cli.command else {
                panic!("ACP command should parse");
            };
            assert!(args.acp);
            assert_eq!(args.acp_args.effort, Some(expected));
        }
    }

    fn run_args(args: &[&str]) -> RunArgs {
        let mut argv = vec!["basis", "spawn", "prompt"];
        argv.extend_from_slice(args);

        let Some(Command::Spawn(parsed)) = Cli::try_parse_from(argv).expect("parses").command
        else {
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
        assert!(!plain.detached);
        assert!(!plain.await_result);
        assert_eq!(plain.timeout, None);
    }

    #[test]
    fn lifecycle_flags_are_explicit_and_waits_are_bounded() {
        let spawned = run_args(&["--detached", "--await", "--timeout", "45s"]);
        assert!(spawned.detached);
        assert!(spawned.await_result);
        assert_eq!(
            spawned.timeout.map(DurationArg::duration),
            Some(std::time::Duration::from_secs(45))
        );

        let error = Cli::try_parse_from(["basis", "spawn", "prompt", "--timeout", "45s"])
            .expect_err("a wait timeout without --await has no meaning");
        assert_eq!(error.exit_code(), i32::from(EXIT_USAGE));
    }

    #[test]
    fn communication_verbs_share_one_task_handle() {
        let Some(Command::Send(send)) = Cli::try_parse_from([
            "basis",
            "send",
            "workspace/task",
            "refine the answer",
            "--await",
            "--timeout",
            "2m",
        ])
        .expect("send parses")
        .command
        else {
            panic!("send command should parse");
        };
        assert_eq!(send.task, "workspace/task");
        assert_eq!(send.message, "refine the answer");
        assert!(send.await_result);

        let Some(Command::Ask(ask)) = Cli::try_parse_from([
            "basis",
            "ask",
            "workspace/task",
            "what changed?",
            "--timeout",
            "2m",
        ])
        .expect("ask parses")
        .command
        else {
            panic!("ask command should parse");
        };
        assert_eq!(ask.task, "workspace/task");
        assert_eq!(ask.message, "what changed?");

        let Some(Command::Wait(wait)) = Cli::try_parse_from([
            "basis",
            "wait",
            "workspace/task",
            "--message",
            "message-id",
            "--timeout",
            "2m",
        ])
        .expect("wait parses")
        .command
        else {
            panic!("wait command should parse");
        };
        assert_eq!(wait.task, "workspace/task");
        assert_eq!(wait.message.as_deref(), Some("message-id"));

        assert!(matches!(
            Cli::try_parse_from(["basis", "cancel", "workspace/task"])
                .expect("cancel parses")
                .command,
            Some(Command::Cancel(_))
        ));
        assert!(matches!(
            Cli::try_parse_from(["basis", "watch", "workspace/task"])
                .expect("watch parses")
                .command,
            Some(Command::Watch(_))
        ));
        assert!(matches!(
            Cli::try_parse_from(["basis", "inbox", "workspace/task"])
                .expect("inbox parses")
                .command,
            Some(Command::Inbox(_))
        ));
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
        let error = Cli::try_parse_from(["basis", "run", "prompt", "--deadline", "30"])
            .expect_err("refused");

        assert!(
            error.to_string().contains("30m"),
            "the message must show an accepted form: {error}"
        );
    }

    #[test]
    fn fingerprint_defaults_to_the_current_directory() {
        let Some(Command::Fingerprint(args)) = Cli::try_parse_from(["basis", "fingerprint"])
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
            Cli::try_parse_from(["basis", "fingerprint", "-C", "/repo"])
                .expect("parses")
                .command
        else {
            panic!("fingerprint command should parse");
        };

        assert_eq!(args.workspace, Some(PathBuf::from("/repo")));
    }

    #[test]
    fn commands_are_on_unless_the_run_says_no_shell() {
        assert!(!run_args(&[]).no_shell, "ADR-0013: on by default");
        assert!(run_args(&["--no-shell"]).no_shell);

        let Some(Command::Serve(serve)) =
            Cli::try_parse_from(["basis", "serve", "--acp", "--no-shell"])
                .expect("parses")
                .command
        else {
            panic!("ACP command should parse");
        };
        assert!(serve.acp_args.no_shell);
    }

    #[test]
    fn serving_requires_one_explicit_transport() {
        assert!(Cli::try_parse_from(["basis", "serve"]).is_err());
        assert!(Cli::try_parse_from(["basis", "serve", "--acp", "--bridge"]).is_err());
        assert!(Cli::try_parse_from(["basis", "serve", "--acp"]).is_ok());
        assert!(Cli::try_parse_from(["basis", "serve", "--bridge"]).is_ok());
    }

    #[test]
    fn bridge_options_require_the_bridge_mode() {
        let Some(Command::Serve(acp_with_bridge_flag)) =
            Cli::try_parse_from(["basis", "serve", "--acp", "--allow-origin", "http://x"])
                .expect("clap parses the complete shape")
                .command
        else {
            panic!("serve command should parse");
        };
        assert!(acp_with_bridge_flag.has_bridge_options());

        let Some(Command::Serve(serve)) =
            Cli::try_parse_from(["basis", "serve", "--bridge", "--bind", "127.0.0.1:0"])
                .expect("bridge parses")
                .command
        else {
            panic!("serve command should parse");
        };
        assert!(serve.bridge);
        assert_eq!(serve.bridge_args.bind, Some("127.0.0.1:0".parse().unwrap()));
    }

    #[test]
    fn the_retired_grant_is_gone_rather_than_quietly_ignored() {
        // Someone with `--allow-shell` in a script should be told it no longer
        // exists, not have it silently accepted or, worse, taken as a prompt.
        let error =
            Cli::try_parse_from(["basis", "run", "prompt", "--allow-shell"]).expect_err("refused");

        assert_eq!(error.exit_code(), i32::from(EXIT_USAGE));
    }
}
