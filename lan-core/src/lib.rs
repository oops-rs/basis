//! lan-core — the in-process SDK of [lan](https://github.com/oops-rs/lan), an
//! embeddable agent harness built on [Mentra](https://github.com/oops-rs/mentra).
//!
//! This crate is the harness itself: workspace discovery (AGENTS.md, skills,
//! templates, `.mcp.json`), the run lifecycle, one event stream, and the seams
//! a host plugs into (approval, hooks). It carries no protocol, no transport
//! and no terminal code, so an embedder's dependency graph states what they
//! actually use (ADR-0011).
//!
//! Embedding surfaces, in order of preference:
//!
//! 1. **In-process**: depend on this crate (Rust hosts).
//! 2. **ACP**: `lan-acp` serves the Agent Client Protocol (JSON-RPC 2.0 over
//!    stdio) over this crate's event stream, for editors and web UIs. It is
//!    what the `lan` binary runs with no subcommand.
//! 3. **Subprocess**: `lan run --json` streams JSONL events for scripts and CI.
//!
//! The core has no opinions: task-specific behavior enters through data — the
//! prompt, the workspace (AGENTS.md, skills, templates, `.mcp.json`), and
//! config — never through code in this crate.

pub mod approval;
pub mod branch;
pub mod context;
pub mod event;
pub mod fingerprint;
pub mod hooks;
pub mod mcp;
mod paths;
pub mod provider;
pub mod run;
pub mod shell;
pub mod skills;
pub mod store;
pub mod templates;

pub use approval::{
    AllowAll, ApprovalDecision, ApprovalPolicy, ApprovalRequest, Approver, DenyAll,
};
pub use branch::{BranchError, EntryKind, TranscriptEntry};
pub use context::{
    ContextConfig, ContextDocument, ContextError, ContextScope, DEFAULT_CONTEXT_FILE,
    WorkspaceContext,
};
pub use event::{
    EVENT_SCHEMA_VERSION, Event, EventLine, JsonlWriter, RunOutcome, SkillSummary, TemplateSummary,
};
// `fingerprint::snapshot` keeps its module: at the crate root `snapshot` would
// not say a snapshot of what, and the two types beside it are only meaningful
// as its result.
pub use fingerprint::{Fingerprint, Snapshot};
pub use hooks::{
    HOOK_SCHEMA_VERSION, HookCall, HookConfigError, HookOutcome, HookRequest, HookResponse,
    HookRunner, HookSpec, HooksConfig, HooksSource, OnFailure,
};
pub use mcp::{
    DEFAULT_GLOBAL_MCP_FILE, DEFAULT_WORKSPACE_MCP_FILE, McpConfig, McpError, McpServer, McpSource,
};
pub use run::{
    Bound, CollectingSink, Effort, EventSink, FnSink, NullSink, PreparedRun, RunConfig, RunContext,
    RunError, RunReport, TurnOptions, resume, run, run_with_approver,
};
pub use shell::ShellAccess;
pub use skills::{SkillsConfig, SkillsSource};
// `store::list` keeps its module: at the crate root `list` would not say what
// is being listed, and `PersistedSession` is only meaningful beside it.
pub use store::PersistedSession;
pub use templates::{Template, TemplateError, TemplateSource, TemplatesConfig};
