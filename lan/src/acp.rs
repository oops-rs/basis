//! The ACP server: lan's front door.
//!
//! [ACP](https://agentclientprotocol.com) is JSON-RPC 2.0 over stdio, LSP-style
//! — the standard editors and web UIs already speak. Serving it is what makes
//! lan embeddable without lan shipping a client: Zed, JetBrains, and acp-ui
//! drive it as-is (PROPOSAL.md Bet 2, ADR-0002).
//!
//! Running `lan` with no subcommand serves this on stdin/stdout, because the
//! embedded case is the primary case.
//!
//! # Shape
//!
//! - [`update`] maps lan's [`Event`](crate::Event) onto `session/update`. That
//!   mapping is the whole reason `Event` is lan's own type rather than a
//!   re-export of mentra's: one normalization, many surfaces.
//! - [`approver`] answers mentra's permission requests by asking the client,
//!   turning lan's existing [`Approver`](crate::Approver) seam into
//!   `session/request_permission` with no new plumbing.
//! - [`session`] holds the open conversations, keyed by mentra's persisted
//!   agent id so that `session/load` is just [`resume`](crate::run::resume).
//! - [`server`] wires the handlers onto a connection.

mod approver;
mod server;
mod session;
mod update;

pub use approver::AcpApprover;
pub use server::{ServeConfig, SessionSource, serve, serve_stdio};
pub use session::{AcpSession, SessionRegistry};
pub use update::session_update;
