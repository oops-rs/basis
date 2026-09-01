//! Opening a workspace: everything a run should only have to discover once.
//!
//! This is the resolution that used to happen inside `prepare()`, per run —
//! context discovery, model resolution, skill registration, template loading,
//! hook loading, MCP connection. ADR-0010 asked for it to happen once and for
//! runs to be minted from the result, because a twenty-agent fan-out should
//! read `AGENTS.md` once rather than twenty times, and should not open twenty
//! copies of every MCP server.
//!
//! What opening does **not** settle anymore is the process: ADR-0018 moved the
//! provider, the credential, the store policy, and the host's interceptors to
//! [`RuntimeBuilder`](crate::RuntimeBuilder). A workspace either borrows a
//! shared [`Runtime`](crate::Runtime) ([`with_runtime`](WorkspaceBuilder::with_runtime))
//! or carries a recipe for a private one
//! ([`with_runtime_builder`](WorkspaceBuilder::with_runtime_builder)), and the
//! bare `Workspace::open(path)` is the second of those with every default —
//! byte-identical to what it always did.
//!
//! Everything settled here is settled for the life of the [`Workspace`]. What a
//! caller can still change per run lives in [`RunSpec`](super::RunSpec).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mentra::{ModelInfo, ModelSelector};

#[cfg(feature = "mcp")]
use crate::mcp::{self, McpConfig, connections::McpConnections};
use crate::{
    compaction::Compaction,
    config::{self, Config},
    context::{ContextConfig, SystemPrompt, WorkspaceContext},
    error::RunError,
    event::ContextFile,
    hooks::{self, HookRunner, HooksConfig},
    memory::{self, MemoryConfig},
    run::LoadedSkill,
    runtime::{Runtime, RuntimeBuilder, RuntimeRecipe, dispatch},
    shell::ShellAccess,
    skills::{self, SkillRoots, SkillsConfig},
    store,
    templates::{self, Template, TemplatesConfig},
    tools::{
        declared::{self, DeclaredTools, ToolsConfig},
        host::{HostToolBinding, WorkspaceHostTools},
    },
};

use super::{Workspace, WorkspaceReuse, lifecycle::MintPosture, roster::ToolRoster};

/// How a workspace is opened.
///
/// Named a builder rather than a config because it is one: it exists to be
/// filled in and then consumed by [`open`](Self::open). The type mentra calls
/// `WorkspaceConfig` is a different thing entirely — the agent's base directory
/// — and basis sets that from this one rather than exposing it.
///
/// Fields are private because the
/// embedded runtime recipe can hold a credential. `with_*` returns a new
/// value, so a host can keep a half-configured builder and finish it
/// differently per workspace.
pub struct WorkspaceBuilder {
    path: PathBuf,
    runtime: RuntimeSource,
    /// One coherent, sticky switch over every repository/home convention.
    discovery_enabled: bool,
    /// Whether this workspace permits only one independent prepare/resume.
    fresh_only: bool,
    /// Inherited policy, a selector override, or complete host-resolved metadata.
    model: WorkspaceModel,
    context: ContextConfig,
    /// What `config.json` said; `None` means discover it at
    /// [`open`](WorkspaceBuilder::open).
    config: Option<Config>,
    /// The host's own say over the system prompt; `None` is discovery alone.
    system_prompt: Option<SystemPrompt>,
    skills: SkillsConfig,
    memory: MemoryConfig,
    /// Which tools the model is offered (decision D3). `ToolRoster::default()`
    /// unless a caller says otherwise.
    roster: ToolRoster,
    #[cfg(feature = "mcp")]
    mcp: McpConfig,
    templates: TemplatesConfig,
    hooks: HooksConfig,
    tools: ToolsConfig,
    /// Native tools owned by this workspace rather than by its runtime.
    host_tools: Vec<Box<dyn crate::tools::ExecutableTool>>,
    shell: ShellAccess,
    compaction: Compaction,
}

/// The one mutually-exclusive source of this workspace's model.
///
/// A sum rather than parallel optional fields makes last-call-wins exact: a
/// selector and resolved metadata cannot both survive on one builder.
#[derive(Debug)]
enum WorkspaceModel {
    Inherited,
    Selector(ModelSelector),
    Resolved(ModelInfo),
}

/// Where this workspace's runtime comes from: borrowed from the host, or
/// built privately from a recipe, bound to this workspace's path.
///
/// The recipe is boxed because it is two orders of magnitude larger than the
/// `Arc` beside it — a provider, a credential, a history policy, an
/// interceptor list, a command environment and a target map — and every
/// `WorkspaceBuilder` would otherwise carry room for all of it whether or not
/// it holds one.
enum RuntimeSource {
    Shared(Arc<Runtime>),
    Private(Box<RuntimeBuilder>),
    Reusable(Box<RuntimeRecipe>),
}

/// Hand-written for the reason [`RuntimeBuilder`]'s is: the private recipe can
/// hold a credential, and its own `Debug` redacts it.
impl std::fmt::Debug for WorkspaceBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceBuilder")
            .field("path", &self.path)
            .field(
                "runtime",
                match &self.runtime {
                    RuntimeSource::Shared(runtime) => runtime,
                    RuntimeSource::Private(recipe) => &**recipe,
                    RuntimeSource::Reusable(recipe) => recipe,
                },
            )
            .field("discovery_enabled", &self.discovery_enabled)
            .field("fresh_only", &self.fresh_only)
            .field("model", &self.model)
            .field("context", &self.context)
            .field("config", &self.config)
            .field("system_prompt", &self.system_prompt)
            .field("skills", &self.skills)
            .field("memory", &self.memory)
            .field("roster", &self.roster)
            .field("templates", &self.templates)
            .field("hooks", &self.hooks)
            .field("tools", &self.tools)
            .field(
                "host_tools",
                &format_args!("{} tools", self.host_tools.len()),
            )
            .field("shell", &self.shell)
            .field("compaction", &self.compaction)
            .finish_non_exhaustive()
    }
}

