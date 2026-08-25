//! basis — the in-process SDK of [basis](https://github.com/oops-rs/basis), an
//! embeddable agent harness built on [Mentra](https://github.com/oops-rs/mentra).
//!
//! This crate is the harness itself: workspace discovery (AGENTS.md, skills,
//! templates, `.basis/tools.json`, `.mcp.json`), the run lifecycle, one event
//! stream, and the seams a host plugs into (approval, hooks). It carries no
//! protocol, no transport and no terminal code, so an embedder's dependency
//! graph states what they actually use (ADR-0011).
//!
//! Embedding surfaces, in order of preference:
//!
//! 1. **In-process**: depend on this crate (Rust hosts).
//! 2. **ACP**: `basis-acp` serves the Agent Client Protocol (JSON-RPC 2.0 over
//!    stdio) over this crate's event stream, for editors and web UIs. It is
//!    reached from the binary with the explicit `basis serve --acp` command.
//! 3. **Subprocess**: `basis spawn --json` streams JSONL events for scripts and CI;
//!    `basis run` remains a compatibility alias.
//!
//! The core has no opinions: task-specific behavior enters through data — the
//! prompt, the workspace (AGENTS.md, skills, templates, `.mcp.json`), and
//! config — never through code in this crate.
//!
//! # Three shapes
//!
//! [`Workspace`] is the SDK's shape (ADR-0010). Opening one settles everything
//! that belongs to a repository rather than to a prompt — context documents,
//! the resolved model, skills, templates, hooks, MCP connections — and then
//! mints runs from it without doing any of that again:
//!
//! ```no_run
//! # async fn example() -> Result<(), basis::RunError> {
//! let workspace = basis::Workspace::open("/repo").await?;
//! let mut run = workspace.prepare("what does this repo do?")?;
//! let report = run.execute(basis::CollectingSink::default()).await?;
//! # let _ = report;
//! # Ok(())
//! # }
//! ```
//!
//! [`run`](run()) and [`run_with_approver`] are the one-prompt shape: a path
//! and a prompt in, a report out, with a workspace opened and dropped around
//! it. They are wrappers over the same open-and-prepare — so nothing behaves
//! differently for having gone through one.
//!
//! [`Runtime`] is the process's shape (ADR-0018), and only the N-repository
//! host sees it: what changes when the host changes — provider and credential,
//! the history store, the host's interceptors — is built once and every
//! workspace borrows it through an `Arc`. `Workspace::open` builds a private
//! one behind the scenes, so the other two shapes never name it.
//!
//! # Features
//!
//! - **`mcp`** (default) — `.mcp.json` discovery and the MCP binding of the
//!   tool contract. Built without it, the crate has no MCP concept at all: no
//!   `McpConfig` on a run, no servers registered, and a run header that names
//!   none (ADR-0012). Custom tools remain, because MCP was only ever one of the
//!   ways to reach them: [`tools::declared`] is core, and a workspace's
//!   `.basis/tools.json` works in a build that has never heard of MCP.

pub mod approval;
pub mod branch;
pub mod budget;
pub mod compaction;
pub mod config;
pub mod context;
pub mod error;
pub mod event;
mod expand;
pub mod fingerprint;
mod frontmatter;
pub mod hooks;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod memory;
mod paths;
pub mod provider;
pub mod run;
pub mod runtime;
pub mod shell;
pub mod skills;
pub mod store;
mod subprocess;
pub mod templates;
pub mod tools;
pub mod workspace;

