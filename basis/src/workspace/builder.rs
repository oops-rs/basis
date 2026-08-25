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

use mentra::ModelSelector;

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
    runtime::{Runtime, RuntimeBuilder, dispatch},
    shell::ShellAccess,
    skills::{self, SkillsConfig},
    store,
    templates::{self, Template, TemplatesConfig},
    tools::declared::{self, DeclaredTools, ToolsConfig},
};

use super::Workspace;

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
    /// An override; `None` defers to the runtime's model policy.
    model: Option<ModelSelector>,
    context: ContextConfig,
    /// What `config.json` said; `None` means discover it at
    /// [`open`](WorkspaceBuilder::open).
    config: Option<Config>,
    /// The host's own say over the system prompt; `None` is discovery alone.
    system_prompt: Option<SystemPrompt>,
    skills: SkillsConfig,
    memory: MemoryConfig,
    #[cfg(feature = "mcp")]
    mcp: McpConfig,
    templates: TemplatesConfig,
    hooks: HooksConfig,
    tools: ToolsConfig,
    shell: ShellAccess,
    compaction: Compaction,
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
                },
            )
            .field("model", &self.model)
            .field("context", &self.context)
            .field("config", &self.config)
            .field("system_prompt", &self.system_prompt)
            .field("skills", &self.skills)
            .field("memory", &self.memory)
            .field("templates", &self.templates)
            .field("hooks", &self.hooks)
            .field("tools", &self.tools)
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
            model: None,
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
            #[cfg(feature = "mcp")]
            mcp: McpConfig::default(),
            templates: TemplatesConfig::default(),
            hooks: HooksConfig::default(),
            tools: ToolsConfig::default(),
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

    /// Overrides the runtime's model policy, for this workspace alone.
    ///
    /// Unset, the runtime's [`with_model`](crate::RuntimeBuilder::with_model)
    /// policy decides. Either way the *resolved* model is this workspace's
    /// fact, fixed at open and reported by every run it mints.
    pub fn with_model(self, model: ModelSelector) -> Self {
        Self {
            model: Some(model),
            ..self
        }
    }

    pub fn with_context(self, context: ContextConfig) -> Self {
        Self { context, ..self }
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

    /// Sets where subprocess hooks are discovered.
    ///
    /// A hook is an external command that gets a say over each tool call; see
    /// [`crate::hooks`] for the wire contract and for what happens when one
    /// breaks. [`RuntimeBuilder::with_interceptor`](crate::RuntimeBuilder::with_interceptor)
    /// is the same say, in the host's process — host scope is runtime scope.
    pub fn with_hooks(self, hooks: HooksConfig) -> Self {
        Self { hooks, ..self }
    }

    /// Sets where declared subprocess tools are discovered.
    ///
    /// A declared tool is a command the workspace offers the *model* as a tool,
    /// with a JSON schema for its input; see [`crate::tools::declared`] for the
    /// manifest and for what a failing one tells the model. The tools are
    /// registered on the runtime this workspace borrows and deregistered — as
    /// far as mentra's registry allows — when the workspace drops, so a
    /// repository's tools never reach another repository's runs.
    pub fn with_tools(self, tools: ToolsConfig) -> Self {
        Self { tools, ..self }
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
    /// workspace registers is loadable by another's runs — an accepted
    /// consequence of sharing; [`Workspace::skills`] reports only what this
    /// workspace registered. MCP tools live on the same single registry but do
    /// **not** travel: every roster minted here hides the `mcp__*` tools of
    /// servers this workspace does not own.
    pub async fn open(self) -> Result<Workspace, RunError> {
        let context = WorkspaceContext::discover_with(&self.path, &self.context)?;

        // Read before the runtime is acquired, for the reason the hooks file
        // below is: a config that does not parse must fail the open rather
        // than let a run reach a model nobody in this repository chose. The
        // global directory is the context config's, so one process cannot read
        // two different global directories.
        let config = match self.config {
            Some(config) => config,
            None => config::Config::discover(&self.path, self.context.global_dir.as_deref())?,
        };

        // Loaded before the runtime is acquired so a hooks file that does not
        // parse fails the open loudly, rather than at the first tool call —
        // or worse, never.
        let loaded_hooks = hooks::load(&self.path, &self.hooks)?;

        // Read here for the same reason, and one of its own: a manifest that
        // does not parse is a tool the model's instructions assume and will not
        // find. Registering it needs the runtime, so that waits until there is
        // one.
        let declared_sources = declared::discover(&self.path, &self.tools)?;

        // Memory, before the runtime is acquired for the reason the files
        // above are — a memory that does not parse fails the open naming the
        // file. The workspace root derives beside the runtime's store dir
        // ([`crate::memory`]), which on the private path is still a recipe, so
        // both shapes are asked before the match below consumes them. The
        // roots are resolved whether or not they exist yet: the private
        // runtime's policy names them (the model writes memories through the
        // ordinary file tools, and the roots sit outside the workspace), and
        // the first memory is written by exactly the run that finds none to
        // read. On a *shared* runtime the fixed policy cannot carry
        // per-workspace roots, so the index still renders but a file tool
        // write to a root outside the agent's own directory is refused —
        // recorded beside the other costs of sharing.
        let store_dir = match &self.runtime {
            RuntimeSource::Shared(runtime) => runtime.store_dir().map(Path::to_path_buf),
            RuntimeSource::Private(recipe) => recipe.named_store_dir().map(Path::to_path_buf),
        };
        let memory_sources = memory::roots(&self.memory, store_dir.as_deref());
        let memories = memory::load(&memory_sources)?;
        let memory_roots: Vec<PathBuf> = memory_sources
            .iter()
            .map(|source| source.path.clone())
            .collect();

        let shared = matches!(self.runtime, RuntimeSource::Shared(_));
        let runtime = match self.runtime {
            // A shared runtime's provider, credential and endpoint are the
            // host's process facts and were settled before this workspace
            // existed, so a file's `provider` and `base_url` have nothing to
            // reach here — the host that shares a runtime is the one that
            // decided the connection, and applies `RuntimeBuilder::with_config`
            // itself if it wants a file to speak for it. What still applies is
            // `model`, below, which ADR-0018 already makes a workspace override.
            RuntimeSource::Shared(runtime) => runtime,
            RuntimeSource::Private(recipe) => Arc::new(recipe.with_config(&config).build_for(
                &self.path,
                self.shell,
                &memory_roots,
            )?),
        };

        // The workspace's own override first, then the file, then the runtime's
        // policy — which on the private path is already the file's answer, so
        // the two agree by construction rather than by luck.
        let model = runtime
            .resolve_model(self.model.or_else(|| config.model_selector()))
            .await?;

        // Skills must be registered on the runtime before any session spawns,
        // so every agent's tool roster includes `load_skill`.
        let skills_dirs = register_skills(runtime.mentra_runtime(), &self.path, &self.skills)?;
        let skills = runtime
            .mentra_runtime()
            .skills()
            .into_iter()
            .map(|skill| LoadedSkill {
                name: skill.name,
                description: skill.description,
                model_invocable: skill.model_invocable,
                path: skill.path,
            })
            .collect();

        // Beside the skills and for the same reason: a tool has to be on the
        // runtime before any session spawns, or the first roster is offered
        // without it. The names are claimed first, so a manifest naming a tool
        // this runtime already answers to — `spawn`, a mentra builtin, another
        // workspace's declaration — refuses the open instead of replacing it.
        let declared_tools = DeclaredTools::register(
            Arc::clone(&runtime),
            &dispatch::canonical(&self.path),
            &declared_sources,
        )?;
        let declared_tool_names = declared_tools.names().to_vec();

        // Templates need no runtime registration — they are basis-side convention
        // data, rendered into a prompt by whatever surface offers them.
        let (templates_dirs, templates) = load_templates(&self.path, &self.templates)?;

        // One runner for both interception bindings, host interceptors folded
        // first: the chain order host interceptors → global hooks → workspace
        // hooks predates the runtime split and survives it — only the
        // registration point moved, onto the runtime's dispatcher.
        let runner = runtime.interceptors().iter().cloned().fold(
            HookRunner::new(&self.path, loaded_hooks),
            |runner, interceptor| runner.with_interceptor(interceptor),
        );
        let hook_registration = runtime.register_workspace(dispatch::WorkspaceGuardEntry {
            runner: Arc::new(runner),
            shell: self.shell,
            root: dispatch::canonical(&self.path),
            // On a private runtime the shell posture and the `.git` carve-out
            // are already in policy; enforcing them in the dispatcher too
            // would change whose words a denial arrives in.
            shared,
        });

        // Both lists reach the header whether or not this build has MCP in it:
        // what a run reports is a schema clients parse, and a field that
        // vanished with a cargo feature would make the stream's shape depend on
        // how basis was built.
        #[cfg(feature = "mcp")]
        let (mcp_connections, mcp_files, mcp_servers) = {
            let (files, servers) = discovered_mcp(&self.path, &self.mcp)?;
            let connections =
                McpConnections::connect(Arc::clone(&runtime), &self.path, servers).await;
            let names = connections.names().to_vec();

            (connections, files, names)
        };
        #[cfg(not(feature = "mcp"))]
        let (mcp_files, mcp_servers): (Vec<ContextFile>, Vec<String>) = (Vec::new(), Vec::new());

        Ok(Workspace {
            root: resolved_workspace(&self.path, &context),
            // Compaction is two statements from two owners, joined here: the
            // numbers are this workspace's, the directory the snapshots land in
            // is the runtime's, because it is the one that knows where this
            // workspace's history lives (ADR-0018).
            agent: agent_config(
                &self.path,
                &context,
                self.system_prompt.as_ref(),
                memory::index_block(&memories).as_deref(),
                self.compaction,
                runtime.transcripts_dir().to_path_buf(),
            ),
            identifier: store::runtime_identifier(&self.path),
            path: self.path,
            provider: runtime.provider().to_string(),
            runtime,
            model,
            // The last thing the file still has to say, and the one this
            // builder cannot say for it: an effort is a per-turn request, so
            // it waits here until a `RunSpec` that asked for none is minted.
            effort: config.effort.as_ref().map(|effort| effort.value),
            config,
            context,
            memories,
            skills_dirs,
            skills,
            templates_dirs,
            templates,
            mcp_files,
            mcp_servers,
            declared_tool_files: sourced(&declared_sources),
            declared_tools: declared_tool_names,
            declared_registration: declared_tools,
            hook_registration,
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

/// Registers every skills directory that exists, most specific first.
///
/// Roots layer rather than replace, so a workspace skill shadows a personal one
/// of the same name and everything else from the weaker roots still loads. Which
/// four roots those are, and why they are in that order, is [`crate::skills`].
fn register_skills(
    runtime: &mentra::Runtime,
    workspace: &Path,
    config: &SkillsConfig,
) -> Result<Vec<PathBuf>, RunError> {
    let sources = skills::discover(workspace, config);
    let paths: Vec<PathBuf> = sources.iter().map(|source| source.path.clone()).collect();

    runtime.register_skills_dirs(&paths)?;

    Ok(paths)
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

/// The workspace as discovery resolved it, falling back to what was asked for.
///
/// Discovery follows symlinks so the parent walk is meaningful, which means a
/// document's path can sit under a different spelling of the same directory
/// than the caller typed. Reporting the resolved root keeps the header
/// internally consistent — `workspace` and `context_files` name one place.
///
/// Shared with [`prepare_with_session`](crate::run::prepare_with_session) for
/// the same reason [`load_templates`] is: the one path that does not open a
/// workspace must still report one the same way.
pub(crate) fn resolved_workspace(requested: &Path, context: &WorkspaceContext) -> PathBuf {
    context
        .root()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| requested.to_path_buf())
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
/// Because that default is an opinion, and a costly one: mentra blanks every
/// tool result but the three most recent on the way to *every* provider
/// request, at any context size. See [`crate::compaction`]. The other eight
/// fields of its `CompactionConfig` are still inherited — only what basis has
/// a reason to state is stated.
///
/// # Why tools leave the roster
///
/// ADR-0016 gives the model one door for *do something I cannot do by
/// thinking*: [`spawn`](crate::tools::spawn). `shell`, `background_run` and
/// `task` are the doors it replaces, and leaving them alongside it would
/// restore exactly what the ADR removed — three names at the approval gate, and
/// three rule namespaces, for one question. [`UNSURFACED_TOOLS`] is the other
/// half of the same argument, applied to intrinsics that were never a decision
/// at all: they are on the roster because mentra registers everything it has
/// and basis never said otherwise, which is not the same as basis offering
/// them.
///
/// **Hidden is a roster fact, not a capability fact.** Every one of them stays
/// registered on the runtime, which is precisely why `spawn` can still reach
/// the command executor underneath. What a caller said about commands is still
/// decided by [`ShellAccess`] — baked into policy on a private runtime,
/// enforced by the hook dispatcher on a shared one — on the path `spawn` uses:
/// `--no-shell` shuts commands off for `spawn` exactly as it did for `shell`.
///
/// The hidden set travels: `DisposableSubagentTemplate::from_agent` clones this
/// whole config, so a subagent of a subagent is offered the same one door.
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
    compaction: Compaction,
    transcripts: PathBuf,
) -> mentra::agent::AgentConfig {
    mentra::agent::AgentConfig {
        // The memory index rides the context's own render path — after the
        // documents, before a host's `Append`, gone under `Replace` — so it
        // obeys the same rules as everything else in the prompt.
        system: context.render_with_appendix(system_prompt, memory_index),
        tool_profile: mentra::agent::ToolProfile::hide(hidden_tools()),
        workspace: mentra::agent::WorkspaceConfig {
            base_dir: workspace.to_path_buf(),
            ..Default::default()
        },
        compaction: compaction.into_mentra(transcripts),
        // D2 (wave 1): mentra's memory engine is off. basis's memory is a
        // file convention arriving in a later wave, and mentra's is a store —
        // auto-recall would put that store's content into the prompt with
        // nothing visible saying so, which is exactly the kind of silent
        // input basis exists to remove. Recall off here, the three memory
        // tools hidden in [`UNSURFACED_TOOLS`], and the write tools refused
        // at execution too, so no unhidden path can reach the store either.
        memory: mentra::agent::MemoryConfig {
            auto_recall_enabled: false,
            write_tools_enabled: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Every name basis takes off the model's roster: what `spawn` replaced, and
/// what basis has never surfaced.
///
/// Two constants rather than one list because the two carry different
/// arguments and a reader deserves to know which applies to a given name.
fn hidden_tools() -> impl Iterator<Item = &'static str> {
    REPLACED_TOOLS.into_iter().chain(UNSURFACED_TOOLS)
}

/// The tools `spawn` replaces, by the names mentra registers them under.
const REPLACED_TOOLS: [&str; 3] = ["shell", "background_run", "task"];

/// What mentra registers that basis has never deliberately offered.
///
/// Registration is mentra's default posture — `register_tools` walks every
/// intrinsic variant it has — so a name reaching the model here is the absence
/// of a decision rather than one. Each of these fails a different way, and none
/// of the failures is visible to the person running the agent:
///
/// - **`team_spawn` and its six siblings are delegation by another name.** A
///   second door for *hand work to something else, read back a summary* is
///   exactly what ADR-0016 removed `task` for: two names arriving at one
///   approval gate, and two namespaces of remembered rules, for a question an
///   operator asks once. Nothing in basis mints a team, reads a teammate inbox,
///   or renders a `team_request`, so the door does not even lead where its
///   description says. `docs/REDESIGN.md` has recorded these as awaiting a
///   concrete use case since Phase D; reachable-by-accident is not the
///   deliberate surfacing that row is waiting for.
/// - **`idle` is that surface's exit.** Its whole effect is
///   `Agent::request_idle`, which mentra's orchestrator reads as
///   `should_end_turn` — a yield *back to the teammate loop* basis never
///   starts. On a basis run the model calling it ends its own turn mid-task
///   and the caller reads a short answer with no error in it.
/// - **`task_create` and the other four write a board nothing reads.** basis
///   surfaces no task board — not on the event stream, not over ACP, not in
///   the CLI — so a model that files, claims and updates work items gets
///   plausible success back from every call and nothing observable happens.
///   Confident bookkeeping into a void is worse than no bookkeeping, because
///   it reads to the model as coordination.
/// - **`check_background` reports on a tool that is hidden.** The only thing it
///   can report on is `background_run`, which left the roster with ADR-0016's
///   two other doors, so it can answer nothing but "no such task".
/// - **`memory_pin`, `memory_forget` and `memory_search` reach a store basis
///   has decided against (D2, wave 1).** basis's memory is a file convention
///   arriving in a later wave; mentra's engine — recall injection included,
///   switched off in `agent_config` beside this list — is not it. A model
///   pinning facts into a store nothing surfaces is the task-board failure
///   again: plausible success, and nothing the person running the agent can
///   see.
///
/// Deliberately still offered, and each for a reason: `load_skill`, because
/// on-demand skills are basis's own convention and that tool is how a skill is
/// loaded; and `compact`, because a model that can see its context filling
/// should be able to act on it (that the *user* has no matching control is a
/// separate gap, and hiding this would not close it).
const UNSURFACED_TOOLS: [&str; 17] = [
    "check_background",
    "idle",
    "task_create",
    "task_claim",
    "task_update",
    "task_list",
    "task_get",
    "team_spawn",
    "team_send",
    "team_read_inbox",
    "team_broadcast",
    "team_request",
    "team_respond",
    "team_list_requests",
    "memory_pin",
    "memory_forget",
    "memory_search",
];

#[cfg(test)]
mod tests;
