//! The process-scoped substrate every workspace borrows.
//!
//! ADR-0018's split. [`Workspace`](crate::Workspace) used to conflate two
//! lifetimes: half its configuration was process infrastructure — a privately
//! built mentra runtime, provider and credential resolution, the history store
//! policy, the host's interceptors — and half was repository discovery. A host
//! opening N workspaces paid the process costs N times. This module is the
//! noun the first half was missing.
//!
//! - **[`Runtime`]** owns mentra's runtime, the provider/credential/base-URL
//!   and model *policy*, the history store policy, and the host's
//!   interceptors. Build one with [`Runtime::builder`], share it with
//!   [`WorkspaceBuilder::with_runtime`](crate::WorkspaceBuilder::with_runtime).
//! - **[`Workspace`](crate::Workspace)** keeps what the repository says —
//!   context, skills, templates, hooks, `.mcp.json` — and borrows the runtime
//!   through an `Arc`. MCP *connections* stay workspace-owned: minted from
//!   repository config, dead with the workspace.
//! - **`Workspace::open(path)` is unchanged sugar** that builds a private
//!   default runtime bound to that path. The one-repository host never sees
//!   this module; only the N-repository host reaches for it.
//!
//! ```no_run
//! # async fn example() -> Result<(), basis::RunError> {
//! use std::sync::Arc;
//! use basis::{Runtime, Workspace};
//!
//! let runtime = Arc::new(Runtime::builder().build()?);
//! let one = Workspace::builder("/repo/one").with_runtime(Arc::clone(&runtime)).open().await?;
//! let two = Workspace::builder("/repo/two").with_runtime(runtime).open().await?;
//! # let _ = (one, two);
//! # Ok(())
//! # }
//! ```

pub(crate) mod builder;
mod credential;
mod executor;
mod interception;
mod tool_results;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// `skill_root_key` is the identity mentra's own registry matches a skills root
// by, so the holder count below is keyed exactly the way upstream keys it —
// where basis used to carry a copy of the rule that could drift out of step.
use mentra::{
    ModelSelector, RuntimePolicy, Session,
    agent::AgentConfig,
    runtime::{SessionOptions, SessionResumeOptions},
    session::PermissionRuleScope,
    skill_root_key,
    tool::{AudienceToolRegistration, ToolAudience, ToolNameCollision},
};

use crate::{shell::ShellAccess, tools::declared::DeclaredToolSpec};

pub use builder::RuntimeBuilder;
pub use tool_results::ToolResultPolicy;

/// The types a **command target's executor** is written against.
///
/// Re-exported for the reason [`CancellationToken`](crate::CancellationToken)
/// is, and under the same rule: every mentra type basis's surface makes a
/// caller *name*, basis re-exports — so a host implementing
/// [`RuntimeExecutor`] never adds mentra to its own manifest and pins the
/// same version, a skew that would otherwise fail to compile with no hint
/// two crates disagree about one trait. There is currently no public way to
/// register an executor a command routes to (`docs/targets.md` has the
/// dateline note); the trait and the types below it are unaffected. The
/// sibling of [`crate::tools`]'s tool-authoring re-exports, in the module
/// that owns this seam.
///
/// The set is what an executor's `run` signature and body actually touch:
/// [`RuntimeExecutor`] to implement, [`CommandRequest`] to read,
/// [`CommandSpec`] to match the command out of it, [`CommandOutput`] to answer
/// with, and [`LocalRuntimeExecutor`] for a wrapper that delegates the ordinary
/// case rather than reimplementing it. Writing the `async fn` also needs
/// [`async_trait`](crate::async_trait), which the crate root already re-exports.
/// See `docs/targets.md` for a worked example.
pub use mentra::runtime::{
    CommandOutput, CommandRequest, CommandSpec, LocalRuntimeExecutor, RuntimeExecutor,
};

/// How patiently a run waits out a provider that is failing transiently, as
/// [`RuntimeBuilder::with_provider_retry`] takes it.
///
/// Mentra's own type, re-exported under the rule on
/// [`CancellationToken`](crate::CancellationToken): the builder makes a host
/// *name* this to call the method, so basis re-exports it rather than sending
/// the host to its own `mentra` dependency and a version pin that can skew.
/// basis defines no schedule of its own — a parallel type here would be two
/// spellings of one policy, and mentra is the half that does the sleeping.
pub use mentra::runtime::ProviderRetry;

/// One statement about how a provider connection is retried: the waits, and
/// how many of them.
///
/// mentra keeps the two apart on `RunOptions` — a typed schedule beside a bare
/// count — and basis's hosts set them apart too, since the commonest adjustment
/// is the count alone. But nothing downstream of a builder ever wants one
/// without the other: a runtime's fallback, the copy every
/// [`PreparedRun`](crate::PreparedRun) carries from it, and the value a turn's
/// options fall back to are each *both* halves. So they travel as one value
/// from the builder to the run rather than as a pair that every hop has to
/// remember to keep together.
///
/// Internal: the halves are set and overridden separately on the public
/// surface — [`RuntimeBuilder::with_provider_retry`] and
/// [`RuntimeBuilder::with_provider_retry_budget`], and their
/// [`TurnOptions`](crate::TurnOptions) counterparts — which is the shape a host
/// asked for and mentra's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    /// How patiently a failing provider is waited out.
    pub(crate) schedule: ProviderRetry,
    /// How many retries follow the initial call.
    pub(crate) budget: usize,
}

impl Default for RetryPolicy {
    /// mentra's own answer to both halves: *unset* and *mentra's default* are
    /// the same policy, so a runtime nobody configured builds exactly the
    /// `RunOptions` mentra would have.
    fn default() -> Self {
        Self {
            schedule: ProviderRetry::default(),
            budget: mentra::runtime::RunOptions::default().retry_budget,
        }
    }
}

/// Which wire transport mentra streams the Responses format over, as
/// [`RuntimeBuilder::with_responses_transport`] takes it.
///
/// Re-exported for the same reason and beside the executor types above, in the
/// module that owns the builder asking for it. Mind the feature: selecting
/// [`ResponsesTransport::WebSocket`] needs basis's `responses-websocket`
/// feature, which forwards to mentra's — see the method for what happens
/// without it.
pub use mentra::provider::ResponsesTransport;

