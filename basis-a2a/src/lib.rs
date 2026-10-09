//! The A2A server over basis: any basis agent, reachable by any
//! [A2A](https://a2a-protocol.org/v1.0.0/specification) client.
//!
//! ADR-0028 made A2A basis's agent-to-agent protocol. ACP is how a person's
//! editor drives an agent and MCP is how an agent calls tools; A2A is how
//! one agent hands work to another whose insides it cannot see. This crate
//! is the half that *receives* work. The half that sends it — fetching a
//! card, calling a remote agent as a tool — is mentra's `a2a` client,
//! configured through basis's `.basis/a2a.json`.
//!
//! # An A2A task is a basis task
//!
//! There is no second lifecycle here. Every accepted message is one
//! `basis-tasks` spawn in the served workspace, and what A2A reports is read
//! straight off it — see [`project`](crate::project) for the table. So a
//! served task is as durable as any other: it survives this server
//! restarting, `basis list` shows it, and `basis watch` replays it. A
//! `contextId` is a conversation; a follow-up in one continues it with
//! `Continuation::Named`, and `basis-tasks`' own one-executor-per-conversation
//! rule is what refuses a second message while the first is still running.
//!
//! # What is served
//!
//! The JSON-RPC binding of protocol version 1.0, with SSE streaming, and the
//! Agent Card at [`AGENT_CARD_PATH`]. Push notifications, the extended card,
//! non-text parts and replies to a task awaiting input are each refused with
//! the spec's own error, and the card claims none of them (ADR-0028 §5;
//! proposal 0005 has what would change that).
//!
//! A served agent never prompts: [`InboundApproval`] has no variant that
//! would. It has no shell unless the operator grants one. And anything but
//! a loopback address needs a [`BearerToken`].
//!
//! ```no_run
//! # async fn serve() -> Result<(), Box<dyn std::error::Error>> {
//! use basis_a2a::{A2aServer, A2aService, AgentIdentity, ServeConfig};
//!
//! let identity = AgentIdentity::new("helper", "Answers questions about this repository.");
//! let service = A2aService::open(ServeConfig::new("/path/to/repo", identity))?;
//! let server = A2aServer::bind("127.0.0.1:8080".parse()?, service).await?;
//! println!("serving {}", server.endpoint());
//! server.serve().await?;
//! # Ok(())
//! # }
//! ```

mod card;
mod config;
mod http;
mod index;
mod project;
mod rpc;
mod service;
mod stream;

pub use a2a;
pub use card::AGENT_CARD_PATH;
pub use config::{AgentIdentity, BearerToken, InboundApproval, RunDefaults, ServeConfig};
pub use http::{A2aServer, BindError, router};
pub use service::{A2aService, OpenError};