impl WorkspaceBuilder {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            // A private default runtime, so the one-repository host never sees
            // the third noun (ADR-0018): `Workspace::open(path)` behaves as it
            // always has.
            runtime: RuntimeSource::Private(Box::default()),
            discovery_enabled: true,
            fresh_only: false,
            model: WorkspaceModel::Inherited,
            context: ContextConfig::default(),
            // Unset, so `open` reads the convention where convention says it
            // is — the same default every other discovery on this builder has.
            config: None,
            // Unset, so the prompt is what the workspace says and nothing else.
            // basis ships no system prompt of its own (PROPOSAL.md Bet 4) and
            // a seam is not a default.
            system_prompt: None,
            skills: SkillsConfig::default(),
            memory: MemoryConfig::default(),
            // D3: today's exact hidden set, so `Workspace::open(path)` offers
            // precisely what it always has.
            roster: ToolRoster::default(),
            #[cfg(feature = "mcp")]
            mcp: McpConfig::default(),
            templates: TemplatesConfig::default(),
            hooks: HooksConfig::default(),
            tools: ToolsConfig::default(),
            host_tools: Vec::new(),
            // Granted, per ADR-0013, and from the enum's own default rather
            // than from anything ambient: what a run may do is stated here, in
            // configuration, not read out of the environment behind the caller.
            shell: ShellAccess::default(),
            // Keeps every tool result the model was shown, and leaves mentra's
            // summarizing numbers where mentra put them (see
            // [`crate::compaction`]).
            compaction: Compaction::default(),
        }
    }

    /// Borrows the host's runtime instead of building a private one.
    ///
    /// The N-repository shape: one [`Runtime`] built once, every workspace
    /// opened with a clone of the `Arc`. Provider, credential, store, and
    /// host interceptors are the runtime's facts and cannot be re-said here;
    /// what this workspace still decides is what its repository says, plus the
    /// [`with_model`](Self::with_model) override and its command posture.
    pub fn with_runtime(self, runtime: Arc<Runtime>) -> Self {
        Self {
            runtime: RuntimeSource::Shared(runtime),
            ..self
        }
    }

    /// Supplies the recipe for this workspace's private runtime.
    ///
    /// [`open`](Self::open) builds it bound to this workspace's path — the
    /// per-path persist identifier and workspace-bounded policy the bare
    /// `Workspace::open` has always produced — so this is *configuring* the
    /// sugar, not switching shapes. It is also the migration path for every
    /// knob ADR-0018 moved: a one-shot caller that needs an interceptor or a
    /// store directory puts it on a [`RuntimeBuilder`](crate::RuntimeBuilder)
    /// and hands it here.
    pub fn with_runtime_builder(self, runtime: RuntimeBuilder) -> Self {
        Self {
            runtime: RuntimeSource::Private(Box::new(runtime)),
            ..self
        }
    }

    /// Supplies the repeatable private-runtime recipe for consume/rebuild.
    ///
    /// Opening this source is intentionally stricter than an ordinary private
    /// builder: discovery must be disabled, fresh-only must be explicit, and
    /// complete resolved model metadata must be supplied. The resulting
    /// workspace starts unbound and cannot mint until
    /// [`Workspace::bind_host_tools`](crate::Workspace::bind_host_tools)
    /// consumes it with the checkout's complete host-tool set (including an
    /// explicitly empty set).
    ///
    /// The supported reuse proof is intentionally narrower than everything
    /// Mentra can run: a discovery-off host supplies an exact allow-list
    /// roster and does not escape through raw Mentra APIs or execute team,
    /// background, `spawn`, detached custom-tool, or other work whose lifetime
    /// Basis cannot track. Such use poisons reuse where Basis can observe the
    /// escape; the remaining execution limits are part of the host contract,
    /// not inferred cleanup.
    #[must_use]
    pub fn with_runtime_recipe(self, recipe: RuntimeRecipe) -> Self {
        Self {
            runtime: RuntimeSource::Reusable(Box::new(recipe)),
            ..self
        }
    }

    /// Allows exactly one independent [`Workspace::prepare`] or
    /// [`Workspace::resume`] attempt from the opened workspace.
    ///
    /// Subsequent turns on the returned [`crate::PreparedRun`] remain
    /// attached and unrestricted. The claim is irreversible even if the first
    /// attempt fails: without Gate 1b's scrub contract, Basis cannot prove a
    /// partly minted/resumed runtime is clean enough to retry.
    ///
    /// Requires a private runtime recipe. A shared runtime could be minted by
    /// another workspace through a different `Arc`, bypassing this workspace's
    /// gate, so [`open`](Self::open) refuses that ownership shape.
    /// Direct session creation through [`Workspace::mentra_runtime`] is the
    /// raw Mentra escape hatch and is outside this supported Basis lifecycle.
    #[must_use]
    pub fn fresh_only(self) -> Self {
        Self {
            fresh_only: true,
            ..self
        }
    }

    /// Overrides the runtime's model policy, for this workspace alone.
    ///
    /// Unset, the runtime's [`with_model`](crate::RuntimeBuilder::with_model)
    /// policy decides. Either way the *resolved* model is this workspace's
    /// fact, fixed at open and reported by every run it mints.
    pub fn with_model(self, model: ModelSelector) -> Self {
        Self {
            model: WorkspaceModel::Selector(model),
            ..self
        }
    }

    /// Supplies the complete model metadata this workspace must use.
    ///
    /// Unlike [`with_model`](Self::with_model), this is an answer rather than
    /// a selection policy: [`open`](Self::open) does not list or resolve
    /// models. The metadata, including its context window, reaches every
    /// session minted by the workspace unchanged.
    ///
    /// The model must name the same provider as the workspace's runtime. A
    /// mismatch is refused by [`open`](Self::open) before provider or tool
    /// activity with
    /// [`RunError::ResolvedModelProviderMismatch`](crate::RunError::ResolvedModelProviderMismatch).
    /// Calling this after [`with_model`](Self::with_model), or vice versa,
    /// replaces the earlier value.
    #[must_use]
    pub fn with_resolved_model(self, model: ModelInfo) -> Self {
        Self {
            model: WorkspaceModel::Resolved(model),
            ..self
        }
    }

    pub fn with_context(self, context: ContextConfig) -> Self {
        Self { context, ..self }
    }

    /// Disables every repository- and home-discovered input as one posture.
    ///
    /// Opening still validates and resolves the workspace path, and explicit
    /// host inputs still apply: a supplied [`Config`], private runtime recipe
    /// and provider, model, system prompt, native tools, roster, interceptors,
    /// shell posture, and compaction. What stops is file discovery and the work
    /// caused by it: context, config, hooks, declared tools, memory, skills,
    /// templates, and MCP files/connections are not probed.
    ///
    /// Sticky by construction: no source-specific `with_*` setter changes this
    /// private flag, so calling one later cannot accidentally reopen a file
    /// input. Build a fresh builder to restore the default discovery posture.
    ///
    /// This posture requires a private runtime recipe supplied through
    /// [`with_runtime_builder`](Self::with_runtime_builder). A borrowed runtime
    /// is mutable through every other `Arc` holder, while Mentra reads its
    /// runtime-global skill descriptions on every round; refusing
    /// [`with_runtime`](Self::with_runtime) is the only race-free way Gate 1a's
    /// fresh-only lifecycle can guarantee that no later registration widens
    /// the prompt or roster through Basis's builder surface. A caller that
    /// subsequently mutates [`Workspace::mentra_runtime`] has deliberately
    /// left this contract through the raw Mentra escape hatch.
    #[must_use]
    pub fn without_discovery(self) -> Self {
        Self {
            discovery_enabled: false,
            ..self
        }
    }

    /// Supplies the `config.json` answers instead of discovering them.
    ///
    /// Unset, [`open`](Self::open) reads `.basis/config.json` and the global
    /// `config.json` itself, because opening a path is what reads a
    /// repository's conventions — the same reason it reads `AGENTS.md` and
    /// `.mcp.json` without being asked to.
    ///
    /// Two callers want to say otherwise. A host that already discovered a
    /// [`Config`] — to report it, or to apply its process half to a shared
    /// [`Runtime`](crate::Runtime) with
    /// [`RuntimeBuilder::with_config`](crate::RuntimeBuilder::with_config) —
    /// hands the same value here rather than paying for the read twice. And
    /// `Config::default()` says *nothing*, which is how a host that wants its
    /// own configuration to be the only configuration turns the file off.
    ///
    /// Whatever arrives still loses to every explicit call on this builder and
    /// on the runtime's: this is the layer below them, never above.
    pub fn with_config(self, config: Config) -> Self {
        Self {
            config: Some(config),
            ..self
        }
    }

    /// Gives the host a say over the system prompt, for this workspace's runs.
    ///
    /// [`SystemPrompt::Append`] puts the host's text after the discovered
    /// context, as the most specific block; [`SystemPrompt::Replace`] makes it
    /// the whole prompt and leaves discovery out of it. Unset — the default —
    /// the prompt is the rendered context and nothing else.
    ///
    /// Workspace-level and not runtime-level, deliberately: a host serving
    /// several repositories off one shared [`Runtime`] (ADR-0018) can give each
    /// its own voice, and the prompt is settled at
    /// [`open`](Self::open) into the workspace's own `AgentConfig`, so runs
    /// minted from different workspaces cannot pick up each other's.
    ///
    /// One field, so the last call wins — and the enum makes *both at once*
    /// unspellable rather than undefined.
    pub fn with_system_prompt(self, system_prompt: SystemPrompt) -> Self {
        Self {
            system_prompt: Some(system_prompt),
            ..self
        }
    }

    pub fn with_skills(self, skills: SkillsConfig) -> Self {
        Self { skills, ..self }
    }

    /// Sets where memory files are discovered, or turns discovery off.
    ///
    /// Memory is files, not a subsystem — see [`crate::memory`] for the
    /// convention, the two default roots, and what the index costs. Unset,
    /// the convention applies: the global config directory's `memory/`, plus
    /// the sibling `memory/` beside the runtime's store dir when
    /// [`RuntimeBuilder::with_store_dir`](crate::RuntimeBuilder::with_store_dir)
    /// named one. [`MemoryConfig::disabled`] reads nothing at all.
    pub fn with_memory(self, memory: MemoryConfig) -> Self {
        Self { memory, ..self }
    }

    /// Sets which tools the model is offered, for every run this workspace
    /// mints (decision D3).
    ///
    /// Unset, [`ToolRoster::default`] applies: exactly what every workspace
    /// has offered — `spawn`'s replaced doors and basis's never-surfaced
    /// intrinsics hidden, everything else offered. Neither constructor on
    /// [`ToolRoster`] changes what is *registered* on the runtime; see its
    /// module docs for the two things that still apply on top of whatever
    /// roster is set here — a per-mint hide of a sibling workspace's tools,
    /// and the rendered prompt, which has no opinion about the roster at all.
    pub fn with_tool_roster(self, roster: ToolRoster) -> Self {
        Self { roster, ..self }
    }

    /// Sets which MCP servers this workspace connects.
    ///
    /// Servers arrive from three places — the caller's own list, the
    /// workspace's `.mcp.json`, and the global one — and this is where the
    /// first of those goes. See [`crate::mcp`] for the precedence.
    ///
    /// The connections are opened once, by [`open`](Self::open), owned by the
    /// workspace, and shared by every run minted from it — on a shared runtime
    /// they die with this workspace, not with the runtime (ADR-0018).
    #[cfg(feature = "mcp")]
    pub fn with_mcp(self, mcp: McpConfig) -> Self {
        Self { mcp, ..self }
    }

    pub fn with_templates(self, templates: TemplatesConfig) -> Self {
        Self { templates, ..self }
    }

    /// Sets the host-supplied subprocess hooks and where file hooks are
    /// discovered.
    ///
    /// A hook is an external command that gets a say over each tool call; see
    /// [`crate::hooks`] for the wire contract and for what happens when one
    /// breaks. [`RuntimeBuilder::with_interceptor`](crate::RuntimeBuilder::with_interceptor)
    /// is the same say, in the host's process — host scope is runtime scope.
    /// Typed [`HooksConfig::supplied`](crate::hooks::HooksConfig::supplied)
    /// hooks run before global and workspace file hooks; disabling discovery
    /// retains only that typed list.
    pub fn with_hooks(self, hooks: HooksConfig) -> Self {
        Self { hooks, ..self }
    }

    /// Sets the host-supplied declared tools and where file declarations are
    /// discovered.
    ///
    /// A declared tool is a command the workspace offers the *model* as a tool,
    /// with a JSON schema for its input; see [`crate::tools::declared`] for the
    /// manifest and for what a failing one tells the model. The tools are
    /// registered on the runtime this workspace borrows and deregistered — as
    /// far as mentra's registry allows — when the workspace drops, so a
    /// repository's tools never reach another repository's runs. Typed
    /// [`ToolsConfig::supplied`](crate::tools::declared::ToolsConfig::supplied)
    /// entries outrank workspace and global files and remain active when file
    /// discovery is disabled.
    pub fn with_tools(self, tools: ToolsConfig) -> Self {
        Self { tools, ..self }
    }

    /// Registers a native tool for this workspace only.
    ///
    /// Unlike [`RuntimeBuilder::with_tool`](crate::RuntimeBuilder::with_tool),
    /// this tool is owned by the workspace: it is offered to this workspace's
    /// runs and hidden from siblings borrowing the same runtime. The public
    /// name is claimed without suffixing and is released, with the Mentra
    /// registration, when the workspace drops.
    ///
    /// Reusable runtime recipes bind their complete per-checkout set through
    /// [`Workspace::bind_host_tools`](crate::Workspace::bind_host_tools)
    /// instead; mixing the incremental builder form with that complete binding
    /// is refused at open.
    pub fn with_host_tool<T>(self, tool: T) -> Self
    where
        T: crate::tools::ExecutableTool + 'static,
    {
        Self {
            host_tools: {
                let mut tools = self.host_tools;
                tools.push(Box::new(tool));
                tools
            },
            ..self
        }
    }

    /// Grants or denies command execution, for every run this workspace mints.
    ///
    /// Granted by default (ADR-0013). Denying is the read-only posture: it
    /// shuts the command tools and nothing else, so it is a narrowing of what
    /// these runs do, never a claim about what the process could do.
    ///
    /// Workspace-level because it is a statement about this repository's runs.
    /// On a private runtime it is baked into the runtime's policy; on a shared
    /// one — whose policy cannot vary per workspace — it is enforced by the
    /// runtime's hook dispatcher, which denies `spawn`'s command mode for this
    /// workspace's agents (see [`crate::runtime`]).
    pub fn with_shell(self, shell: ShellAccess) -> Self {
        Self { shell, ..self }
    }

    /// Sets how much of a conversation reaches the model, for every run this
    /// workspace mints.
    ///
    /// Unset, [`Compaction::default`] applies: every tool result the model was
    /// shown stays in front of it, and mentra's summarizing trigger is
    /// untouched. See [`crate::compaction`] for the two mechanisms and for why
    /// the default is what it is.
    ///
    /// Workspace-level, not runtime-level, and the reason is mechanical rather
    /// than aesthetic. These numbers live on mentra's `AgentConfig`, one is
    /// built per workspace by [`open`](Self::open)'s `agent_config`, and every
    /// session this workspace mints — and every subagent that clones its
    /// config — carries that one. A runtime-level knob would have to be read
    /// back out at the same moment anyway, and could not then be varied per
    /// repository, which ADR-0018's split is precisely about: the runtime owns
    /// what changes when the host changes, and how much history a repository's
    /// runs keep is not that.
    pub fn with_compaction(self, compaction: Compaction) -> Self {
        Self { compaction, ..self }
    }

    /// Does all of it: discovery, runtime acquisition, model, skills,
    /// templates, hooks, MCP connections.
    ///
    /// This is the expensive call, and the only one. Everything it settles is
    /// fixed for the life of the returned [`Workspace`]; a run minted from that
    /// workspace does no I/O of its own.
    ///
    /// # Where the workspace is
    ///
    /// The path is made absolute and canonical here, once, and that resolved
    /// directory is [`Workspace::root`] — a workspace opened as `.`, through a
    /// symlink, or with a `..` in it reports the directory those spellings
    /// name, not the spelling. Everything downstream takes that one value: the
    /// agent's base directory, the runtime's policy roots, the hook
    /// dispatcher's key, the store identifier, and the run header's
    /// `workspace`. Nothing resolves it again, so a process that changes its
    /// working directory afterwards changes nothing about a workspace already
    /// open. A path that does not exist, or is not a directory, fails the open
    /// here rather than at the first tool call.
    ///
    /// # What this workspace's conversations are tagged with
    ///
    /// Every agent persisted from here should carry
    /// [`store::runtime_identifier`](crate::store::runtime_identifier) for this
    /// workspace, which is what makes [`store::list`](crate::store::list) — and
    /// therefore ACP's `session/list` — able to answer *which conversations
    /// belong to this repository*. On a private runtime it does, exactly as
    /// before. On a shared runtime mentra 0.18 can only tag with the
    /// runtime-wide identifier fixed at build (`"basis:runtime"`), so rows minted
    /// there stay out of every per-workspace list until the per-session
    /// override lands upstream — see [`Runtime::mint`](crate::Runtime), which
    /// is the one line that changes. Mis-listing is the whole cost: mentra
    /// loads an agent by id alone, so resuming is unaffected, and an agent
    /// re-tags itself the next time it persists under a runtime that knows its
    /// workspace.
    ///
    /// # What sharing a runtime shares
    ///
    /// Skills are registered on the runtime's single registry, so a skill one
    /// workspace registers is loadable by another's runs for as long as both
    /// are open — an accepted consequence of sharing, and what
    /// [`Workspace::skills`] therefore reports. It ends with the workspace: the
    /// roots this open registered come off the runtime when the [`Workspace`]
    /// drops, so a sibling that outlives it stops being able to reach its
    /// skills, and a root two workspaces both registered — the user's global
    /// ones, on any host that opens more than one repository — stays until the
    /// last of them goes. MCP tools live on the same single registry but do
    /// **not** travel even while both are open: every roster minted here hides
    /// the `mcp__*` tools of servers this workspace does not own.
    pub async fn open(self) -> Result<Workspace, RunError> {
        if let RuntimeSource::Reusable(recipe) = &self.runtime {
            if !self.host_tools.is_empty() {
                return Err(RunError::ReusableWorkspaceRequiresHostToolBinding);
            }
            if self.discovery_enabled {
                return Err(RunError::ReusableWorkspaceRequiresDiscoveryOff);
            }
            if !self.fresh_only {
                return Err(RunError::ReusableWorkspaceRequiresFreshOnly);
            }
            let WorkspaceModel::Resolved(model) = &self.model else {
                return Err(RunError::ReusableWorkspaceRequiresResolvedModel);
            };
            if self.roster.as_profile().allowed_tools.is_none() {
                return Err(RunError::ReusableWorkspaceRequiresExactRoster);
            }
            if model.provider.as_str() != recipe.provider().as_str() {
                return Err(RunError::ResolvedModelProviderMismatch {
                    model: model.id.clone(),
                    model_provider: model.provider.as_str().to_string(),
                    runtime_provider: recipe.provider().as_str().to_string(),
                });
            }
        }

        // **The one resolution.** Everything below this line names the
        // workspace through `path` and nothing re-derives it: the dispatcher
        // key, the private runtime's policy roots, the agent's base directory,
        // the hook runner's directory, the store identifier and the run
        // header all take this value. A relative spelling would otherwise
        // survive into all of them and be resolved again — against whatever
        // the process's working directory happened to be at the time, which is
        // not a thing a workspace should depend on: mentra normalizes a policy
        // root at every check, not at construction
        // (`RuntimePolicy::normalize_policy_root`), so a relative root means
        // *the same* run answers differently after a `chdir`. Canonical, not
        // merely absolute, for the reason
        // [`store::runtime_identifier`](crate::store::runtime_identifier)
        // gives: a symlinked spelling and its target are one workspace.
        //
        // Placed after the reusable-posture checks so a misconfigured builder
        // is still refused before any filesystem question is asked of it.
        let path = crate::context::resolve_workspace(&self.path)?;

        let context = if self.discovery_enabled {
            WorkspaceContext::discover_with(&path, &self.context)?
        } else {
            // `none` skips every file candidate but deliberately retains
            // canonical workspace-path validation.
            WorkspaceContext::discover_with(&path, &ContextConfig::none())?
        };

        // A shared runtime can acquire a skill loader after any one-time
        // inspection, and Mentra appends that loader's descriptions on every
        // round independently of the agent roster. Reject the ownership shape
        // itself, immediately after the one operation discovery-off retains
        // (workspace validation), so the refusal precedes runtime acquisition,
        // model resolution and all provider/tool/interceptor activity.
        if !self.discovery_enabled && matches!(&self.runtime, RuntimeSource::Shared(_)) {
            return Err(RunError::DiscoveryDisabledSharedRuntime);
        }
        if self.fresh_only && matches!(&self.runtime, RuntimeSource::Shared(_)) {
            return Err(RunError::FreshOnlySharedRuntime);
        }
        let fresh_only = self.fresh_only;

        // Read before the runtime is acquired, for the reason the hooks file
        // below is: a config that does not parse must fail the open rather
        // than let a run reach a model nobody in this repository chose. The
        // global directory is the context config's, so one process cannot read
        // two different global directories.
        let config = match self.config {
            Some(config) => config,
            None if self.discovery_enabled => {
                config::Config::discover(&path, self.context.global_dir.as_deref())?
            }
            None => Config::default(),
        };

        // Loaded before the runtime is acquired so a hooks file that does not
        // parse fails the open loudly, rather than at the first tool call —
        // or worse, never.
        let loaded_hooks = if self.discovery_enabled {
            hooks::load(&path, &self.hooks)?
        } else {
            hooks::load_supplied(&self.hooks)?
        };

        // Validate supplied values and read files here for the same reason, and
        // one of their own: an invalid declaration is a tool the model's
        // instructions assume and will not find. Registering needs the runtime,
        // so that waits until there is one. Discovery-off keeps the typed list
        // and touches no manifest path.
        let supplied_tools = declared::load_supplied(&self.tools)?;
        let declared_sources = if self.discovery_enabled {
            declared::discover(&path, &self.tools)?
        } else {
            Vec::new()
        };

        // Memory, before the runtime is acquired for the reason the files
        // above are — a memory that does not parse fails the open naming the
        // file. The workspace root derives beside the runtime's store dir
        // ([`crate::memory`]), which on the private path is still a recipe, so
        // both shapes are asked before the match below consumes them. The
        // roots are resolved whether or not they exist yet: the private
        // runtime's policy names them (the model writes memories through the
        // ordinary file tools, and the roots sit outside the workspace), and
        // the first memory is written by exactly the run that finds none to
        // read.
        //
        // **`WorkspaceMemoryRoot::BesideStore` resolves only here, on the
        // private path.** A shared runtime's store dir is one runtime-wide
        // fact, not this workspace's — every workspace borrowing it would
        // derive the identical sibling `memory/` directory, and each would
        // read the others' memory index into its own prompt (worse than the
        // write-is-refused gap this replaces: that let the index render
        // anyway). `None` here is what makes `memory::roots` skip the
        // workspace root entirely on a shared runtime, exactly parallel to
        // the dispatcher's existing shared-runtime posture — it can deny, it
        // cannot grant a root the policy never named. The global root is
        // unaffected: every workspace's own memories are exactly that,
        // whichever runtime they borrow. An explicit
        // [`WorkspaceMemoryRoot::Dir`](crate::memory::WorkspaceMemoryRoot::Dir)
        // is unaffected either way — naming a path is the host taking
        // responsibility for it, shared runtime or not.
        let store_dir = match &self.runtime {
            RuntimeSource::Shared(_) => None,
            RuntimeSource::Private(recipe) => recipe.named_store_dir().map(Path::to_path_buf),
            RuntimeSource::Reusable(_) => None,
        };
        // This wave's own I/O — `roots`, the per-file reads `load` does, and
        // the `canonicalize` inside `crate::paths::same_dir` — goes to a
        // blocking thread (whole-wave review, G7): `basis-acp` cold-opens
        // workspaces on its shared runtime, and this is genuinely blocking
        // work the way `spawn_blocking`'s other callers already are
        // (`hooks/runner.rs`, `tools/declared/tool.rs`). The context, hooks
        // and declared-tools discovery just above stay sync on purpose —
        // they predate this wave and are not what it added, so smoothing the
        // asymmetry away here would be a second refactor nobody asked for.
        let memory_config = self.memory;
        let (memory_sources, memories) = if self.discovery_enabled {
            tokio::task::spawn_blocking(move || {
                let memory_sources = memory::roots(&memory_config, store_dir.as_deref());
                let memories = memory::load(&memory_sources)?;
                Ok::<_, memory::MemoryError>((memory_sources, memories))
            })
            .await
            .map_err(RunError::MemoryDiscovery)??
        } else {
            (Vec::new(), Vec::new())
        };
        let memory_roots: Vec<PathBuf> = memory_sources
            .iter()
            .map(|source| source.path.clone())
            .collect();

        let shared = matches!(self.runtime, RuntimeSource::Shared(_));
        let (runtime, reusable_recipe) = match self.runtime {
            // A shared runtime's provider, credential and endpoint are the
            // host's process facts and were settled before this workspace
            // existed, so a file's `provider` and `base_url` have nothing to
            // reach here — the host that shares a runtime is the one that
            // decided the connection, and applies `RuntimeBuilder::with_config`
            // itself if it wants a file to speak for it. What still applies is
            // `model`, below, which ADR-0018 already makes a workspace override.
            RuntimeSource::Shared(runtime) => (runtime, None),
            RuntimeSource::Private(recipe) => (
                Arc::new(recipe.with_config(&config).build_for(
                    &path,
                    self.shell,
                    &memory_roots,
                )?),
                None,
            ),
            RuntimeSource::Reusable(recipe) => {
                let runtime = Arc::new(recipe.build_for(&path, self.shell, &memory_roots).await?);
                (runtime, Some(recipe))
            }
        };

        // The workspace's own override first, then the file, then the runtime's
        // policy — which on the private path is already the file's answer, so
        // the two agree by construction rather than by luck. A resolved model
        // is already the final answer: preserve it whole and never consult the
        // provider's catalogue.
        let model = match self.model {
            WorkspaceModel::Inherited => runtime.resolve_model(config.model_selector()).await?,
            WorkspaceModel::Selector(selector) => runtime.resolve_model(Some(selector)).await?,
            WorkspaceModel::Resolved(model) => {
                if model.provider.as_str() != runtime.provider() {
                    return Err(RunError::ResolvedModelProviderMismatch {
                        model: model.id.clone(),
                        model_provider: model.provider.as_str().to_string(),
                        runtime_provider: runtime.provider().to_string(),
                    });
                }
                model
            }
        };

        // Skills must be registered on the runtime before any session spawns,
        // so every agent's tool roster includes `load_skill`. The hold is a
        // stack value until the `Workspace` below takes it: every `?` between
        // here and there drops it, so an open refused after this point leaves
        // a shared runtime holding no skills of a workspace that never opened.
        let (skills_registration, skills) = if self.discovery_enabled {
            let registration = register_skills(Arc::clone(&runtime), &path, &self.skills)?;
            let loaded = runtime
                .mentra_runtime_internal()
                .skills()
                .into_iter()
                .map(|skill| LoadedSkill {
                    name: skill.name,
                    description: skill.description,
                    model_invocable: skill.model_invocable,
                    path: skill.path,
                    root: skill.root,
                })
                .collect();
            (registration, loaded)
        } else {
            (SkillRoots::none(Arc::clone(&runtime)), Vec::new())
        };

        // Beside the skills and for the same reason: a tool has to be on the
        // runtime before any session spawns, or the first roster is offered
        // without it. The names are claimed first, so a manifest naming a tool
        // this runtime already answers to — `spawn`, a mentra builtin, another
        // workspace's declaration — refuses the open instead of replacing it.
        // No `dispatch::canonical` around the root here or on the guard entry
        // below: `path` already is what that helper would return, and calling
        // it again would be the second resolution this open exists to do
        // without. Registration and lookup still meet on one key, because the
        // lookup side canonicalizes the *call's* directory, which is the side
        // that has not been resolved yet.
        let declared_tools = DeclaredTools::register_with_supplied(
            Arc::clone(&runtime),
            &path,
            &declared_sources,
            &supplied_tools,
        )?;
        let declared_tool_names = declared_tools.names().to_vec();
        let host_tools = WorkspaceHostTools::register(
            Arc::clone(&runtime),
            &path,
            self.host_tools,
            HostToolBinding::Workspace,
        )?;

        // Templates need no runtime registration — they are basis-side convention
        // data, rendered into a prompt by whatever surface offers them.
        let (templates_dirs, templates) = if self.discovery_enabled {
            load_templates(&path, &self.templates)?
        } else {
            (Vec::new(), Vec::new())
        };

        // One runner for both interception bindings, host interceptors folded
        // first: the chain order host interceptors → global hooks → workspace
        // hooks predates the runtime split and survives it — supplied hooks
        // sit before file hooks, while only the registration point moved onto
        // the runtime's dispatcher.
        let runner = runtime.interceptors().iter().cloned().fold(
            HookRunner::new(&path, loaded_hooks),
            |runner, interceptor| runner.with_interceptor(interceptor),
        );
        // Written by every mint, read by `spawn` when a child policy narrows a
        // delegated child's roster — see `Workspace::minted_agent`. Empty
        // until the first mint, which is correct: nothing has been offered a
        // roster yet, so nothing has been denied one either.
        let foreign_tools = Arc::new(std::sync::RwLock::new(std::collections::BTreeSet::new()));
        let hook_registration = runtime.register_workspace(dispatch::WorkspaceGuardEntry {
            runner: Arc::new(runner),
            shell: self.shell,
            root: path.clone(),
            // On a private runtime the shell posture and the `.git` carve-out
            // are already in policy; enforcing them in the dispatcher too
            // would change whose words a denial arrives in.
            shared,
            foreign_tools: Arc::clone(&foreign_tools),
        });

        // Both lists reach the header whether or not this build has MCP in it:
        // what a run reports is a schema clients parse, and a field that
        // vanished with a cargo feature would make the stream's shape depend on
        // how basis was built.
        #[cfg(feature = "mcp")]
        let (mcp_connections, mcp_files, mcp_servers) = {
            if self.discovery_enabled {
                let (files, servers) = discovered_mcp(&path, &self.mcp)?;
                let connections =
                    McpConnections::connect(Arc::clone(&runtime), &path, servers).await;
                let names = connections.names().to_vec();

                (connections, files, names)
            } else {
                let servers = mcp::configured_supplied(&self.mcp)?;
                let connections =
                    McpConnections::connect(Arc::clone(&runtime), &path, servers).await;
                let names = connections.names().to_vec();
                (connections, Vec::new(), names)
            }
        };
        #[cfg(not(feature = "mcp"))]
        let (mcp_files, mcp_servers): (Vec<ContextFile>, Vec<String>) = (Vec::new(), Vec::new());

        let reuse = reusable_recipe.map(|recipe| WorkspaceReuse::new(recipe, self.shell));

        Ok(Workspace {
            // Compaction is two statements from two owners, joined here: the
            // numbers are this workspace's, the directory the snapshots land in
            // is the runtime's, because it is the one that knows where this
            // workspace's history lives (ADR-0018).
            agent: agent_config(
                &path,
                &context,
                self.system_prompt.as_ref(),
                memory::index_block(&memories).as_deref(),
                self.roster,
                self.compaction,
                runtime.transcripts_dir().to_path_buf(),
            ),
            identifier: store::runtime_identifier(&path),
            // Not resolved a second time: `path` *is* what discovery resolved,
            // and asking again would reintroduce the second answer this open
            // exists to do without.
            root: path,
            provider: runtime.provider().to_string(),
            runtime,
            reuse,
            mint_posture: MintPosture::new(fresh_only),
            model,
            // The last thing the file still has to say, and the one this
            // builder cannot say for it: an effort is a per-turn request, so
            // it waits here until a `RunSpec` that asked for none is minted.
            effort: config.effort.as_ref().map(|effort| effort.value),
            config,
            context,
            memories,
            skills_registration,
            skills,
            templates_dirs,
            templates,
            mcp_files,
            mcp_servers,
            declared_tool_files: sourced(&declared_sources),
            declared_tools: declared_tool_names,
            declared_registration: declared_tools,
            host_tools,
            hook_registration,
            foreign_tools,
            #[cfg(feature = "mcp")]
            mcp_connections,
        })
    }
}

