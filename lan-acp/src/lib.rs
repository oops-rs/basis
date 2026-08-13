//! lan-acp — the ACP adapter: lan's explicit protocol surface.
//!
//! [ACP](https://agentclientprotocol.com) is JSON-RPC 2.0 over stdio, LSP-style
//! — the standard editors and web UIs already speak. Serving it is what makes
//! lan embeddable without lan shipping a client: Zed, JetBrains, and acp-ui
//! drive it as-is (PROPOSAL.md Bet 2, ADR-0002).
//!
//! This is an adapter over [`lan_core`] and nothing else: the run lifecycle,
//! the event stream and the seams are all the core's, and what is here is the
//! translation at one edge. Opt-in by dependency, so a host embedding the
//! harness in-process never compiles a JSON-RPC server it does not run
//! (ADR-0011). The binary exposes this adapter through the explicit
//! `lan serve --acp` command on stdin/stdout; a bare `lan` invocation is
//! reserved for usage and prompt shorthand (ADR-0017).
//!
//! # Shape
//!
//! - [`session_update`] maps lan's [`Event`](lan_core::Event) onto
//!   `session/update`. That mapping is the whole reason `Event` is lan's own
//!   type rather than a re-export of mentra's: one normalization, many surfaces.
//! - [`AcpApprover`] answers mentra's permission requests by asking the client,
//!   turning lan's existing [`Approver`](lan_core::Approver) seam into
//!   `session/request_permission` with no new plumbing.
//! - [`SessionModes`] is the client's permission switch: ACP session modes over
//!   lan's [`Approver`](lan_core::Approver) seam, applied in front of the
//!   approver so the answer can still change mid-session. The three modes are
//!   [`ApprovalMode`], which is lan-acp's own because an enumerable mode list
//!   is a protocol concept — the core has the trait and nothing else.
//! - `history` replays a resumed conversation, which is what separates
//!   `session/load` from `session/resume`.
//! - [`SessionRegistry`] holds the open conversations, keyed by mentra's
//!   persisted agent id so that `session/load` is just
//!   [`resume`](lan_core::run::resume).
//! - [`serve`] wires the handlers onto a connection.
//! - [`serve_stdio`] is the stdin/stdout transport, and the one place lan looks
//!   at what a peer sent before serving it.
//! - [`available_commands`] and [`from_acp`] are the two mappings between a
//!   core convention and the wire: templates become the commands a client
//!   offers, and a client's `mcpServers` become servers lan can register.

mod approver;
mod commands;
mod history;
mod mcp;
mod mode;
mod server;
mod session;
mod stdio;
mod update;

pub use approver::AcpApprover;
pub use commands::available_commands;
pub use mcp::from_acp;
pub use mode::{ApprovalMode, ModeError, ModedApprover, SessionModes};
pub use server::{ServeConfig, SessionSource, serve};
pub use session::{AcpSession, SessionRegistry};
pub use stdio::{StdioError, serve_stdio};
pub use update::session_update;
