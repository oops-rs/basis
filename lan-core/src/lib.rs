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
//!    reached from the binary with the explicit `lan serve --acp` command.
//! 3. **Subprocess**: `lan spawn --json` streams JSONL events for scripts and CI;
//!    `lan run` remains a compatibility alias.
//!
//! The core has no opinions: task-specific behavior enters through data — the
//! prompt, the workspace (AGENTS.md, skills, templates, `.mcp.json`), and
//! config — never through code in this crate.
//!
//! # Two shapes
//!
//! [`Workspace`] is the SDK's shape (ADR-0010). Opening one settles everything
//! that belongs to a repository rather than to a prompt — context documents,
//! the credential and the resolved model, skills, templates, hooks, MCP
//! connections — and then mints runs from it without doing any of that again:
//!
//! ```no_run
//! # async fn example() -> Result<(), lan_core::RunError> {
//! let workspace = lan_core::Workspace::open("/repo").await?;
//! let mut run = workspace.prepare("what does this repo do?")?;
//! let report = run.execute(lan_core::CollectingSink::default()).await?;
//! # let _ = report;
//! # Ok(())
//! # }
//! ```
//!
//! [`run`](run()) and its neighbours are the one-prompt shape: a
//! [`RunConfig`] in, a report out, with a workspace opened and dropped around
//! it. They are wrappers over the same path — [`RunConfig::split`] is the seam
//! — so nothing behaves differently for having gone through one.
//!
//! # Features
//!
//! - **`mcp`** (default) — `.mcp.json` discovery and the MCP binding of the
//!   tool contract. Built without it, the crate has no MCP concept at all: no
//!   `McpConfig` on a run, no servers registered, and a run header that names
//!   none (ADR-0012). Custom tools remain, because MCP was only ever one of the
//!   ways to reach them.

pub mod approval;
pub mod branch;
pub mod budget;
pub mod context;
pub mod event;
pub mod fingerprint;
pub mod hooks;
pub mod lifecycle;
#[cfg(feature = "mcp")]
pub mod mcp;
mod paths;
pub mod provider;
pub mod run;
pub mod shell;
pub mod skills;
pub mod store;
pub mod templates;
pub mod tools;
pub mod workspace;

pub use approval::{
    AllowAll, ApprovalAnswer, ApprovalDecision, ApprovalGate, ApprovalRequest, Approver, DenyAll,
};
pub use branch::{BranchError, EntryKind, TranscriptEntry};
pub use budget::BudgetPool;
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
    HOOK_SCHEMA_VERSION, HookCall, HookConfigError, HookEvent, HookOutcome, HookRequest,
    HookResponse, HookRunner, HookSpec, HooksConfig, HooksSource, Interceptor, InterceptorError,
    OnFailure,
};
pub use lifecycle::{
    Cancellation, LifecycleError, Supervisor, TaskContext, TaskHandle, TaskId, TaskState, WaitError,
};
#[cfg(feature = "mcp")]
pub use mcp::{
    DEFAULT_GLOBAL_MCP_FILE, DEFAULT_WORKSPACE_MCP_FILE, McpConfig, McpError, McpServer, McpSource,
};
pub use run::{
    Bound, CancellationToken, CollectingSink, Effort, EventFanIn, EventSink, FnSink, MergedEvents,
    NullSink, OutputReport, OutputSpec, PreparedRun, RunConfig, RunContext, RunError, RunReport,
    RunUsage, TaggedEvent, TaggedSink, TurnOptions, resume, run, run_with_approver,
};
pub use shell::ShellAccess;
// Mentra's, deliberately. These three are the types lan's own surface asks a
// caller to *name* — a model to resolve, a provider to prefer, a token to stop
// a turn with — and re-exporting them is what keeps that from meaning "add
// mentra to your manifest, pinned to whatever version lan-core happens to
// resolve". A skew there is a type error with no explanation in it. Everything
// else mentra owns stays behind `mentra::`, where an embedder that wants the
// runtime itself already is.
pub use mentra::{BuiltinProvider, ModelSelector};
// The attribute both of lan's async traits make an implementor spell:
// `Approver` and `Interceptor` are `#[async_trait]`, so without this line
// writing either impl means adding `async-trait` to the host's own manifest —
// a dependency lan's docs used to ask for without saying so. Same rule as the
// mentra types above, applied to a macro.
pub use async_trait::async_trait;
pub use skills::{SkillsConfig, SkillsSource};
// `store::list` keeps its module: at the crate root `list` would not say what
// is being listed, and `PersistedSession` is only meaningful beside it.
pub use store::PersistedSession;
pub use templates::{Template, TemplateError, TemplateSource, TemplatesConfig};
// `tools::spawn` keeps its module: `SpawnTool` at the crate root would sit
// beside a dozen types that are not tools, and the name an operator writes in a
// rule or a hook is only meaningful next to the tool it names.
pub use tools::SpawnTool;
pub use workspace::{RunSpec, Workspace, WorkspaceBuilder};