/// Which builtin file tools the model is offered, as
/// [`RuntimeBuilder::with_file_tools`] takes it.
///
/// Mentra's own enum, re-exported beside the two above and for their reason:
/// the method makes a host *name* it, and a parallel type here would be a
/// second spelling of a set mentra is the one registering. basis's default is
/// [`FileToolProfile::Split`] rather than mentra's `Batched` — see the method
/// for why, and for who would want the other.
pub use mentra::FileToolProfile;

/// The types a **host-supplied provider** is written with.
///
/// [`RuntimeBuilder::with_provider_instance`] takes an `impl Provider` — the
/// trait itself is re-exported at the crate root, beside the other types
/// basis's surface makes a caller name — and implementing it touches exactly
/// this set: [`ProviderDescriptor`] (naming itself by [`ProviderId`]) and
/// [`ProviderCapabilities`] to say who and what, [`ModelInfo`] to list
/// models, [`Request`] to receive, mentra's [`ProviderError`] to fail with,
/// and a [`ProviderEventStream`] to answer with — assembled whole from a
/// [`Response`] (content in [`ContentBlock`]s, spoken in a [`Role`], costed
/// in [`TokenUsage`]) via [`provider_event_stream_from_response`], or event
/// by event from [`ProviderEvent`] ([`ContentBlockStart`],
/// [`ContentBlockDelta`]). Re-exported beside the executor types above and
/// under their rule: the builder makes a host *name* these, so basis
/// re-exports them rather than costing the host a mentra dependency and a
/// version pin that can skew. Mind the name: this `ProviderError` is
/// mentra's — the one a `Provider` implementation answers with — not
/// [`crate::provider::ProviderError`], which is how basis's own *resolution*
/// refuses.
pub use mentra::provider::{
    ContentBlock, ContentBlockDelta, ContentBlockStart, ModelInfo, ProviderCapabilities,
    ProviderDescriptor, ProviderError, ProviderEvent, ProviderEventStream, ProviderId, Request,
    Response, Role, TokenUsage, provider_event_stream_from_response,
};

/// Which request format a custom endpoint is spoken to in, as
/// [`RuntimeBuilder::with_wire`] takes it.
///
/// Two wires answer to the name "OpenAI-compatible" and they agree on almost
/// nothing: a flat `messages` array against typed input items, tool arguments
/// as a JSON string against a value, `max_tokens` against `max_output_tokens`
/// — and, the part an operator meets first, `v1/chat/completions` against
/// `v1/responses`. Speaking the wrong one is a 404 on the very first turn,
/// worded like a mistyped URL.
///
/// basis's own enum rather than mentra's `WireApi`, which is where the rule
/// above gives way. `WireApi` also names Anthropic's and Gemini's formats, and
/// neither is something a base URL can be spoken to in — re-exporting it would
/// let a host write a call basis could only refuse. Two wires are what basis
/// can honor here, so two variants is what it takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Wire {
    /// What the OpenAI-compatible ecosystem actually implements: Ollama, LM
    /// Studio, vLLM, llama.cpp, DeepSeek, Groq, Together, OpenRouter, and the
    /// gateways in front of them. The default, because it is the wire behind
    /// nearly every URL anyone pastes.
    #[default]
    ChatCompletions,
    /// OpenAI's own `v1/responses` — served by OpenAI, by the proxies that
    /// forward to it, and by almost nothing else.
    Responses,
}

use crate::error::RunError;

use builder::execution::{PolicyShaping, workspace_policy};

/// One process's substrate: mentra's runtime plus the resolution policy and
/// host guards that were fixed when it was built.
///
/// Shared through an `Arc` by every [`Workspace`](crate::Workspace) opened on
/// it, so N repositories cost one provider resolution and one store handle.
/// `Send` and `Sync`; sessions are minted from `&self`.
pub struct Runtime {
    mentra: mentra::Runtime,
    /// The fixed pairs every process this runtime spawns receives, from
    /// [`RuntimeBuilder::with_command_environment`].
    ///
    /// Kept here rather than only inside the executor because "every process"
    /// means more than commands: a declared tool
    /// ([`crate::tools::declared`]) spawns a program of its own, and a host
    /// that told the runtime where its service lives expects that program to
    /// be told too. The registry is built per workspace and this is
    /// runtime-scoped (ADR-0018), so the runtime is where the workspace
    /// borrows it from. Shared with the executor rather than copied, so there
    /// is one statement and not two.
    command_environment: Arc<std::collections::BTreeMap<String, String>>,
    /// The id this runtime's provider is registered under — resolution's
    /// answer, or a host-supplied instance's own descriptor. Models resolve
    /// against it at workspace open, and its string is what workspaces copy
    /// into their run headers.
    provider: ProviderId,
    /// The default model *policy*; a workspace may override the selector, and
    /// the resolved id is always the workspace's own fact.
    model: ModelSelector,
    /// Default retry policy for runs minted here, from
    /// [`RuntimeBuilder::with_provider_retry`] and
    /// [`RuntimeBuilder::with_provider_retry_budget`].
    ///
    /// Runtime-scoped for ADR-0018's reason: it describes the *connection* to
    /// the provider, like the credential and the base URL beside it, and not
    /// what one prompt may spend. Kept here because mentra takes it on each
    /// run's options rather than on its runtime, so this is the value every
    /// [`PreparedRun`](crate::PreparedRun) minted on this runtime copies. A
    /// turn may override either half through
    /// [`TurnOptions`](crate::TurnOptions).
    retry_policy: RetryPolicy,
    /// Whether the builder explicitly selected in-memory, process-local
    /// history with `with_ephemeral_history`.
    ///
    /// Kept as a posture fact because a per-run provider header is persisted
    /// inside Mentra's `AgentConfig`. A host may put credentials there only
    /// when this says no agent record can reach disk.
    ephemeral_history: bool,
    /// Where a compaction snapshot goes, derived once from the history posture
    /// this runtime was built with.
    ///
    /// Runtime-scoped because the history posture is: a snapshot is a verbatim
    /// copy of the conversation the store holds, so the two belong in one
    /// directory and only one caller knows which. Every workspace opened here
    /// reads it at open and writes it into its agent config — the numbers
    /// beside it in that config are the workspace's
    /// ([`Compaction`](crate::Compaction)), the directory is this.
    transcripts: PathBuf,
    /// What this runtime's builder said about every policy it hands out, kept
    /// so a workspace's own policy gets the same shaping the runtime's did.
    /// See [`session_policy`](Self::session_policy).
    policy_shaping: PolicyShaping,
    /// mentra's hold on the host's own interception participants, registered
    /// globally when this runtime was built (ADR-0018) and kept for as long as
    /// it lives.
    ///
    /// Global rather than per workspace, because host scope *is* runtime
    /// scope: an audience-scoped registration would run a host's guards for
    /// the sessions basis mints and silently skip the ones a host creates for
    /// itself through [`mentra_runtime`](Self::mentra_runtime), which is the
    /// case `with_interceptor`'s doc promises. Still one chain with each
    /// workspace's own: mentra composes one participant snapshot per call out
    /// of every matching batch, so this batch and a workspace's join in
    /// registration order — this one first, since a runtime is built before
    /// any workspace on it opens — and a rewrite's attribution accumulates
    /// across both. [`crate::runtime::interception`] carries the argument.
    ///
    /// `None` when the host registered no interceptors.
    #[allow(dead_code, reason = "held for its Drop")]
    host_interceptors: Option<mentra::runtime::ExecutionHookRegistration>,
    /// Which workspace owns each MCP server name on this runtime's single tool
    /// registry — bridged tools are namespaced by server, so two workspaces
    /// configuring one name must be told apart here.
    #[cfg(feature = "mcp")]
    mcp_claims: Mutex<HashMap<String, McpClaim>>,
    /// Which workspace owns each declared tool name on the same single
    /// registry. See [`Runtime::claim_declared_tool`] for why this exists.
    declared_claims: Mutex<HashMap<String, DeclaredClaim>>,
    /// How many open workspaces hold each skills root on this runtime's single
    /// skill registry. See [`Runtime::register_skill_roots`].
    skill_root_holders: Mutex<HashMap<PathBuf, usize>>,
    /// One live interception chain per tool audience, holder-counted. See
    /// [`Runtime::register_hook_chain`].
    hook_chains: Mutex<HashMap<String, HookChainClaim>>,
}