/// Which tool manifests took effect, for the workspace's own report.
///
/// The same shape `.mcp.json`'s discovery reports, because the two files raise
/// the same question: a caller looking at a run should be able to see which
/// file put a program within the model's reach.
fn sourced(sources: &[declared::ToolsSource]) -> Vec<ContextFile> {
    sources
        .iter()
        .map(|source| ContextFile {
            path: source.path.clone(),
            scope: source.scope.label(),
        })
        .collect()
}

/// Discovers the MCP servers this workspace connects, and which files said so.
///
/// Discovery runs for its own sake as well: the header names which files took
/// effect, and an `.mcp.json` is the last thing that should apply invisibly —
/// it says which programs to spawn. The connecting happens in
/// [`crate::mcp::connections`], which owns the claim-and-bridge fold.
#[cfg(feature = "mcp")]
fn discovered_mcp(
    workspace: &Path,
    config: &McpConfig,
) -> Result<(Vec<ContextFile>, Vec<mcp::ConfiguredServer>), RunError> {
    let files: Vec<ContextFile> = mcp::discover(workspace, config)?
        .iter()
        .map(|source| ContextFile {
            path: source.path.clone(),
            scope: source.scope.label(),
        })
        .collect();

    Ok((files, mcp::configured(workspace, config)?))
}

