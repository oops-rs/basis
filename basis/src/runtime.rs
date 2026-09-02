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
//!
//! # What is where
//!
//! This file is the noun: the [`Runtime`] itself, what a builder settled on it,
//! and the readers a workspace asks. The three questions sharing one costs
//! answers to live beside it, because each is a subject of its own and this
//! file was carrying all four:
//!
//! - `scope` — what a workspace's sessions run *as*. The policy, the tool
//!   audience and the persisted identity mentra keeps in no agent, restated on
//!   every mint and every resume.
//! - `claims` — who holds what on the single tool and skill registries a
//!   shared runtime carries, and what a collision means for each.
//! - `interception` — who judges a call: the host's global guards, and each
//!   workspace's own chain, holder-counted per audience.
//! - `agents` — which workspace each live agent answers for. The one question
//!   an audience cannot answer, because two opens of one directory share one.

pub(crate) mod agents;
pub(crate) mod builder;
mod claims;
mod credential;
mod executor;
mod interception;
#[cfg(test)]
pub(crate) mod probe;
mod scope;
mod tool_results;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mentra::ModelSelector;

pub use builder::RuntimeBuilder;
pub use tool_results::ToolResultPolicy;

use claims::DeclaredClaim;
pub(crate) use claims::DeclaredToolOrigin;
#[cfg(feature = "mcp")]
use claims::McpClaim;
use interception::HookChainClaim;
pub(crate) use interception::HookChainHold;
pub(crate) use scope::SessionScope;

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

use builder::execution::PolicyShaping;

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
    /// Which workspace each live agent minted, resumed or delegated to here
    /// answers for. See [`agents`].
    ///
    /// An `Arc` because two things outside this type read it and neither may
    /// hold the runtime: the `spawn` tool mentra's registry owns — an
    /// `Arc<Runtime>` there would be a cycle through the registry and the
    /// runtime would never drop — and each workspace's interception chain.
    agents: Arc<agents::AgentRegistry>,
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

    /// Which workspace each live agent here answers for, for the two readers
    /// that cannot ask an audience: a workspace's own interception chain, and
    /// `spawn`. See [`agents`].
    pub(crate) fn agents(&self) -> &Arc<agents::AgentRegistry> {
        &self.agents
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
}