/// One workspace's interception chain, registered live for its audience.
///
/// The third join-and-count ledger on this runtime, and it exists for the
/// reason the other two do: a *root* may be open twice — the shape
/// `basis-host` produces deliberately, one workspace per set of
/// client-supplied MCP servers — while the thing being registered is one per
/// root. An audience is derived from the root, so two such opens would
/// otherwise put two complete chains behind one audience and mentra would walk
/// both for either one's calls: every subprocess hook spawned twice per call,
/// an audit hook logging each call twice, and a rewrite that is not idempotent
/// fed its own output.
///
/// So the second open of a root **joins** the first's registration, and the
/// chain comes off when the last holder goes — exactly as a declared tool's
/// claim behaves. The price of joining is that both opens must agree about
/// what the chain *is*: see [`Runtime::register_hook_chain`].
#[derive(Debug)]
struct HookChainClaim {
    /// The workspace root behind the audience, for the refusal's message.
    root: PathBuf,
    holders: usize,
    /// The chain the live registration is running. A same-root open presenting
    /// a different one is refused rather than silently subjected to this.
    hooks: Vec<crate::hooks::HookSpec>,
    /// mentra's own hold, kept beside the claim rather than by a workspace
    /// because the claim is what counts holders: the chain has to outlive the
    /// first of them to drop. Removing the claim drops this, and dropping it
    /// is the unregister.
    #[allow(dead_code, reason = "held for its Drop")]
    registration: mentra::runtime::ExecutionHookRegistration,
}

/// One holder's share of a live interception chain, released on drop.
///
/// The sibling of [`DeclaredTools`](crate::tools::declared::DeclaredTools) and
/// [`SkillRoots`](crate::skills::SkillRoots), and held by the
/// [`Workspace`](crate::Workspace) for the same reason: dropping it is what
/// stops a dropped workspace being consulted.
pub(crate) struct HookChainHold {
    runtime: Arc<Runtime>,
    /// The audience this chain answers for — the key, and the only thing the
    /// release needs.
    audience: String,
}

impl std::fmt::Debug for HookChainHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookChainHold")
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl Drop for HookChainHold {
    fn drop(&mut self) {
        self.runtime.release_hook_chain(&self.audience);
    }
}

/// The live scope one workspace's sessions run in: what they may do, and whose
/// tools they can see.
///
/// Both halves are session options upstream, both are deliberately left out of
/// the persisted `AgentConfig`, and both therefore have to be restated on every
/// resume — so they travel as one value rather than as arguments each caller
/// has to remember to keep in step.
#[derive(Debug, Clone)]
pub(crate) struct SessionScope {
    /// [`store::runtime_identifier`](crate::store::runtime_identifier) for the
    /// workspace, in its two roles: the tag this session's persisted rows
    /// carry, and the name of the tool audience it resolves in.
    ///
    /// One identity rather than two, because there is one workspace. A second
    /// string would only be an opportunity for the listing and the roster to
    /// disagree about which repository a session belongs to, and mentra treats
    /// an audience as opaque — it compares for equality and reads nothing into
    /// the value — so the identity basis already derives is exactly what an
    /// audience wants.
    pub(crate) identifier: String,
    /// The complete policy for this session and its descendants; see
    /// [`Runtime::session_policy`].
    pub(crate) policy: RuntimePolicy,
}

impl SessionScope {
    /// The namespace this workspace's own tools are registered under, and the
    /// one its sessions resolve names in.
    pub(crate) fn audience(&self) -> ToolAudience {
        ToolAudience::new(self.identifier.clone())
    }
}

/// One MCP server name held on this runtime's single tool registry, and what
/// was bridged under it.
///
/// The names matter as much as the owner, and only for one question: *which
/// `mcp__*` tools on this runtime belong to somebody else?* mentra's audience
/// ladder answers it for a workspace in another audience, and cannot answer it
/// for a sibling open of the **same directory** — two such opens share one
/// audience by construction (`SessionScope::audience`), which is exactly the
/// pair `basis-host` produces when one repository is opened twice with
/// different client-supplied servers. So the tool names live beside the claim,
/// and [`Runtime::foreign_mcp_tools`] is what a mint asks.
#[cfg(feature = "mcp")]
#[derive(Debug)]
struct McpClaim {
    /// The claiming workspace root; only it can release the name.
    root: PathBuf,
    /// The `mcp__<server>__<tool>` names bridged under this server, in the
    /// order they took. Empty until the connection succeeds, and empty forever
    /// for a server that never came up.
    tools: Vec<String>,
}