/// Registers every skills directory that exists, most specific first, and
/// returns the hold that gives them back.
///
/// Roots layer rather than replace, so a workspace skill shadows a personal one
/// of the same name and everything else from the weaker roots still loads. Which
/// four roots those are, and why they are in that order, is [`crate::skills`];
/// what the returned value is for, and why the runtime counts holders rather
/// than owners, is [`SkillRoots`].
fn register_skills(
    runtime: Arc<Runtime>,
    workspace: &Path,
    config: &SkillsConfig,
) -> Result<SkillRoots, RunError> {
    let sources = skills::discover(workspace, config);
    let paths: Vec<PathBuf> = sources.iter().map(|source| source.path.clone()).collect();

    SkillRoots::register(runtime, paths)
}

/// Loads every template the workspace defines, with the roots they came from.
///
/// A root that exists but holds a file basis cannot read is an error rather than
/// an empty command list: a template that failed to load and a template nobody
/// wrote look the same from a client, and only one of them is worth knowing
/// about.
///
/// Shared with [`prepare_with_session`](crate::run::prepare_with_session), which
/// discovers templates for a runtime it does not own — one implementation, so
/// the two cannot disagree about which files are a workspace's commands.
pub(crate) fn load_templates(
    workspace: &Path,
    config: &TemplatesConfig,
) -> Result<(Vec<PathBuf>, Vec<Template>), RunError> {
    let sources = templates::discover(workspace, config);
    let dirs: Vec<PathBuf> = sources.iter().map(|source| source.path.clone()).collect();

    Ok((dirs, templates::load_sources(&sources)?))
}

