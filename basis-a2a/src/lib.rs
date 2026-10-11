//! The A2A server over basis: any basis agent — or any host's own tasks —
//! reachable by any [A2A](https://a2a-protocol.org/v1.0.0/specification)
//! client.
//!
//! ADR-0028 made A2A basis's agent-to-agent protocol. ACP is how a person's
//! editor drives an agent and MCP is how an agent calls tools; A2A is how
//! one agent hands work to another whose insides it cannot see. This crate
//! is the half that *receives* work. The half that sends it — fetching a
//! card, handing a remote agent a task as `spawn "@<agent> …"` (ADR-0029) —
//! is mentra's `a2a` client, configured through basis's `.basis/a2a.json`.
//!
//! # Who runs a task
//!
//! By default, `basis-tasks`, and there is no second lifecycle here. Every
//! accepted message is one `basis-tasks` spawn in the served workspace, and
//! what A2A reports is read straight off it — see the `project` module for
//! the table. So a served task is as durable as any other: it survives this
//! server restarting, `basis list` shows it, and `basis watch` replays it. A
//! `contextId` is a conversation; a follow-up in one continues it with
//! `Continuation::Named`, and `basis-tasks`' own one-executor-per-conversation
//! rule is what refuses a second message while the first is still running.
//!
//! A host that already runs tasks of its own serves those instead
//! (ADR-0033): it implements [`TaskHost`] — start, standing, cancel, settled,
//! progress — and opens the service with [`ServeConfig::hosted`]. Everything
//! else on this page is the same for its tasks. The server mints each A2A id
//! and keeps the host's beside it; a follow-up hands the host the id of the
//! latest task in its context as [`HostedStart::continues`]; a completed
//! task's parts, text or data, are its one `result` artifact; and a restart
//! awaits every unsettled task again through [`TaskHost::settled`], never
//! [`TaskHost::start`]. Asking the caller is a `basis-tasks` mechanism, so a
//! hosted config refuses it, as it refuses the shell and the run defaults.
//!
//! ```no_run
//! # async fn serve(host: std::sync::Arc<dyn basis_a2a::TaskHost>) -> Result<(), Box<dyn std::error::Error>> {
//! use basis_a2a::{A2aServer, A2aService, AgentIdentity, BearerToken, ServeConfig};
//!
//! // `host` is the embedder's own `impl TaskHost`.
//! let identity = AgentIdentity::new("librarian", "Answers from its own catalog.");
//! let token = BearerToken::new(std::env::var("A2A_TOKEN")?).ok_or("A2A_TOKEN is empty")?;
//! let config = ServeConfig::hosted(identity, "/var/lib/librarian/a2a", host).with_token(token);
//! let server = A2aServer::bind("0.0.0.0:8080".parse()?, A2aService::open(config)?).await?;
//! server.serve().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # What is served
//!
//! A2A v1.0 in full over two bindings, every method on each (ADR-0030):
//! JSON-RPC at the endpoint's root and HTTP+JSON under [`HTTP_JSON_PATH`],
//! both with SSE streaming, both listed on the Agent Card at
//! [`AGENT_CARD_PATH`] — JSON-RPC first. gRPC is not served. Each binding
//! only parses; one service answers both, so they cannot drift apart.
//!
//! - **Principals.** Named bearer tokens ([`ServeConfig::with_principal`]);
//!   every task belongs to the principal that created it, and every
//!   task-scoped call sees only the caller's own — another's is "not found".
//!   A loopback server with no token is one anonymous principal, and
//!   anything but a loopback address needs a [`BearerToken`].
//! - **Parts.** Text, data (handed to the model as JSON) and inline files of
//!   a textual media type. A file by URL or binary bytes is refused: the
//!   server never fetches what a caller names.
//! - **Push notifications**, when the operator turns them on: configs stored
//!   beside the task, every status change delivered at least once, across
//!   restarts, only to webhooks that resolve to public addresses.
//! - **The extended card**, when the operator gives one, to authenticated
//!   callers.
//! - **Asking the caller.** [`InboundApproval::AskCaller`] puts a
//!   consequential call to whoever sent the task, as `INPUT_REQUIRED`
//!   (`docs/a2a/approval-v1.md`). [`InboundApproval`] still has no variant
//!   that asks a person at this server: there is none.
//! - **Hops.** A message's hop and deadline (`docs/a2a/hops-v1.md`) are read,
//!   past the hop cap refused, and the caller's deadline clamps the task's.
//!
//! The card declares each optional feature when, and only when, it is on. A
//! served agent has no shell unless the operator grants one.
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
mod gate;
mod host;
mod http;
mod index;
mod listener;
mod ops;
mod project;
mod push;
mod rest;
mod rpc;
mod service;
mod stream;

pub use a2a;
pub use card::AGENT_CARD_PATH;
pub use config::{
    AgentIdentity, BearerToken, DEFAULT_LIVE_TASK_QUOTA, DEFAULT_PRINCIPAL, ExtendedCard,
    InboundApproval, Principal, PrincipalError, RunDefaults, ServeConfig,
};
pub use host::{HostError, HostedProgress, HostedStanding, HostedStart, TaskHost};
pub use http::{A2aServer, BindError, router};
pub use rest::HTTP_JSON_PATH;
pub use service::{A2aService, DEFAULT_HOP_CAP, DEFAULT_STREAM_CAP, OpenError};