/// A declared tool name registered on this runtime by a workspace still open.
///
/// `holders` rather than a bare owner because one root may be open twice — a
/// host that opens the same repository for two concurrent callers — and the
/// first of those to drop must not free a name the second is still serving.
/// The entry goes when the count reaches zero, together with the tool itself.
#[derive(Debug)]
struct DeclaredClaim {
    root: PathBuf,
    holders: usize,
    supplied_holders: usize,
    /// The complete resolved declaration the live registration executes.
    /// Supplied same-root holders compare against it before joining.
    spec: DeclaredToolSpec,
    /// mentra's own hold on the audience registration, which is what keeps
    /// the tool answering. Kept beside the claim rather than by the workspace
    /// because the claim is what counts holders: the second open of a root
    /// joins this registration instead of making its own, and the tool has to
    /// outlive the first of them to drop. `None` between the claim and the
    /// registration, and for every holder after the first.
    registration: Option<AudienceToolRegistration>,
}

#[cfg(feature = "mcp")]
impl McpClaim {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DeclaredToolOrigin {
    File,
    Supplied,
}

/// Hand-written because mentra's runtime is not `Debug`. No credential lives
/// here — the key was consumed building the provider — so nothing is redacted.
impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("provider", &self.provider.as_str())
            .field("model", &self.model)
            .field("ephemeral_history", &self.ephemeral_history)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Configures a runtime before building it.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::default()
    }

    /// The runtime the workspaces run on, for a host that wants mentra's own
    /// surface — the task board, teams, the store — alongside basis's.
    ///
    /// The same bargain as [`PreparedRun::session`](crate::PreparedRun::session):
    /// basis does not hide mentra, and reaching past basis's surface is a
    /// supported thing to do rather than a workaround.
    pub fn mentra_runtime(&self) -> &mentra::Runtime {
        &self.mentra
    }

    /// The provider this runtime resolves models against, as its `ProviderId`
    /// string — what every run from every workspace on it reports.
    pub fn provider(&self) -> &str {
        self.provider.as_str()
    }

    /// Where compaction files this runtime's transcript snapshots.
    ///
    /// Read by [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open) into
    /// the agent config, which is the only place mentra takes it. `pub(crate)`
    /// because a host that wants to say where it goes says it once, with
    /// [`with_store_dir`](RuntimeBuilder::with_store_dir), and a reader here
    /// would invite a second answer.
    pub(crate) fn transcripts_dir(&self) -> &Path {
        &self.transcripts
    }

    /// The retry fallback every run minted on this runtime carries.
    ///
    /// Read at mint by `Workspace::minted`, which is what makes a
    /// runtime-scoped knob reach a per-run option.
    pub(crate) const fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }

    /// Whether this runtime was explicitly built with ephemeral history.
    pub(crate) const fn has_ephemeral_history(&self) -> bool {
        self.ephemeral_history
    }

    /// Resolves the model a workspace will use: its own override when it has
    /// one, this runtime's policy otherwise. The result is the workspace's
    /// fact; the policy is the runtime's (ADR-0018).
    pub(crate) async fn resolve_model(
        &self,
        selector: Option<ModelSelector>,
    ) -> Result<ModelInfo, RunError> {
        let selector = selector.unwrap_or_else(|| self.model.clone());

        Ok(self
            .mentra
            .resolve_model(self.provider.clone(), selector)
            .await?)
    }

    /// The complete policy one workspace's sessions run under.
    ///
    /// Derived here rather than on the workspace because half of it is the
    /// runtime's: [`workspace_policy`] states what the repository asked for,
    /// and [`PolicyShaping`] re-applies what the *builder* was told, which a
    /// per-session policy would otherwise drop — mentra replaces the runtime's
    /// policy wholesale for a session rather than merging with it.
    ///
    /// On a private runtime this is byte-identical to the policy
    /// [`RuntimeBuilder::build_for`] baked, and handing it over again costs
    /// nothing. On a shared one it is the whole point: it is what makes a
    /// `ShellAccess::Denied` workspace, its `.git` carve-out and its memory
    /// roots hold for its own sessions and for nobody else's.
    pub(crate) fn session_policy(
        &self,
        workspace: &Path,
        shell: ShellAccess,
        memory_roots: &[PathBuf],
    ) -> RuntimePolicy {
        self.policy_shaping
            .apply_to(workspace_policy(workspace, shell, memory_roots))
    }

    /// The one place a workspace's sessions are created.
    ///
    /// Every field of the scope is applied per session rather than per runtime,
    /// and for one reason: a shared runtime is built before any workspace
    /// exists, so anything fixed on it is fixed for all of them. The
    /// identifier tags this session's persisted rows, without which a
    /// per-workspace listing could not tell one repository's conversations from
    /// another's; the policy says what this repository's runs may do; and the
    /// audience decides which of the registry's tools they can see. The private
    /// path is unaffected by any of it — a runtime with one workspace already
    /// agreed with itself about all three.
    pub(crate) fn mint(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
        scope: &SessionScope,
    ) -> Result<Session, RunError> {
        let options = SessionOptions {
            config,
            policy: Some(scope.policy.clone()),
            tool_audience: Some(scope.audience()),
            project_id: None,
            runtime_identifier: Some(Arc::from(scope.identifier.as_str())),
        };

        Ok(self
            .mentra
            .create_session_with_options(name, model, options)?)
    }

    /// The one place a workspace's sessions are resumed; see
    /// [`mint`](Self::mint) for why it is a place at all.
    ///
    /// The policy and the audience are restated here because mentra
    /// deliberately keeps neither in the persisted agent: a resumed session
    /// handed no policy would inherit the runtime's, which on a shared runtime
    /// is the posture of no workspace at all, and one handed no audience would
    /// resolve only global names — losing this workspace's own bridged and
    /// declared tools between one process and the next.
    ///
    /// **And restating them is exactly why the binding is checked first.** An
    /// agent id says nothing about where its conversation ran — mentra's store
    /// is keyed by agent, not by path — so a caller that picked the workspace
    /// by a client's `cwd` and the conversation by an id it was handed can
    /// bring the two together wrongly. Everything this method then states is
    /// `workspace`'s: the policy carrying its `.git` carve-out, shell posture
    /// and memory roots; the audience deciding which of the shared registry's
    /// tools the model sees. The agent's own `base_dir` does not move with any
    /// of it, and mentra's file tools always allow writes under an agent's
    /// base directory — so a repository whose workspace denies commands and
    /// carves out `.git` would find both true of *another* repository's posture
    /// and neither of its own. The persisted agent's base directory is checked
    /// against this workspace's identity before anything is stated onto it, and
    /// before the session-scope clear below mutates a conversation that is not
    /// this workspace's to mutate.
    pub(crate) fn resume_minted(
        &self,
        agent_id: &str,
        workspace: &Path,
        scope: &SessionScope,
    ) -> Result<Session, RunError> {
        let session = self.mentra.resume_session_with_options(
            agent_id,
            SessionResumeOptions {
                project_id: None,
                policy: Some(scope.policy.clone()),
                tool_audience: Some(scope.audience()),
            },
        )?;

        // Compared as identities rather than as paths, through the one
        // function that decides what "the same workspace" means for the store
        // — so a symlinked or relative spelling on either side is the same
        // answer here as it is in a session listing.
        let based_in = session.config().workspace.base_dir.clone();
        if crate::store::runtime_identifier(&based_in) != scope.identifier {
            return Err(RunError::WorkspaceMismatch {
                agent_id: agent_id.to_owned(),
                workspace: workspace.to_path_buf(),
                agent_workspace: based_in,
            });
        }

        // basis's documented duration for a "…for this session" answer is the
        // live session: it survives further runs in the process that holds it
        // and dies at the next attach. mentra 0.26 disagrees — its session
        // rule namespace is the stable agent id, persisted in the runtime
        // store and replayed across every resume — so the attach is where
        // basis restores its own contract: clear the session scope before the
        // resumed session answers anything from it. A fresh mint has a fresh
        // agent id and nothing to clear; project- and global-scope rules are
        // durable by definition and stay.
        //
        // The `?` fails the whole resume, and the two ways the clear can fail
        // deserve stating apart, because the refusal earns its keep on only
        // one of them. A store that cannot be *read* (corrupt, truncated,
        // newer schema) would fail closed at point of use anyway — mentra
        // propagates the same read error from every rule lookup before
        // applying anything — so refusing the resume there adds determinism,
        // not protection. A store that reads but cannot be *rewritten*
        // (permissions, disk full) is the case the refusal genuinely guards:
        // point-of-use lookups succeed, so the stale session grants WOULD
        // apply, silently. The cost — one bad rules.json fails every resume
        // on the store until repaired — is documented on the error variant
        // and on `Workspace::resume`.
        session
            .permission_handle()
            .clear_scope(PermissionRuleScope::Session)
            .map_err(|error| RunError::SessionRulesNotCleared {
                agent_id: agent_id.to_owned(),
                error,
            })?;

        Ok(session)
    }

    /// The fixed command environment, as the pairs a spawned program is given.
    ///
    /// A `Vec` rather than the map it is stored as, because that is the shape
    /// `crate::subprocess::execute` takes and what a declared tool merges its
    /// own `env` over. Sorted, because the map is.
    pub(crate) fn command_environment(&self) -> Vec<(String, String)> {
        self.command_environment
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// Claims an MCP server name on this runtime's tool registry for the
    /// workspace at `root`, returning the name that took effect.
    ///
    /// Bridged tools are namespaced `mcp__<server>__<tool>` on one registry,
    /// so a name two workspaces both configure would collide: the second
    /// claimant gets a deterministic suffix derived from its root instead, and
    /// reports it through [`Workspace::mcp_servers`](crate::Workspace::mcp_servers).
    #[cfg(feature = "mcp")]
    pub(crate) fn claim_mcp_server(&self, name: &str, root: &Path) -> String {
        let mut claims = self.mcp_claims.lock().expect("mcp claim map poisoned");

        if !claims.contains_key(name) {
            claims.insert(name.to_string(), McpClaim::new(root));
            return name.to_string();
        }

        // `-` cannot appear in the `__` separators mentra parses on, so a
        // suffixed name still round-trips through `parse_mcp_tool_name`.
        let mut effective = format!("{name}-{}", root_suffix(root));
        let mut attempt = 2_u32;
        while claims.contains_key(&effective) {
            effective = format!("{name}-{}-{attempt}", root_suffix(root));
            attempt += 1;
        }

        claims.insert(effective.clone(), McpClaim::new(root));
        effective
    }

    /// Records what a connected server actually bridged under the name it
    /// claimed, so a sibling open can be told which names are not its own.
    ///
    /// Separate from the claim because the two happen at different times: a
    /// name is claimed before the connection is attempted — the manager
    /// namespaces every tool by it — and what came back is only known after.
    /// Only the owning root may write, for
    /// [`release_mcp_claim`](Self::release_mcp_claim)'s reason.
    #[cfg(feature = "mcp")]
    pub(crate) fn record_bridged_tools(&self, name: &str, root: &Path, tools: Vec<String>) {
        let mut claims = self.mcp_claims.lock().expect("mcp claim map poisoned");
        if let Some(claim) = claims.get_mut(name)
            && claim.root == root
        {
            claim.tools = tools;
        }
    }

    /// Every `mcp__*` name on this runtime whose server is not one of `own`.
    ///
    /// What a mint hides from its model. Two sources, because two kinds of
    /// `mcp__*` registration are reachable from a workspace's session and
    /// neither is covered by mentra's audience ladder:
    ///
    /// - **A sibling open of the same directory.** Its bridged tools are
    ///   registered for the audience this workspace also resolves in — one
    ///   directory is one audience — so mentra reports them `Visible`. That is
    ///   the pair `basis-host` deliberately produces when two ACP sessions open
    ///   one repository with different `mcpServers`, and without this the
    ///   session that supplied none could list *and call* the other's
    ///   authenticated server.
    /// - **A host tool registered globally under an `mcp__`-shaped name**
    ///   ([`RuntimeBuilder::with_tool`]). A global is visible to every
    ///   audience on purpose; a name shaped like a bridged tool of a server
    ///   this workspace never configured is not what that rule is for.
    ///
    /// Hiding rather than refusing, and by name: these tools belong to
    /// somebody still open and still serving them. A name in `hidden_tools` is
    /// neither offered nor invokable (`Agent::name_is_allowed`), which is the
    /// property that matters — a model that guessed the name gets the same
    /// answer as one that was never shown it.
    #[cfg(feature = "mcp")]
    pub(crate) fn foreign_mcp_tools(&self, own: &[String]) -> std::collections::BTreeSet<String> {
        let mine = |server: &str| own.iter().any(|owned| owned == server);
        let mut foreign = std::collections::BTreeSet::new();

        for (server, claim) in self
            .mcp_claims
            .lock()
            .expect("mcp claim map poisoned")
            .iter()
        {
            if mine(server) {
                continue;
            }
            foreign.extend(claim.tools.iter().cloned());
        }

        for descriptor in self.mentra.tools() {
            let name = &descriptor.provider.name;
            if let Some((server, _)) = mentra::mcp::parse_mcp_tool_name(name)
                && !mine(server)
            {
                foreign.insert(name.clone());
            }
        }

        foreign
    }

    /// Releases a claim [`claim_mcp_server`](Self::claim_mcp_server) granted.
    /// Only the owning root can release, so one workspace's drop cannot free a
    /// name another still serves.
    #[cfg(feature = "mcp")]
    pub(crate) fn release_mcp_claim(&self, name: &str, root: &Path) {
        let mut claims = self.mcp_claims.lock().expect("mcp claim map poisoned");
        if claims.get(name).is_some_and(|claim| claim.root == root) {
            claims.remove(name);
        }
    }

    /// Claims a declared tool's name for the workspace at `root`, or says who
    /// holds it.
    ///
    /// Refused rather than suffixed, which is where this parts company with
    /// [`claim_mcp_server`](Self::claim_mcp_server). A bridged tool's name is
    /// already synthetic (`mcp__<server>__<tool>`), so renaming one on a
    /// collision costs nothing; a declared tool's name is what the model calls,
    /// what an operator writes in a remembered rule, and what a
    /// `.basis/hooks.json` entry matches on, so a silently renamed one is a
    /// guard that silently stops matching.
    ///
    /// The check that matters is the first-time one: mentra's registry is a map
    /// and `register_tool` *replaces*, so without this a workspace file could
    /// declare a tool called `spawn` and take over the name basis's own tool —
    /// and every rule an operator ever wrote about it — answers to.
    ///
    /// `Ok(true)` means this caller is the name's *first* live holder and owes
    /// the runtime a registration; `Ok(false)` means a sibling open of the same
    /// root already registered it, and the tool on the runtime is the one that
    /// open is serving. One name is one program, so the second open of a
    /// repository joins the registration rather than replacing it under the
    /// first open's running agents.
    pub(crate) fn claim_declared_tool(
        &self,
        root: &Path,
        spec: &DeclaredToolSpec,
        origin: DeclaredToolOrigin,
    ) -> Result<bool, String> {
        let name = &spec.name;
        let mut claims = self
            .declared_claims
            .lock()
            .expect("declared tool claim map poisoned");

        match claims.get_mut(name) {
            Some(claim) if claim.root != root => Err(format!(
                "the workspace at {} is open on this runtime and declares a tool by that name",
                claim.root.display()
            )),
            Some(claim)
                if claim.spec != *spec
                    && (claim.supplied_holders > 0
                        || matches!(origin, DeclaredToolOrigin::Supplied)) =>
            {
                Err(
                    "another live open of this workspace supplied different configuration under \
                     that name"
                        .to_string(),
                )
            }
            Some(claim) => {
                claim.holders += 1;
                if matches!(origin, DeclaredToolOrigin::Supplied) {
                    claim.supplied_holders += 1;
                }
                Ok(false)
            }
            None if self.registers_tool(name) => {
                Err("this runtime already offers a tool by that name".to_string())
            }
            None => {
                claims.insert(
                    name.to_string(),
                    DeclaredClaim {
                        root: root.to_path_buf(),
                        holders: 1,
                        supplied_holders: usize::from(matches!(
                            origin,
                            DeclaredToolOrigin::Supplied
                        )),
                        spec: spec.clone(),
                        registration: None,
                    },
                );
                Ok(true)
            }
        }
    }

    /// Puts a claimed declared tool on the registry, for the claiming
    /// workspace's audience alone.
    ///
    /// Audience-scoped rather than global because a declaration is a
    /// *repository's* statement about a program: on a runtime serving five
    /// repositories, a global registration would offer one repository's tool
    /// to the other four's models. mentra's resolution ladder answers that for
    /// basis now — a name held only by another audience resolves to `Hidden`,
    /// so it is neither listed nor reachable by guessing it.
    ///
    /// The guard goes into the claim, which is the thing that knows how many
    /// workspaces are holding this name; dropping the claim drops the guard and
    /// takes the tool off the registry in the same breath.
    pub(crate) fn install_declared_tool<T>(
        &self,
        audience: &ToolAudience,
        name: &str,
        root: &Path,
        tool: T,
    ) -> Result<(), ToolNameCollision>
    where
        T: mentra::tool::ExecutableTool + 'static,
    {
        let registration = self
            .mentra
            .try_register_tool_for_audience(audience.clone(), tool)?;

        let mut claims = self
            .declared_claims
            .lock()
            .expect("declared tool claim map poisoned");
        // Unreachable in practice — the claim map serializes every opener on
        // this runtime and nothing between the claim and here releases one.
        // Dropping the guard rather than storing it is still the right answer
        // if it ever happened: a registration nobody holds is a tool nobody
        // would take back off.
        if let Some(claim) = claims.get_mut(name)
            && claim.root == root
        {
            claim.registration = Some(registration);
        }
        Ok(())
    }

    /// Releases a claim [`claim_declared_tool`](Self::claim_declared_tool)
    /// granted, taking the tool off the runtime when the last holder goes.
    ///
    /// Only the owning root can release, so one workspace's drop cannot free a
    /// name another still serves. Removing the claim is what makes the claim
    /// map and mentra's registry say the same thing: the registration guard
    /// goes with it, so a released name is free because nothing answers to it
    /// any more, rather than free-with-a-stale-entry-behind-it.
    pub(crate) fn release_declared_tool(
        &self,
        name: &str,
        root: &Path,
        origin: DeclaredToolOrigin,
    ) {
        let mut claims = self
            .declared_claims
            .lock()
            .expect("declared tool claim map poisoned");

        let Some(claim) = claims.get_mut(name) else {
            return;
        };
        if claim.root != root {
            return;
        }

        claim.holders = claim.holders.saturating_sub(1);
        if matches!(origin, DeclaredToolOrigin::Supplied) {
            claim.supplied_holders = claim.supplied_holders.saturating_sub(1);
        }
        if claim.holders == 0 {
            // Under the claim lock, so no other claimant can see the name free
            // while the tool is still registered: the removed claim owns the
            // registration guard, and dropping it here is the unregister.
            claims.remove(name);
        }
    }

    /// Puts a workspace's interception chain on the runtime for its audience,
    /// or joins the identical one already there.
    ///
    /// `Ok` on two shapes and refuses a third:
    ///
    /// - **Nobody holds this audience.** The runner is registered as one
    ///   atomic [`ExecutionHookParticipant`](mentra::runtime::ExecutionHookParticipant)
    ///   batch and the guard goes into the claim.
    /// - **A live open of this same root presents the same chain.** It joins:
    ///   one registration, one holder more. That is what keeps a repository's
    ///   hook programs spawned once per call rather than once per call per
    ///   live open, and what keeps a non-idempotent rewrite from being fed its
    ///   own output — the thing the deleted directory-keyed registry counted
    ///   holders for.
    /// - **A live open of this root presents a *different* chain.** Refused
    ///   with [`RunError::WorkspaceGuardConflict`]. Joining would subject the
    ///   first open's sessions to a chain their caller never configured, and
    ///   registering a second batch would run both for either — so the honest
    ///   answers are "the same chain" or "no". A host that genuinely needs two
    ///   hook configurations for one directory needs two runtimes.
    ///
    /// Compared on the chain itself rather than on a digest of it, because a
    /// [`HookSpec`](crate::hooks::HookSpec) is small, `Eq`, and already the
    /// complete statement of what a participant will do.
    pub(crate) fn register_hook_chain(
        self: &Arc<Self>,
        audience: &ToolAudience,
        root: &Path,
        runner: crate::hooks::HookRunner,
    ) -> Result<HookChainHold, RunError> {
        let key = audience.as_str().to_string();
        let mut chains = self.hook_chains.lock().expect("hook chain map poisoned");

        match chains.get_mut(&key) {
            Some(claim) if claim.hooks != *runner.hooks() => {
                return Err(RunError::WorkspaceGuardConflict {
                    root: claim.root.clone(),
                });
            }
            Some(claim) => claim.holders += 1,
            None => {
                let hooks = runner.hooks().to_vec();
                // Under the claim lock, so nothing can observe an audience
                // counted but unregistered, or free one between the register
                // and the count.
                let registration = self
                    .mentra
                    .register_execution_hook_for_audience(audience.clone(), runner);
                chains.insert(
                    key.clone(),
                    HookChainClaim {
                        root: root.to_path_buf(),
                        holders: 1,
                        hooks,
                        registration,
                    },
                );
            }
        }
        drop(chains);

        Ok(HookChainHold {
            runtime: Arc::clone(self),
            audience: key,
        })
    }

    /// Releases one holder's share, taking the chain off the runtime when the
    /// last of them goes.
    ///
    /// Under the claim lock, like the declared-tool and skills-root releases
    /// below, so no other opener can see an audience free while its chain is
    /// still registered: the removed claim owns mentra's guard, and dropping it
    /// here is the unregister.
    fn release_hook_chain(&self, audience: &str) {
        let mut chains = self.hook_chains.lock().expect("hook chain map poisoned");

        let Some(claim) = chains.get_mut(audience) else {
            return;
        };
        claim.holders = claim.holders.saturating_sub(1);
        if claim.holders == 0 {
            chains.remove(audience);
        }
    }

    /// Registers a workspace's skills roots and counts it as a holder of each.
    ///
    /// mentra 0.24 made registration all-or-nothing and gave a host
    /// `unregister_skills_dirs` to take a root back, which is what lets a
    /// workspace stop leaving its skills on a runtime that outlives it. What
    /// mentra cannot know is *how many* workspaces asked for a root: a root is
    /// one entry upstream however often it is registered, and on a shared
    /// runtime (ADR-0018) every workspace registers the same two user-scoped
    /// roots. Unregistering on the first drop would take the user's own skills
    /// away from every repository still open, so the count lives here — the
    /// same ledger, and for the same reason, as
    /// [`claim_declared_tool`](Self::claim_declared_tool). Upstream says as
    /// much itself: [`mentra::skill_root_key`]'s own doc tells a host counting
    /// several holders of one root to capture that key and hold it, which is
    /// what the map below is.
    ///
    /// The registration happens under the holder lock, so nothing can observe
    /// a root counted but absent, or free one between the register and the
    /// count. An `Err` leaves both sides untouched: mentra commits nothing,
    /// and no holder is recorded.
    pub(crate) fn register_skill_roots(
        &self,
        roots: &[PathBuf],
    ) -> Result<(), mentra::SkillLoadError> {
        let mut holders = self
            .skill_root_holders
            .lock()
            .expect("skill root holder map poisoned");

        self.mentra.register_skills_dirs(roots)?;
        for root in roots {
            *holders.entry(skill_root_key(root)).or_insert(0) += 1;
        }
        Ok(())
    }

    /// Releases the holds [`register_skill_roots`](Self::register_skill_roots)
    /// recorded, taking a root off the runtime when its last holder goes.
    ///
    /// A root nobody else holds leaves mentra's registry entirely: the skills
    /// it contributed stop being listed to the model, `load_skill` refuses
    /// them, and a name this root had shadowed resolves to the weaker root
    /// again. Dropping the last root of all also withdraws `load_skill`, which
    /// the next workspace to open restores.
    ///
    /// Under the holder lock, like the declared-tool release above, so no
    /// other opener can see a root free while its skills are still registered.
    pub(crate) fn release_skill_roots(&self, roots: &[PathBuf]) {
        let mut holders = self
            .skill_root_holders
            .lock()
            .expect("skill root holder map poisoned");

        for root in roots {
            let key = skill_root_key(root);
            let Some(count) = holders.get_mut(&key) else {
                continue;
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                holders.remove(&key);
                self.mentra.unregister_skills_dir(&key);
            }
        }
    }

    /// The descriptor of the declared tool live under `name`.
    ///
    /// Read off basis's own hold on the registration, because mentra exposes no
    /// reader for an audience's tools: `Runtime::tools` and
    /// `Runtime::tool_descriptor` both walk the global map only (an upstream
    /// candidate), so an audience-registered tool is invisible to both.
    /// `#[cfg(test)]` because the only caller is the test that pins *which*
    /// program a name is serving when one repository is open twice.
    #[cfg(test)]
    pub(crate) fn declared_tool_descriptor(
        &self,
        name: &str,
    ) -> Option<mentra::tool::RuntimeToolDescriptor> {
        self.declared_claims
            .lock()
            .expect("declared tool claim map poisoned")
            .get(name)?
            .registration
            .as_ref()
            .map(|registration| registration.descriptor().clone())
    }

    /// Whether mentra's registry already answers to `name` globally — a
    /// builtin, basis's own `spawn`, or a host tool.
    ///
    /// Globals only, which is the question worth asking: an audience-scoped
    /// name belonging to another workspace is already refused by the claim map
    /// above, and one belonging to *this* workspace is refused by mentra's own
    /// same-audience collision check when the registration is attempted.
    fn registers_tool(&self, name: &str) -> bool {
        self.mentra
            .tools()
            .iter()
            .any(|descriptor| descriptor.provider.name == name)
    }
}

