//! The ACP server, driven by a real ACP client.
//!
//! Both ends are the genuine article — basis's server and the
//! `agent-client-protocol` client — joined by `Channel::duplex()` instead of a
//! pipe. Every request is really serialized, dispatched, and answered; only the
//! transport and the model are substituted. No subprocess, no network, no cost.
//!
//! The permission test is the important one. ACP handler closures run inside
//! the dispatch loop and block it until they return, so a `session/prompt` that
//! awaited the client's permission answer inline would deadlock forever: the
//! answer arrives on the loop that is waiting for it. `session/load` is the
//! same hazard from the other side: it reads a transcript behind the lock a
//! running turn holds. Every test here is wrapped in a timeout, because both
//! failures present as a hang rather than an assertion.
//!
//! One test crate rather than several, which is what a directory with a
//! `main.rs` buys: `client` and `source` are the two substituted ends and
//! every test needs both, so a second `tests/*.rs` would be a second crate
//! compiling and linking its own copy of them. The tests divide instead by
//! what a client is doing — prompting in `turns`, deciding a consequential
//! call in `permission`, keeping a session in `sessions`, running out of an
//! allowance in `bounds`, and asking what is there in `discovery`.

mod client;
mod source;

mod bounds;
mod discovery;
mod permission;
mod sessions;
mod turns;
