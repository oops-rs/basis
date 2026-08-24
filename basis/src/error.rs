//! What can go wrong, named once at the root.
//!
//! [`RunError`] is the crate's universal error — opening a workspace,
//! preparing a run, driving one — and it used to live in
//! [`run`](crate::run), where history put it. Four of that module's in-edges
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
/// a failure a caller of [`run`] has always been able to receive.
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
    /// [`PreparedRun::output`].
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
}
