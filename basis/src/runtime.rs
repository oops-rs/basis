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
pub(crate) mod dispatch;
mod executor;
mod reuse;
mod tool_results;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mentra::{ModelSelector, Session, agent::AgentConfig, runtime::SessionOptions};

pub use builder::RuntimeBuilder;
pub use reuse::RuntimeRecipe;
pub use tool_results::ToolResultPolicy;

/// The types a **command target's executor** is written against.
///
/// Re-exported for the reason [`CancellationToken`](crate::CancellationToken)
/// is, and under the same rule: every mentra type basis's surface makes a
/// caller *name*, basis re-exports. [`RuntimeBuilder::with_command_target`]
/// asks for an `impl RuntimeExecutor`, so without these a host could not write
/// one without adding mentra to its own manifest and pinning the same version —
/// a skew there fails to compile with no hint that two crates disagree about
/// one trait. The sibling of [`crate::tools`]'s tool-authoring re-exports, in
/// the module that owns this seam.
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

use dispatch::{HookDispatch, HookRegistration, WorkspaceGuardEntry};

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
    /// Default retry schedule for runs minted here, from
    /// [`RuntimeBuilder::with_provider_retry`].
    ///
    /// Runtime-scoped for ADR-0018's reason: it describes the *connection* to
    /// the provider, like the credential and the base URL beside it, and not
    /// what one prompt may spend. Kept here because mentra takes it on each
    /// run's options rather than on its runtime, so this is the value every
    /// [`PreparedRun`](crate::PreparedRun) minted on this runtime copies. A
    /// turn may override it through [`TurnOptions`](crate::TurnOptions).
    provider_retry: ProviderRetry,
    /// Default retry count after the initial provider call, from
    /// [`RuntimeBuilder::with_provider_retry_budget`]. Kept beside the
    /// schedule and travelling with it for the same reason: mentra splits the
    /// count from the waits, and a runtime that widened one without the other
    /// would be half a statement.
    provider_retry_budget: usize,
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
    /// The one pre-hook basis registered, and the registry workspaces join.
    dispatch: Arc<HookDispatch>,
    /// Which workspace owns each MCP server name on this runtime's single tool
    /// registry — bridged tools are namespaced by server, so two workspaces
    /// configuring one name must be told apart here.
    #[cfg(feature = "mcp")]
    mcp_claims: Mutex<HashMap<String, PathBuf>>,
    /// Which workspace owns each workspace-scoped tool name on the same single
    /// registry. Declared subprocess tools and native workspace host tools
    /// share it so neither can overwrite or leak past the other.
    workspace_tool_claims: Mutex<HashMap<String, WorkspaceToolClaim>>,
    /// How many open workspaces hold each skills root on this runtime's single
    /// skill registry. See [`Runtime::register_skill_roots`].
    skill_root_holders: Mutex<HashMap<PathBuf, usize>>,
}

/// The identity a skills root is counted under.
///
/// mentra matches a root by its canonical path where the filesystem can
/// resolve one and by the exact path it was registered with otherwise
/// (`runtime/skill/registry.rs`'s `root_key`), so the holder count has to be
/// keyed the same way: two workspaces reaching the user's global root through
/// different spellings are one root upstream and must be one entry here, or
/// the first of them to drop would free what the second is still serving.
fn skill_root_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A workspace-scoped tool name registered on this runtime by a workspace
/// still open.
///
/// `holders` rather than a bare owner because one root may be open twice — a
/// host that opens the same repository for two concurrent callers — and the
/// first of those to drop must not free a name the second is still serving.
/// The entry goes when the count reaches zero, together with the tool itself.
#[derive(Debug)]
struct WorkspaceToolClaim {
    root: PathBuf,
    holders: usize,
    /// Only declared tools may join an existing same-root registration. Native
    /// tools are opaque values, so Basis cannot prove two implementations are
    /// the same merely because their public names match.
    share_same_root: bool,
}

#[derive(Clone, Copy)]
enum WorkspaceToolClaimPosture {
    ShareSameRoot,
    Exclusive,
}

enum WorkspaceToolRelease {
    Registered,
    ClaimOnly,
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

