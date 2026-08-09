//! lan — a full-functional, embeddable agent harness built on [Mentra](https://github.com/oops-rs/mentra).
//!
//! Library first, binary second: this crate is the in-process SDK; the `lan`
//! binary is a thin shell over it. Embedding surfaces, in order of preference:
//!
//! 1. **In-process**: depend on this crate (Rust hosts).
//! 2. **ACP**: `lan` with no subcommand serves the Agent Client Protocol
//!    (JSON-RPC 2.0 over stdio) for editors and web UIs.
//! 3. **Subprocess**: `lan run --json` streams JSONL events for scripts and CI.
//!
//! The core has no opinions: task-specific behavior enters through data — the
//! prompt, the workspace (AGENTS.md, skills, templates, `.mcp.json`), and
//! config — never through code in this crate.

pub mod context;
pub mod event;
pub mod provider;
pub mod run;
pub mod skills;

pub use context::{
    ContextConfig, ContextDocument, ContextError, ContextScope, DEFAULT_CONTEXT_FILE,
    WorkspaceContext,
};
pub use event::{EVENT_SCHEMA_VERSION, Event, EventLine, JsonlWriter, RunOutcome};
pub use run::{
    CollectingSink, EventSink, FnSink, NullSink, PreparedRun, RunConfig, RunContext, RunError,
    RunReport, run,
};
pub use skills::{SkillsConfig, SkillsSource};
