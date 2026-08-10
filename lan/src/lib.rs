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

pub mod acp;
pub mod approval;
pub mod branch;
pub mod bridge;
pub mod context;
pub mod event;
pub mod hooks;
pub mod mcp;
mod paths;
pub mod provider;
pub mod run;
pub mod shell;
pub mod skills;
pub mod store;
pub mod templates;
pub mod watch;

pub use approval::{
    AllowAll, ApprovalDecision, ApprovalPolicy, ApprovalRequest, Approver, DenyAll,
    TerminalApprover,
};
pub use branch::{BranchError, EntryKind, TranscriptEntry};
pub use bridge::{Bridge, BridgeConfig, BridgeError};
pub use context::{
    ContextConfig, ContextDocument, ContextError, ContextScope, DEFAULT_CONTEXT_FILE,
    WorkspaceContext,
};
pub use event::{
    EVENT_SCHEMA_VERSION, Event, EventLine, JsonlWriter, RunOutcome, SkillSummary, TemplateSummary,
};
pub use hooks::{
    HOOK_SCHEMA_VERSION, HookCall, HookConfigError, HookOutcome, HookRequest, HookResponse,
    HookRunner, HookSpec, HooksConfig, HooksSource, OnFailure,
};
pub use mcp::{
    DEFAULT_GLOBAL_MCP_FILE, DEFAULT_WORKSPACE_MCP_FILE, McpConfig, McpError, McpServer, McpSource,
};
pub use run::{
    CollectingSink, Effort, EventSink, FnSink, NullSink, PreparedRun, RunConfig, RunContext,
    RunError, RunReport, TurnOptions, resume, run, run_with_approver,
};
pub use shell::ShellAccess;
pub use skills::{SkillsConfig, SkillsSource};
// `store::list` keeps its module: at the crate root `list` would not say what
// is being listed, and `PersistedSession` is only meaningful beside it.
pub use store::PersistedSession;
// `available_commands` stays at `templates::available_commands`: at the crate
// root the name would not say available commands of *what*.
pub use templates::{Template, TemplateError, TemplateSource, TemplatesConfig};
// `watch` the function sits beside `watch` the module the same way `run` does:
// the scheduler is one call, and everything it is built from stays namespaced.
pub use watch::{
    CollectingWatchSink, Interval, IntervalError, IterationBounds, Shutdown, WATCH_SCHEMA_VERSION,
    WatchConfig, WatchError, WatchEvent, WatchJsonlWriter, WatchSink, WatchSummary, watch,
};
