//! basis-acp — the ACP adapter: basis's explicit protocol surface.
//!
//! [ACP](https://agentclientprotocol.com) is JSON-RPC 2.0 over stdio, LSP-style
//! — the standard editors and web UIs already speak. Serving it is what makes
//! basis embeddable without basis shipping a client: Zed, JetBrains, and acp-ui
//! drive it as-is (PROPOSAL.md Bet 2, ADR-0002).
//!
//! This is an adapter over [`basis`] and nothing else: the run lifecycle,
//! the event stream and the seams are all the core's, and what is here is the
//! translation at one edge. Opt-in by dependency, so a host embedding the
//! harness in-process never compiles a JSON-RPC server it does not run
//! (ADR-0011). The binary exposes this adapter through the explicit
//! `basis serve --acp` command on stdin/stdout; a bare `basis` invocation is
//! reserved for usage and prompt shorthand (ADR-0017).
//!
//! # Shape
//!
//! - [`session_update`] maps basis's [`Event`](basis::Event) onto
//!   `session/update`. That mapping is the whole reason `Event` is basis's own
//!   type rather than a re-export of mentra's: one normalization, many surfaces.
//! - [`AcpApprover`] answers mentra's permission requests by asking the client,
//!   turning basis's existing [`Approver`](basis::Approver) seam into
//!   `session/request_permission` with no new plumbing.
//! - [`SessionModes`] is the client's permission switch: ACP session modes over
//!   basis's [`Approver`](basis::Approver) seam, applied in front of the
//!   approver so the answer can still change mid-session. The three modes are
//!   the shared [`ApprovalMode`]; basis-acp owns only their enumerable wire
//!   presentation and the read-only session's offering rule.
//! - `options` is the other switch a client holds: `session/set_config_option`
//!   over [`PreparedRun::set_model`](basis::PreparedRun::set_model) and
//!   [`set_effort`](basis::PreparedRun::set_effort), advertised per session
//!   because ACP carries these on the session responses rather than on
//!   `initialize`.
//! - `history` replays a resumed conversation, which is what separates
//!   `session/load` from `session/resume`.
//! - [`SessionRegistry`] adapts basis-host's open-conversation registry to ACP
//!   ids, keyed by mentra's persisted agent id so that `session/load` is just
//!   [`Workspace::resume`](basis::Workspace::resume).
//! - [`serve`] wires the handlers onto a connection; basis-host owns the one
//!   [`Runtime`](basis::Runtime) per process and one
//!   [`Workspace`](basis::Workspace) per configured directory (ADR-0018,
//!   ADR-0025).
//! - [`serve_stdio`] is the stdin/stdout transport, and the one place basis looks
//!   at what a peer sent before serving it.
//! - [`available_commands`] and [`from_acp`] are the two mappings between a
//!   core convention and the wire: templates become the commands a client
//!   offers, and a client's `mcpServers` become servers basis can register.

mod approver;
mod commands;
mod history;
mod mcp;
mod mode;
mod options;
mod server;
mod session;
mod stdio;
mod update;

pub use approver::AcpApprover;
pub use commands::available_commands;
pub use mcp::from_acp;
pub use mode::{ApprovalMode, ModeError, ModedApprover, SessionModes};
pub use options::{Change, ConfigError};
pub use server::{Discovery, ServeConfig, SessionSource, SessionTemplate, serve};
pub use session::{AcpSession, Interrupt, SessionRegistry};
pub use stdio::{StdioError, serve_stdio};
pub use update::session_update;