    /// Basis-internal access that does not cross the reusable lifecycle
    /// boundary. Public callers reach [`mentra_runtime`](Self::mentra_runtime);
    /// a reusable [`Workspace`](crate::Workspace) poisons its generation before
    /// handing that same reference out.
    pub(crate) fn mentra_runtime_internal(&self) -> &mentra::Runtime {
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

    /// The retry schedule and retry-count fallbacks every run minted on this
    /// runtime carries.
    ///
    /// Read at mint by `Workspace::minted`, which is what makes a
    /// runtime-scoped knob reach a per-run option. The two travel together
    /// because they are one statement about one provider connection.
    pub(crate) fn provider_retry(&self) -> (ProviderRetry, usize) {
        (self.provider_retry, self.provider_retry_budget)
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

    /// The one place a workspace's sessions are created.
    ///
    /// `persist_identifier` is the workspace tag the session's rows carry
    /// ([`store::runtime_identifier`](crate::store::runtime_identifier)), and
    /// it is applied per session rather than per runtime: on a shared runtime
    /// every workspace's sessions would otherwise be tagged with the one
    /// identifier fixed at build, and a per-workspace listing could not tell
    /// them apart. The private path is unaffected — its runtime-wide
    /// identifier already is this value, so the override is a no-op there.
    pub(crate) fn mint(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
        persist_identifier: &str,
    ) -> Result<Session, RunError> {
        let options = SessionOptions {
            config,
            project_id: None,
            runtime_identifier: Some(Arc::from(persist_identifier)),
        };

        Ok(self
            .mentra
            .create_session_with_options(name, model, options)?)
    }

    /// The one place a workspace's sessions are resumed; see
    /// [`mint`](Self::mint) for why it is a place at all.
    pub(crate) fn resume_minted(&self, agent_id: &str) -> Result<Session, RunError> {
        Ok(self.mentra.resume_session(agent_id)?)
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

    /// The host interceptors this runtime was built with, for the workspace
    /// open that folds them ahead of its own hooks.
    pub(crate) fn interceptors(&self) -> &[Arc<dyn crate::hooks::Interceptor>] {
        self.dispatch.interceptors()
    }

    /// Joins a workspace to this runtime's hook dispatcher. The registration
    /// deregisters on drop, which is how a dropped workspace stops being
    /// consulted.
    pub(crate) fn register_workspace(&self, entry: WorkspaceGuardEntry) -> HookRegistration {
        self.dispatch.register(entry)
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
            claims.insert(name.to_string(), root.to_path_buf());
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

        claims.insert(effective.clone(), root.to_path_buf());
        effective
    }

    /// Releases a claim [`claim_mcp_server`](Self::claim_mcp_server) granted.
    /// Only the owning root can release, so one workspace's drop cannot free a
    /// name another still serves.
    #[cfg(feature = "mcp")]
    pub(crate) fn release_mcp_claim(&self, name: &str, root: &Path) {
        let mut claims = self.mcp_claims.lock().expect("mcp claim map poisoned");
        if claims.get(name).is_some_and(|owner| owner == root) {
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
    pub(crate) fn claim_declared_tool(&self, name: &str, root: &Path) -> Result<bool, String> {
        self.claim_workspace_tool(name, root, WorkspaceToolClaimPosture::ShareSameRoot)
    }

    /// Exclusively claims a native host tool for one workspace.
    pub(crate) fn claim_host_tool(&self, name: &str, root: &Path) -> Result<(), String> {
        let fresh = self.claim_workspace_tool(name, root, WorkspaceToolClaimPosture::Exclusive)?;
        debug_assert!(fresh);
        Ok(())
    }

    fn claim_workspace_tool(
        &self,
        name: &str,
        root: &Path,
        posture: WorkspaceToolClaimPosture,
    ) -> Result<bool, String> {
        let mut claims = self
            .workspace_tool_claims
            .lock()
            .expect("workspace tool claim map poisoned");

        match claims.get_mut(name) {
            Some(claim) if claim.root != root => Err(format!(
                "the workspace at {} is open on this runtime and owns a tool by that name",
                claim.root.display()
            )),
            Some(claim)
                if matches!(posture, WorkspaceToolClaimPosture::ShareSameRoot)
                    && claim.share_same_root =>
            {
                claim.holders += 1;
                Ok(false)
            }
            Some(_) => Err(format!(
                "the workspace at {} already owns a different tool by that name",
                root.display()
            )),
            None if self.registers_tool(name) => {
                Err("this runtime already offers a tool by that name".to_string())
            }
            None => {
                claims.insert(
                    name.to_string(),
                    WorkspaceToolClaim {
                        root: root.to_path_buf(),
                        holders: 1,
                        share_same_root: matches!(
                            posture,
                            WorkspaceToolClaimPosture::ShareSameRoot
                        ),
                    },
                );
                Ok(true)
            }
        }
    }

    /// Releases a declared or native workspace-tool claim, taking the tool off
    /// the runtime when the last holder goes.
    ///
    /// Only the owning root can release, so one workspace's drop cannot free a
    /// name another still serves. The unregister is what makes the claim map
    /// and mentra's registry say the same thing: a released name is free
    /// because nothing answers to it any more, rather than free-with-a-stale-
    /// entry-behind-it. Before mentra's unregister was public a claim had to be
    /// remembered with `holders: 0` forever, and every dropped workspace left a
    /// tool on a registry a host keeps for its whole process.
    pub(crate) fn release_workspace_tool(&self, name: &str, root: &Path) {
        self.release_workspace_tool_claim(name, root, WorkspaceToolRelease::Registered);
    }

    /// Releases a claim that never reached Mentra's registry.
    pub(crate) fn abandon_workspace_tool_claim(&self, name: &str, root: &Path) {
        self.release_workspace_tool_claim(name, root, WorkspaceToolRelease::ClaimOnly);
    }

    fn release_workspace_tool_claim(&self, name: &str, root: &Path, release: WorkspaceToolRelease) {
        let mut claims = self
            .workspace_tool_claims
            .lock()
            .expect("workspace tool claim map poisoned");

        let Some(claim) = claims.get_mut(name) else {
            return;
        };
        if claim.root != root {
            return;
        }

        claim.holders = claim.holders.saturating_sub(1);
        if claim.holders == 0 {
            claims.remove(name);
            if matches!(release, WorkspaceToolRelease::Registered) {
                // Under the claim lock, so no other claimant can see the name
                // free while the tool is still registered.
                self.mentra.unregister_tool(name);
            }
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
    /// [`claim_declared_tool`](Self::claim_declared_tool).
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

    /// Every workspace-scoped tool name on this runtime that belongs to some
    /// *other* workspace still open.
    ///
    /// What a mint hides, for the reason it hides another workspace's `mcp__*`
    /// tools: the registry is the runtime's and single, but a tool declared by
    /// a repository is that repository's, and offering it to a run in a
    /// different one would run a capability that workspace never asked for.
    pub(crate) fn foreign_workspace_tools(&self, root: &Path) -> Vec<String> {
        self.workspace_tool_claims
            .lock()
            .expect("workspace tool claim map poisoned")
            .iter()
            .filter(|(_, claim)| claim.root != root)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether mentra's registry already answers to `name` — a builtin,
    /// basis's own `spawn`, or a bridged MCP tool.
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
}
