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

    /// Resuming a conversation could not clear its "…for this session"
    /// approval rules, so the resume is refused rather than run with grants
    /// a person gave to an earlier session possibly still live.
    ///
    /// The clear reads the store's `rules.json` before it can filter, so one
    /// corrupt, truncated, or unwritable file fails **every** resume against
    /// that store — ACP `session/load`, `--continue`, a task reattach —
    /// until the file is repaired or deleted. Fresh conversations are
    /// unaffected. Deliberately surfaced rather than worked around: the
    /// underlying store error names the exact file path, and deleting
    /// `rules.json` costs only remembered approval answers, never history.
    #[error(
        "resuming `{agent_id}` could not clear its for-this-session approval rules \
         ({error}); every resume on this store will fail until its rules.json — the \
         path in the error above — is repaired or deleted (deleting it costs only \
         remembered approval answers, never history)"
    )]
    SessionRulesNotCleared {
        /// The conversation whose resume was refused.
        agent_id: String,
        /// The store's own failure, naming the file it could not read or
        /// rewrite.
        #[source]
        error: mentra::error::RuntimeError,
    },

    /// [`Workspace::resume`](crate::Workspace::resume) was handed a
    /// conversation that belongs to a different workspace.
    ///
    /// mentra's store is keyed by agent, not by path, so an id alone says
    /// nothing about where its conversation ran — and everything a resume
    /// restates is *this* workspace's: the policy carrying its `.git`
    /// carve-out and shell posture, the tool audience deciding which of the
    /// registry's tools it can see, the persisted-row tag. Stamping those onto
    /// another repository's conversation would run it under a posture nobody
    /// chose for it while its agent stayed based in its own directory — which
    /// mentra's file tools always allow writes under. So the binding is
    /// checked against the persisted agent's own base directory, and a
    /// mismatch is refused before the session is handed out.
    ///
    /// A host that means "resume one of mine" takes the id from
    /// [`store::list`](crate::store::list) for its own workspace, which is
    /// where a client got it anyway.
    #[error(
        "conversation `{agent_id}` belongs to the workspace at {} and cannot be resumed \
         under the one at {}",
        agent_workspace.display(),
        workspace.display()
    )]
    WorkspaceMismatch {
        /// The conversation whose resume was refused.
        agent_id: String,
        /// The workspace that tried to resume it.
        workspace: std::path::PathBuf,
        /// The directory the persisted agent is actually based in.
        agent_workspace: std::path::PathBuf,
    },

    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),

    #[error(transparent)]
    Context(#[from] ContextError),

    /// Host-resolved model metadata names a provider other than the runtime's.
    ///
    /// Raised while opening a workspace, applying a per-run profile, or
    /// switching an attached [`PreparedRun`](crate::PreparedRun), before model
    /// catalogue, model request, or tool activity. The mismatch cannot be
    /// repaired by looking up the id: provider identity is part of the host's
    /// resolved contract.
    #[error(
        "resolved model `{model}` belongs to provider `{model_provider}`, but the runtime uses \
         `{runtime_provider}`"
    )]
    ResolvedModelProviderMismatch {
        /// The host-resolved model id.
        model: String,
        /// The provider named by the model metadata.
        model_provider: String,
        /// The provider registered on the runtime.
        runtime_provider: String,
    },

    /// Complete provider request options contain one or more extra headers,
    /// but this runtime can persist its Mentra agent configs.
    ///
    /// Header names and values are deliberately absent: either can itself be
    /// sensitive. Use an explicitly ephemeral runtime for request-scoped
    /// credentials, or configure durable connection credentials on the
    /// provider instead.
    #[error(
        "run profile request headers require a runtime built with \
         RuntimeBuilder::with_ephemeral_history"
    )]
    RunProfileHeadersRequireEphemeralHistory,

    /// A [`RunProfile`](crate::RunProfile) field Mentra cannot change on an
    /// already persisted agent.
    ///
    /// Refused before the session is looked up or resumed, rather than
    /// projecting the supported subset and silently dropping part of the
    /// host's contract. Resolved model metadata and the dedicated reasoning
    /// override are each supported alone through Mentra's exact session
    /// setters; every other field is named here when present.
    #[error("run profile field `{field}` cannot be applied while resuming a session")]
    UnsupportedResumeProfile {
        /// The first unsupported field in deterministic profile order.
        field: &'static str,
    },

    /// A resumed profile model would require separately persisting both model
    /// and reasoning changes, because the profile or an effective legacy
    /// effort also changes reasoning.
    ///
    /// Mentra 0.23 exposes one setter for each but no atomic combined update.
    /// Refused before session lookup so a failed second write can never leave
    /// half of the host's profile in force.
    #[error(
        "a resumed run profile cannot change model and reasoning together; \
         apply only one persisted override"
    )]
    NonAtomicResumeProfile,

    /// Discovery was disabled on a builder borrowing a shared runtime.
    ///
    /// Mentra's runtime-global skill loader can be changed after an `Arc` is
    /// borrowed, and its model-visible descriptions are read on every round.
    /// No one-time inspection can therefore prove that a shared runtime stays
    /// discovery-free. Gate 1a's fresh-only lifecycle fails closed before
    /// runtime acquisition, model resolution, provider requests, workspace
    /// tool registration, or interception; use
    /// [`WorkspaceBuilder::with_runtime_builder`](crate::WorkspaceBuilder::with_runtime_builder)
    /// so opening privately constructs the runtime it owns.
    #[error(
        "discovery-disabled workspaces cannot borrow a shared runtime; supply a fresh private \
         runtime recipe with WorkspaceBuilder::with_runtime_builder"
    )]
    DiscoveryDisabledSharedRuntime,

    /// Fresh-only ownership was requested with a borrowed runtime.
    #[error(
        "fresh-only workspaces cannot borrow a shared runtime; supply a fresh private runtime \
         recipe with WorkspaceBuilder::with_runtime_builder"
    )]
    FreshOnlySharedRuntime,

    /// The workspace's one independent mint/resume attempt was already used.
    #[error(
        "this fresh-only workspace has already attempted its one independent prepare or resume; \
         open a new workspace with a fresh private runtime to try again"
    )]
    FreshOnlyRunAlreadyAttempted,

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
    /// [`PreparedRun::output`](crate::PreparedRun::output), which delivers this
    /// inside an [`OutputFailure`](crate::OutputFailure) so the report the turn
    /// earned comes with it.
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

    /// Two live opens of one workspace present different interception chains.
    ///
    /// One directory is one tool audience, and a workspace registers its
    /// chain for that audience — so a second live open of the same root either
    /// **joins** the registration already there, which needs the two chains to
    /// be the same, or would put a second complete chain behind one audience.
    /// The second is not a middle ground: mentra would walk both for either
    /// open's calls, spawning every subprocess hook twice per call and feeding
    /// a non-idempotent rewrite its own output, and the first open's sessions
    /// would be judged by a chain their caller never configured. So identical
    /// chains join and different ones are refused here.
    ///
    /// A host that genuinely needs two hook configurations for one directory
    /// needs two [`Runtime`](crate::Runtime)s. A host whose two opens differ
    /// only in their supplied MCP servers — `basis-host`'s deliberate shape —
    /// never meets this: the hooks come from one discovery configuration and
    /// are equal.
    #[error(
        "the workspace at '{}' is already open on this runtime with a different hook \
         configuration; two live opens of one directory share one interception chain, so they \
         must configure the same one",
        root.display()
    )]
    WorkspaceGuardConflict { root: std::path::PathBuf },

    #[error("failed to load declared tools: {0}")]
    Tools(#[from] crate::tools::declared::DeclaredToolError),

    /// A command target name that cannot be routed on (ADR-0021).
    ///
    /// Dormant: with `with_command_target` withdrawn, nothing can put a name
    /// in front of the validation that raises this, so `build` currently has
    /// no target to refuse. The variant keeps its slot in this
    /// `#[non_exhaustive]` enum for the day a registration seam returns.
    #[error("`{name}` cannot be a command target name: {reason}")]
    CommandTarget { name: String, reason: String },

    /// A host tool a *workspace* was given
    /// ([`WorkspaceBuilder::with_tool`](crate::WorkspaceBuilder::with_tool))
    /// that cannot take the name it asked for.
    ///
    /// Two reasons reach here, and both refuse the open rather than register
    /// part of a set. The name may be one no tool may wear — empty, longer
    /// than a provider accepts, outside the charset, or `mcp__`-prefixed,
    /// which is how mentra names a bridged server's tool. Or it may be taken:
    /// by something this runtime already answers to globally (`spawn`, a
    /// mentra builtin, a [`RuntimeBuilder::with_tool`](crate::RuntimeBuilder::with_tool)
    /// global), by another repository open on the same runtime, or by another
    /// live open of *this* directory.
    ///
    /// That last one is the case worth stating, because it is the one a host
    /// meets by accident. One directory is one tool audience, so two live
    /// opens of it share a namespace, and a native tool is compiled code
    /// closing over whatever the host had when it supplied it — there is no
    /// way to tell two of them apart, and joining would serve the second open
    /// the first one's closure. A host that genuinely needs its own native
    /// tools per open of one directory needs one [`Runtime`](crate::Runtime)
    /// per open; a declaration, which is data, joins instead.
    #[error("the host tool `{name}` cannot be registered for this workspace: it {reason}")]
    WorkspaceHostTool { name: String, reason: String },

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

impl RunError {
    /// Whether this failure means another holder already has the conversation open.
    pub fn is_open_elsewhere(&self) -> bool {
        matches!(
            self,
            Self::Runtime(mentra::error::RuntimeError::LeaseUnavailable(_))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_elsewhere_identifies_only_a_lease_conflict() {
        let conflict = RunError::Runtime(mentra::error::RuntimeError::LeaseUnavailable(
            "already leased".to_string(),
        ));

        assert!(conflict.is_open_elsewhere());
        assert!(!RunError::EmptyPrompt.is_open_elsewhere());
    }
}
