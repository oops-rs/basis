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
//! - [`mode`] is the client's permission switch: ACP session modes over lan's
//!   [`ApprovalPolicy`](crate::ApprovalPolicy), applied in front of the
//!   approver so it can still change mid-session.
//! - [`history`] replays a resumed conversation, which is what separates
//!   `session/load` from `session/resume`.
//! - [`session`] holds the open conversations, keyed by mentra's persisted
//!   agent id so that `session/load` is just [`resume`](crate::run::resume).
//! - [`server`] wires the handlers onto a connection.
//! - [`stdio`] is the stdin/stdout transport, and the one place lan looks at
//!   what a peer sent before serving it — see [`serve_stdio`].

mod approver;
mod history;
mod mode;
mod server;
mod session;
mod stdio;
mod update;

pub use approver::AcpApprover;
pub use mode::{ModeError, ModedApprover, SessionModes};
pub use server::{ServeConfig, SessionSource, serve};
pub use session::{AcpSession, SessionRegistry};
pub use stdio::{StdioError, serve_stdio};
pub use update::session_update;
