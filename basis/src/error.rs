//! What can go wrong, named once at the root.
//!
//! [`RunError`] is the crate's universal error — opening a workspace,
//! preparing a run, driving one — and it used to live in
//! [`run`](mod@crate::run), where history put it. Four of that module's in-edges
//! (the store, the event mapping, the runtime, the budget) imported nothing
//! from `run` *but* this type, which manufactured four of the crate's import
//! cycles out of one name. At the root, an error is something every module
//! may name without owing the run module anything; `run` re-exports it, so
//! `basis::run::RunError` still reads.

use thiserror::Error;

#[cfg(feature = "mcp")]
use crate::mcp::McpError;
use crate::{context::ContextError, provider::ProviderError};

/// Anything that can go wrong opening a workspace, preparing a run, or driving
/// one.
///
/// One error type across all three, rather than a `WorkspaceError` beside it:
/// opening a workspace exists to prepare runs, and every failure listed here is
/// a failure a caller of [`run`](crate::run()) has always been able to receive.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunError {
    #[error("prompt is empty")]
    EmptyPrompt,

    /// The shared allowance this turn draws on has nothing left.
    ///
    /// A decision rather than a failure of the work, which is why it is its own
    /// variant: a caller fanning out over a [`BudgetPool`](crate::BudgetPool)
    /// stops minting on this, where it would retry on a provider error. Raised
    /// before the prompt is sent and before the stream opens, so the
    /// conversation is left exactly as it was.
    #[error("the shared token budget is spent: {spent} of {limit} tokens reported")]
    BudgetExhausted { limit: u64, spent: u64 },

    #[error("no session to resume")]
    NoSuchSession,

    /// The directory named for this runtime's conversations holds a basis
    /// ≤0.6 store — mentra's SQLite database — which this build neither links
    /// nor migrates (ADR-0023).
    ///
    /// basis's own words rather than mentra's: the upstream file store
    /// detects the same file and names its `store-sqlite` cargo feature,
    /// which is advice for a mentra embedder, not for the person whose
    /// conversations are in the file. Raised before any file store is opened
    /// in the directory, because an empty store beside the database would
    /// read as every conversation being lost. See
    /// [`store`](crate::store)'s module docs for where the check runs.
    #[error(
        "'{}' holds conversations from basis 0.6 or earlier (runtime.sqlite, a SQLite \
         database); this build persists conversations as plain files and the database is \
         not migrated. To continue an old conversation, use basis 0.6. To start new work \
         here, point the store somewhere fresh (`RuntimeBuilder::with_store_dir`; for the \
         CLI, `BASIS_DATA_DIR`) or move the old store directory aside",
        dir.display()
    )]
    LegacyStore {
        /// The store directory holding the pre-0.7 database.
        dir: std::path::PathBuf,
    },

    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),

    #[error(transparent)]
    Context(#[from] ContextError),

    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error("runtime error: {0}")]
    Runtime(#[from] mentra::error::RuntimeError),

    /// A typed turn answered, but not in the shape that was asked for.
    ///
    /// Separate from [`Runtime`](Self::Runtime) because the two call for
    /// different reactions and basis can tell them apart honestly: this one is
    /// basis's own verdict. The typed path asks mentra for the raw payload and
    /// deserializes it here, so a value that does not fit `T` is a schema or
    /// prompt problem — retry with a clearer schema — while a provider failure
    /// is not. The exchange stays in the session's transcript either way; see
    /// [`PreparedRun::output`](crate::PreparedRun::output).
    #[error("the run's output did not match the requested type: {0}")]
    OutputMismatch(#[source] serde_json::Error),

    #[error("failed to write an event: {0}")]
    Sink(#[from] std::io::Error),

    #[error("event forwarding task failed: {0}")]
    Forwarder(#[from] tokio::task::JoinError),

    #[error("failed to load skills: {0}")]
    Skills(#[from] mentra::SkillLoadError),

    #[error(transparent)]
    #[cfg(feature = "mcp")]
    Mcp(#[from] McpError),

    #[error("failed to load prompt templates: {0}")]
    Templates(#[from] crate::templates::TemplateError),

    #[error("failed to load memories: {0}")]
    Memory(#[from] crate::memory::MemoryError),

    /// The blocking thread [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open)
    /// runs memory discovery on (roots, per-file reads, `canonicalize`)
    /// panicked or was cancelled before it returned (whole-wave review, G7).
    ///
    /// Not `#[from]`: [`Forwarder`](Self::Forwarder) already claims
    /// `tokio::task::JoinError` for the event-forwarding task, and thiserror
    /// cannot generate two `From` impls for one source type on one enum — so
    /// this is built by hand at the one call site that needs it.
    #[error("memory discovery failed: {0}")]
    MemoryDiscovery(#[source] tokio::task::JoinError),

    #[error("failed to load hooks: {0}")]
    Hooks(#[from] crate::hooks::HookConfigError),

    #[error("failed to load declared tools: {0}")]
    Tools(#[from] crate::tools::declared::DeclaredToolError),

    /// A command target name that cannot be routed on
    /// ([`RuntimeBuilder::with_command_target`](crate::RuntimeBuilder::with_command_target),
    /// ADR-0021).
    ///
    /// Raised by `build` rather than by a panic at the registering call,
    /// because that is where this builder answers every other piece of bad
    /// input — an unattributed credential is refused by `provider::resolve_with`
    /// at exactly the same moment. A host reading its targets out of its own
    /// configuration can then report a bad name the way it reports every other
    /// bad setting, instead of losing the process to it.
    #[error("`{name}` cannot be a command target name: {reason}")]
    CommandTarget { name: String, reason: String },

    /// A host tool ([`RuntimeBuilder::with_tool`](crate::RuntimeBuilder::with_tool))
    /// whose name collides with one basis already registered — `spawn`, a
    /// mentra builtin, or an earlier host tool on the same builder (decision
    /// D5d).
    ///
    /// mentra's registry is a map and its plain `with_tool` *replaces*, so
    /// without this a host tool named `spawn` would silently take over the
    /// name and inherit every rule an operator ever wrote about commands and
    /// delegation. Raised by `build`, after basis's own registrations exist to
    /// collide against, rather than a silent swap.
    #[error("a host tool could not be registered: {0}")]
    HostTool(#[from] mentra::tool::ToolNameCollision),
}