/// Eight hex characters of FNV-1a over the workspace root: stable across
/// processes, so the same collision resolves to the same name every run.
#[cfg(feature = "mcp")]
fn root_suffix(root: &Path) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    format!("{:08x}", (hash >> 32) as u32 ^ hash as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sharing across tasks and threads is the type's whole reason to exist,
    /// so it is asserted at compile time — the same pin `Workspace` carries.
    #[test]
    fn a_runtime_can_be_shared_across_tasks() {
        const fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<Runtime>();
        assert_send_sync::<RuntimeBuilder>();
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn a_taken_server_name_is_suffixed_and_a_released_one_is_free_again() {
        use std::path::Path;

        let runtime = Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds");

        let first = runtime.claim_mcp_server("fs", Path::new("/repo/one"));
        let second = runtime.claim_mcp_server("fs", Path::new("/repo/two"));
        let again = runtime.claim_mcp_server("fs", Path::new("/repo/two"));

        assert_eq!(first, "fs", "the first claimant keeps the plain name");
        assert_ne!(second, "fs", "the second must not collide in the registry");
        assert!(second.starts_with("fs-"), "{second}");
        assert_ne!(again, second, "every live claim is its own namespace");

        // Only the owner can free a name.
        runtime.release_mcp_claim("fs", Path::new("/repo/two"));
        runtime.release_mcp_claim(&second, Path::new("/repo/one"));
        assert_eq!(
            runtime.claim_mcp_server("fs", Path::new("/repo/three")),
            format!("fs-{}", root_suffix(Path::new("/repo/three"))),
            "a name someone else holds stays held"
        );

        runtime.release_mcp_claim("fs", Path::new("/repo/one"));
        assert_eq!(
            runtime.claim_mcp_server("fs", Path::new("/repo/four")),
            "fs",
            "a released name is claimable plain again"
        );
    }

    /// The case mentra's audience ladder cannot answer: two live opens of one
    /// directory share one audience, so a sibling's bridged tools resolve
    /// `Visible` for either of them. What tells them apart is which servers
    /// each open actually configured, which is what this asks.
    #[cfg(feature = "mcp")]
    #[test]
    fn a_bridged_tool_is_foreign_to_every_open_that_did_not_configure_its_server() {
        use std::path::Path;

        let runtime = Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds");

        // One repository, opened twice: the first client supplied an
        // authenticated server, the second supplied none. Same root, so the
        // same audience.
        let root = Path::new("/repo");
        let server = runtime.claim_mcp_server("prod-db", root);
        runtime.record_bridged_tools(&server, root, vec!["mcp__prod-db__query".to_string()]);

        assert_eq!(
            runtime
                .foreign_mcp_tools(&[])
                .into_iter()
                .collect::<Vec<_>>(),
            ["mcp__prod-db__query"],
            "the open that configured no servers must not be offered the other's"
        );
        assert!(
            runtime
                .foreign_mcp_tools(std::slice::from_ref(&server))
                .is_empty(),
            "and the open that configured it keeps it"
        );

        // Released with its workspace: a name nothing serves is nobody's to
        // hide.
        runtime.release_mcp_claim(&server, root);
        assert!(runtime.foreign_mcp_tools(&[]).is_empty());
    }

    /// A host tool registered globally under an `mcp__`-shaped name is visible
    /// to every audience by the rule that makes globals global — which is not
    /// the rule a name shaped like somebody's bridged server tool should get.
    #[cfg(feature = "mcp")]
    #[test]
    fn a_global_tool_shaped_like_a_bridged_one_is_foreign_to_every_workspace() {
        use mentra::tool::{
            ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
        };
        use serde_json::{Value, json};

        struct HostAdmin;

        impl ToolDefinition for HostAdmin {
            fn descriptor(&self) -> RuntimeToolDescriptor {
                RuntimeToolDescriptor::builder("mcp__internal__admin")
                    .description("the host's own tool")
                    .input_schema(json!({"type": "object"}))
                    .build()
            }
        }

        #[async_trait::async_trait]
        impl ToolExecutor for HostAdmin {
            async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
                Ok("administered".to_string())
            }
        }

        let runtime = Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .with_tool(HostAdmin)
            .build()
            .expect("builds");

        assert_eq!(
            runtime
                .foreign_mcp_tools(&[])
                .into_iter()
                .collect::<Vec<_>>(),
            ["mcp__internal__admin"],
            "no workspace configured a server called `internal`"
        );
    }
}