/// Turns discovered context into the agent's system prompt, scopes the agent to
/// the workspace, settles which tools the model is offered, and says how much
/// of the conversation reaches the model. Everything else stays at mentra's
/// defaults — opinions belong in the prompt and the workspace, not here.
///
/// `system_prompt` is the host's say over the first of those, and `None` — the
/// default and what every caller before it did — is discovery alone. basis
/// still ships no prompt of its own: the text in either variant is the host's.
///
/// # Why compaction is not left at mentra's default
///
/// Because evidence retention is a Basis invariant, not an upstream default.
/// Mentra currently also keeps every result, but Basis pins that posture so a
/// future default cannot silently blank what the model just read. See
/// [`crate::compaction`]. The mutually exclusive projected-byte policy is
/// explicitly off; the remaining unexposed settings are inherited.
///
/// # Which tools the model is offered
///
/// `roster` is [`ToolRoster`] (decision D3), a workspace's own knob over
/// mentra's `ToolProfile` — see its module docs for what each constructor
/// does and does not change, and for the two things (a sibling workspace's
/// hidden tools, the rendered prompt) that apply on top of whatever roster is
/// set here regardless.
///
/// **Hidden is a roster fact, not a capability fact.** Every tool a roster
/// hides stays registered on the runtime, which is precisely why `spawn` can
/// still reach the command executor underneath even though
/// [`ToolRoster::default`] hides it by name. What a caller said about
/// commands is still decided by [`ShellAccess`] — baked into policy on a
/// private runtime, enforced by the hook dispatcher on a shared one — on the
/// path `spawn` uses: `--no-shell` shuts commands off for `spawn` exactly as
/// it did for `shell`.
///
/// The roster travels: `DisposableSubagentTemplate::from_agent` clones this
/// whole config, so a subagent of a subagent is offered the same roster.
///
/// Built once and cloned per run, because none of its inputs are per-run —
/// the per-mint extension (hiding other workspaces' MCP tools) happens in
/// [`Workspace::prepare`](super::Workspace::prepare)'s path, where the shared
/// registry's current contents are known.
fn agent_config(
    workspace: &Path,
    context: &WorkspaceContext,
    system_prompt: Option<&SystemPrompt>,
    memory_index: Option<&str>,
    roster: ToolRoster,
    compaction: Compaction,
    transcripts: PathBuf,
) -> mentra::agent::AgentConfig {
    mentra::agent::AgentConfig {
        // The memory index rides the context's own render path — after the
        // documents, before a host's `Append`, gone under `Replace` — so it
        // obeys the same rules as everything else in the prompt, and none of
        // them consult `roster` at all (item d of D3).
        system: context.render_with_appendix(system_prompt, memory_index),
        tool_profile: roster.into_profile(),
        workspace: mentra::agent::WorkspaceConfig {
            base_dir: workspace.to_path_buf(),
            ..Default::default()
        },
        compaction: compaction.into_mentra(transcripts),
        // D2 (wave 1): mentra's memory engine is off. basis's memory is a
        // file convention (`crate::memory`), and mentra's is a store —
        // auto-recall would put that store's content into the prompt with
        // nothing visible saying so, which is exactly the kind of silent
        // input basis exists to remove. Recall off here, the three memory
        // tools hidden in `ToolRoster`'s default set, and the write tools
        // refused at execution too, so no unhidden path can reach the store
        // either.
        memory: mentra::agent::MemoryConfig {
            auto_recall_enabled: false,
            write_tools_enabled: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