// `ToolSideEffectLevel` is mentra's, and comes to the root under the same rule
// as `CancellationToken` below: `is_consequential` and
// `ApprovalRequest::side_effect_level` both make a caller name it, and a policy
// written against it should not cost the host a mentra dependency of its own.
pub use approval::{
    AllowAll, ApprovalAnswer, ApprovalDecision, ApprovalGate, ApprovalRequest, Approver, DenyAll,
    ToolSideEffectLevel,
};
pub use branch::{BranchError, EntryKind, TranscriptEntry};
pub use budget::BudgetPool;
// Beside the other things a workspace is built *with*: what a run's history
// keeps is set on `WorkspaceBuilder`, so the type naming it belongs where a
// host already looks for `ShellAccess` and `ContextConfig`.
pub use compaction::Compaction;
pub use config::{
    CONFIG_SCHEMA_VERSION, Config, ConfigError, DEFAULT_GLOBAL_CONFIG_FILE,
    DEFAULT_WORKSPACE_CONFIG_FILE, Setting,
};
pub use context::{
    ContextConfig, ContextDocument, ContextError, ContextScope, DEFAULT_CONTEXT_FALLBACK_FILE,
    DEFAULT_CONTEXT_FILE, SystemPrompt, WorkspaceContext,
};
pub use error::RunError;
pub use event::{
    EVENT_SCHEMA_VERSION, Event, EventLine, JsonlWriter, Mutability, RunOutcome, SkillSummary,
    TemplateSummary,
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
#[cfg(feature = "mcp")]
pub use mcp::{
    DEFAULT_GLOBAL_MCP_FILE, DEFAULT_WORKSPACE_MCP_FILE, McpConfig, McpError, McpServer, McpSource,
};
// The memory *configuration* comes to the root, beside its siblings
// `SkillsConfig` and `TemplatesConfig` — a host pointing basis at different
// memory roots is doing the same thing it does for those. `Memory` and its
// kind stay in `memory`, next to the convention they only make sense beside.
pub use memory::{MemoryConfig, WorkspaceMemoryRoot};
pub use run::{
    Bound, Bounds, CancellationToken, CollectingSink, Compacted, ContentBlock, Effort, EventFanIn,
    EventSink, FnSink, MergedEvents, ModelInfo, NullSink, OutputReport, OutputSpec, PreparedRun,
    PromptPart, ReasoningChange, ReasoningOptions, RoundAdjustment, RoundBoundary, RoundContext,
    RoundDecision, RoundStrategy, RoundToolResult, RunContext, RunReport, RunUsage, TaggedEvent,
    TaggedSink, TurnOptions, run, run_with_approver,
};
pub use runtime::{Runtime, RuntimeBuilder};
pub use shell::ShellAccess;
// Mentra's, deliberately. These are the types basis's own surface asks a
// caller to *name* — a model to resolve, a provider to prefer, a provider to
// *be* ([`RuntimeBuilder::with_provider_instance`]), a token to stop a turn
// with — and re-exporting them is what keeps that from meaning "add mentra to
// your manifest, pinned to whatever version basis happens to resolve". A skew
// there is a type error with no explanation in it. Everything else mentra
// owns stays behind `mentra::`, where an embedder that wants the runtime
// itself already is — except what implementing `Provider` touches, which is
// [`runtime`]'s provider-authoring re-export, beside the executor set and for
// its reason.
pub use mentra::{BuiltinProvider, ModelSelector, Provider};
// The attribute both of basis's async traits make an implementor spell:
// `Approver` and `Interceptor` are `#[async_trait]`, so without this line
// writing either impl means adding `async-trait` to the host's own manifest —
// a dependency basis's docs used to ask for without saying so. Same rule as the
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
// The declared binding's *configuration* comes to the root, beside its
// siblings `HooksConfig`, `SkillsConfig` and `TemplatesConfig` — a host
// pointing basis at a different manifest is doing the same thing it does for
// those, and `DeclaredToolError` is what a failed open hands back. The tool
// type and the declaration stay in `tools::declared`, next to the format they
// only make sense beside.
pub use tools::declared::{
    DEFAULT_GLOBAL_TOOLS_FILE, DEFAULT_WORKSPACE_TOOLS_FILE, DeclaredToolError,
    TOOLS_SCHEMA_VERSION, ToolsConfig, ToolsSource,
};
pub use workspace::{RunSpec, ToolRoster, Workspace, WorkspaceBuilder};
