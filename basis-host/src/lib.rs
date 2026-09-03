//! basis-host — host-side basis machinery shared by adapters.
//!
//! `basis` is the SDK and owns conventions, the run surface, and the durable
//! event schema. What lives here is the next layer up that a long-lived host
//! needs regardless of protocol. The first shared seam is approval policy and
//! its session-scoped remembered answers.
//!
//! The layer line is ADR-0025's application of ADR-0011: this crate depends on
//! [`basis`] and nothing heavier. Protocol bindings stay in `basis-acp`;
//! terminal bindings stay in `basis-cli`; durable task coordination stays in
//! `basis-tasks`.

mod approval;
mod config;
mod session;
mod source;
mod timestamp;

pub use approval::{ApprovalPolicy, PolicyApprover, PolicyGate, SessionApproval};
pub use config::{Discovery, SessionSource, SessionTemplate};
pub use session::{HostSession, Interrupt};
pub use source::ConfiguredSource;
pub use timestamp::rfc3339;
