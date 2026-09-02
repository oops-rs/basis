//! A workspace opened once, minting runs cheaply.
//!
//! Everything a run needs but does not change divides in two, and basis once
//! kept both halves in one `RunConfig` and re-resolved the
//! lot for every prompt. ADR-0010 named the cost: a twenty-agent fan-out read
//! `AGENTS.md` twenty times, resolved the model twenty times, and opened twenty
//! copies of every MCP server the workspace configures.
//!
//! So:
//!
//! - **[`WorkspaceBuilder::open`]** settles what belongs to the workspace —
//!   context documents, the resolved model, skills, templates, hooks, MCP
//!   connections, the command posture, the approval gate.
//!   It is `async` and it does real I/O, once.
//! - **[`Workspace::prepare`]** mints one run from a [`RunSpec`]. It is *not*
//!   `async`, which is the honest signal that nothing is discovered, resolved,
//!   or connected here: a session is spawned on the runtime that already
//!   exists, and that is all.
//!
//! What belongs to the *process* rather than to either — the provider and
//! credential, the history store, the host's interceptors — is a third thing,
//! [`Runtime`] (ADR-0018). A workspace borrows one through an
//! `Arc`, and a host opening many workspaces builds it once.
//!
//! ```no_run
//! # async fn example() -> Result<(), basis::RunError> {
//! use basis::{AllowAll, CollectingSink, Workspace};
//!
//! let workspace = Workspace::open("/repo").await?;
//!
//! // Two runs, one discovery, driven together.
//! let mut first = workspace.prepare("what does this repo do?")?;
//! let mut second = workspace.prepare("what is not tested?")?;
//! let (a, b) = tokio::join!(
//!     first.execute_with_approver(CollectingSink::default(), AllowAll),
//!     second.execute_with_approver(CollectingSink::default(), AllowAll),
//! );
//! # let _ = (a?, b?);
//! # Ok(())
//! # }
//! ```
//!
//! The free functions in [`crate::run`](mod@crate::run) — `run`, `prepare`,
//! `resume` and the rest — are thin wrappers that open a workspace, mint one
//! run from it, and drop the workspace when the run ends. There is one
//! resolution path, and this is it.

mod builder;
mod lifecycle;
mod profile;
mod roster;
mod spec;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use mentra::{ModelInfo, Session, agent::AgentConfig, provider::ReasoningOptions};

pub use builder::WorkspaceBuilder;
pub use profile::RunProfile;
pub use roster::ToolRoster;
pub(crate) use spec::DEFAULT_SESSION_NAME;
pub use spec::RunSpec;

pub(crate) use builder::load_templates;
use lifecycle::MintPosture;

#[cfg(feature = "mcp")]
use crate::mcp::connections::McpConnections;
use crate::{
    config::Config,
    context::WorkspaceContext,
    error::RunError,
    event::ContextFile,
    fingerprint::{self, Snapshot},
    memory::Memory,
    run::{Effort, LoadedSkill, PreparedRun, RunContext},
    runtime::{Runtime, dispatch::HookRegistration},
    skills::SkillRoots,
    templates::Template,
    tools::declared::DeclaredTools,
};

/// One workspace, resolved: the runtime it borrows, the model, and everything
/// discovered on disk, ready to mint runs from.
///
/// Held by reference by every run it mints, so a host keeps one per repository
/// for as long as it wants to send prompts at it. Dropping it does not end the
/// runs already minted — a [`PreparedRun`] owns its session — but the MCP
/// connections go with it, and so does the runtime when this held the last
/// `Arc`.
///
/// `Send` and `Sync`: the runtime is shared through `Arc`s and creates
/// sessions from `&self`, so concurrent minting from one workspace needs no
/// lock of basis's own.
pub struct Workspace {
    /// The directory this workspace is scoped to: absolute and canonical,
    /// resolved exactly once by [`WorkspaceBuilder::open`] and never derived
    /// again.
    ///
    /// One field rather than two — the requested spelling and the resolved
    /// one — because everything downstream has to agree: the agent's base
    /// directory, the runtime's policy roots, the dispatcher's key, the store
    /// identifier and the run header's `workspace` all take this value, and a
    /// second spelling kept beside it is only an opportunity for two of them
    /// to name different directories.
    root: PathBuf,
    runtime: Arc<Runtime>,
    /// Whether supported Basis APIs may mint more than one independent
    /// session from this workspace.
    mint_posture: MintPosture,
    model: ModelInfo,
    /// The reasoning effort a [`RunSpec`] gets when it asked for none, as
    /// `config.json` set it. `None` leaves the provider's own default, which
    /// is what every run had before there was a file to say otherwise.
    effort: Option<Effort>,
    /// What `config.json` said, kept so a host can report which file decided
    /// the model it is looking at.
    config: Config,
    provider: String,
    /// [`store::runtime_identifier`](crate::store::runtime_identifier) for
    /// [`root`](Self::root), computed once: what this workspace's
    /// conversations are (or, on a shared runtime, should be — see
    /// [`WorkspaceBuilder::open`]) tagged with.
    identifier: String,
    context: WorkspaceContext,
    /// The memories discovered at open, frontmatter only, name-ordered after
    /// shadowing. What the agent config's index block was rendered from.
    memories: Vec<Memory>,
    /// Built once from the context, cloned per run: none of its inputs vary.
    agent: AgentConfig,
    /// The skills roots this open put on the runtime, held for as long as this
    /// workspace is: their paths are what a run reports, and the hold is what
    /// takes them back off a shared runtime on drop.
    skills_registration: SkillRoots,
    skills: Vec<LoadedSkill>,
    templates_dirs: Vec<PathBuf>,
    templates: Vec<Template>,
    mcp_files: Vec<ContextFile>,
    mcp_servers: Vec<String>,
    declared_tool_files: Vec<ContextFile>,
    declared_tools: Vec<String>,
    /// Keeps this workspace's declared tools claimed on the runtime's single
    /// registry; releases the claims on drop.
    declared_registration: DeclaredTools,
    /// Keeps this workspace's hooks and guards registered on the runtime's
    /// dispatcher; deregisters on drop.
    #[allow(dead_code, reason = "held for its Drop")]
    hook_registration: HookRegistration,
    /// The other half of the registry entry's `foreign_tools` cell, written by
    /// [`minted_agent`](Self::minted_agent) so `spawn` can read what this
    /// workspace's model is currently denied.
    foreign_tools: Arc<RwLock<BTreeSet<String>>>,
    #[cfg(feature = "mcp")]
    #[allow(dead_code, reason = "held for its Drop")]
    mcp_connections: McpConnections,
}

/// Hand-written because neither the runtime nor the registration is `Debug`
/// material, and because the context documents hold whole files — a derived
/// impl would dump them.
impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .field("provider", &self.provider)
            .field("model", &self.model.id)
            .field("fresh_only", &self.mint_posture.is_fresh_only())
            .field("config_files", &self.config.files)
            .field("context_files", &self.context.documents().len())
            .field("memories", &self.memories.len())
            .field("skills", &self.skills.len())
            .field("templates", &self.templates.len())
            .field("mcp_servers", &self.mcp_servers)
            .field("declared_tools", &self.declared_tools)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// Opens `path` with basis's defaults: a private runtime with the provider
    /// auto-detected from the environment, the newest model it offers, and
    /// every convention discovered where convention says to look.
    ///
    /// [`builder`](Self::builder) is the same call with the knobs exposed —
    /// including [`with_runtime`](WorkspaceBuilder::with_runtime), for the
    /// host that opens many workspaces on one [`Runtime`].
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, RunError> {
        Self::builder(path).open().await
    }

    /// Configures a workspace before opening it.
    pub fn builder(path: impl Into<PathBuf>) -> WorkspaceBuilder {
        WorkspaceBuilder::new(path)
    }

    /// Mints a run: a fresh conversation against this workspace.
    ///
    /// Synchronous, and deliberately so — everything expensive already
    /// happened. What this does is spawn a session on the existing runtime and
    /// hand back the [`PreparedRun`] that drives it.
    ///
    /// The spec's prompt may be empty. Once a session outlives a turn, a
    /// conversation with nothing said yet is a real state — it is what ACP's
    /// `session/new` opens — so the emptiness check belongs where a prompt is
    /// actually sent, which is [`PreparedRun::execute_with_approver`] and
    /// [`PreparedRun::send_with_options`]. (The free [`run`](crate::run()) keeps
    /// its own up-front check, because a one-shot caller that passed nothing
    /// wants to hear about it before a session exists.)
    pub fn prepare(&self, spec: impl Into<RunSpec>) -> Result<PreparedRun, RunError> {
        let spec = spec.into();
        self.mint_posture.claim()?;
        if let Some(model) = spec.profile.resolved_model() {
            validate_model_provider(model, &self.provider)?;
        }
        if spec.profile.has_extra_headers() && !self.runtime.has_ephemeral_history() {
            return Err(RunError::RunProfileHeadersRequireEphemeralHistory);
        }
        let model = spec
            .profile
            .resolved_model()
            .cloned()
            .unwrap_or_else(|| self.model.clone());
        let model_id = model.id.clone();
        let agent = self.minted_agent(&spec.profile);
        let context_snapshot = agent.system.clone();
        let mut session =
            self.runtime
                .mint(spec.session_name.clone(), model, agent, &self.identifier)?;
        if !spec.profile.decides_reasoning() {
            apply_effort(&mut session, spec.effort.or(self.effort))?;
        }

        Ok(self.minted(session, spec, model_id, context_snapshot))
    }

    /// Picks up a conversation a previous process left behind.
    ///
    /// `agent_id` is [`PreparedRun::agent_id`], not the session id: mentra
    /// persists agents, and a session is one process's view of one. Resuming
    /// replays the transcript from the store, so the first turn after this
    /// already knows everything the last one did.
    ///
    /// Resuming is also where a "…for this session" approval answer dies
    /// (see [`ApprovalDecision`](crate::ApprovalDecision)): the attach clears
    /// the conversation's session-scope rules before anything can answer
    /// from them. That clear must read the store's `rules.json`, so a
    /// corrupt or unwritable file fails **every** resume against the store —
    /// not just this conversation's — with
    /// [`RunError::SessionRulesNotCleared`](crate::RunError) naming the file
    /// to repair or delete; fresh conversations keep working, and deleting
    /// the file costs only remembered approval answers, never history.
    ///
    /// The workspace has to be the one the conversation belongs to. Nothing
    /// here checks that — mentra's store is keyed by agent, not by path — so
    /// resuming an agent under a workspace it never ran in gives it that
    /// workspace's context and tools alongside its own history.
    ///
    /// mentra does not persist a model's context window
    /// (`Agent::from_loaded` always resumes at `None` — `set_model` is the
    /// only way back), so a resumed session starts with an unknown one. This
    /// reapplies this workspace's own resolved model exactly when the resumed
    /// conversation is still on it and this resume is not also changing
    /// reasoning — the same model [`prepare`](Self::prepare) would have minted
    /// — which restores mentra's own compaction threshold as well as
    /// [`PreparedRun::context_window`]. A reasoning-changing resume leaves the
    /// window unknown so it performs one persisted mutation rather than two. A conversation
    /// [`PreparedRun::set_model`] had already moved elsewhere keeps whatever
    /// that call left it at instead: basis has no window for a model it does
    /// not resolve, and forcing this one back would silently undo a choice
    /// the caller made.
    pub fn resume(
        &self,
        agent_id: &str,
        spec: impl Into<RunSpec>,
    ) -> Result<PreparedRun, RunError> {
        let spec = spec.into();
        self.mint_posture.claim()?;
        if let Some(model) = spec.profile.resolved_model() {
            validate_model_provider(model, &self.provider)?;
        }
        if let Some(field) = spec.profile.unsupported_on_resume() {
            return Err(RunError::UnsupportedResumeProfile { field });
        }
        let effort = spec.effort.or(self.effort);
        let changes_reasoning = effort.is_some();
        if spec.profile.resolved_model().is_some() && changes_reasoning {
            return Err(RunError::NonAtomicResumeProfile);
        }

        let mut session = self.runtime.resume_minted(agent_id)?;
        let model = if let Some(model) = spec.profile.resolved_model() {
            session.set_model(model.clone())?;
            model.id.clone()
        } else if !changes_reasoning && session_on_resolved_model(&session, &self.model) {
            session.set_model(self.model.clone())?;
            self.model.id.clone()
        } else {
            session.metadata().model.clone()
        };

        apply_effort(&mut session, effort)?;

        // basis does not yet read the resumed AgentConfig back (mentra
        // 0.26's `Session::config` exposes it — mentra#41, unadopted here).
        // The persisted agent may carry a per-run system override that
        // differs from this workspace's current default, so substituting
        // `self.agent.system` would turn an unknown estimate into a
        // confidently wrong one.
        Ok(self.minted(session, spec, model, None))
    }

    /// A cheap stand-in for everything in this workspace a run could see.
    ///
    /// The utility ADR-0014 kept when `watch` was deleted, on the type its
    /// ledger row promised it to. The semantics are [`crate::fingerprint`]'s
    /// verbatim: a digest over `git ls-files` plus `HEAD`, `stat` only, and
    /// every uncertain answer resolving to *changed* rather than unchanged.
    ///
    /// Fingerprints the workspace **as it is now**, not as it was when the
    /// workspace was opened — that is the whole point, since a caller's loop
    /// asks it repeatedly against one long-lived workspace.
    ///
    /// Blocking: it spawns `git` and stats files. An async caller belongs on a
    /// blocking thread — `tokio::task::spawn_blocking`, or the equivalent.
    pub fn fingerprint(&self) -> Snapshot {
        fingerprint::snapshot(&self.root)
    }

    /// The directory this workspace's runs are scoped to.
    ///
    /// The same value as [`root`](Self::root), and deliberately so: a caller
    /// that opened a relative or symlinked spelling gets back the directory
    /// that spelling named, not the spelling. Both names are kept because
    /// hosts use both.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The workspace root: absolute, symlinks followed, resolved once at
    /// [`open`](Self::open). What the run header reports, what the agent is
    /// based in, what the runtime's policy roots are built from, and what
    /// [`fingerprint`](Self::fingerprint) reads — one directory under one
    /// spelling, so `workspace` and `context_files` can never disagree.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The model every run from this workspace uses, resolved once.
    pub fn model(&self) -> &str {
        &self.model.id
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The context documents discovered at open, weakest precedence first.
    pub fn context(&self) -> &WorkspaceContext {
        &self.context
    }

    /// The memories discovered at open, name-ordered, a workspace memory
    /// shadowing a global one of the same name.
    ///
    /// Frontmatter only: the index in the system prompt is what a memory
    /// costs by default, and the body stays on disk for the model — or a host
    /// showing its user what the agent remembers — to read on demand. See
    /// [`crate::memory`] for the convention.
    pub fn memories(&self) -> &[Memory] {
        &self.memories
    }

    /// Every skill a run from this workspace could reach at open, after
    /// layering, name-ordered.
    ///
    /// On a private runtime that is exactly this workspace's four roots. On a
    /// shared one (ADR-0018) the registry is the runtime's and additive, so it
    /// is this workspace's roots *and* whatever a sibling workspace open at the
    /// time had registered — which is what a run can actually `load_skill`, and
    /// therefore the honest answer to what this reports.
    /// [`LoadedSkill::root`](crate::run::LoadedSkill::root) is how to tell the
    /// two apart: a root under [`root`](Self::root) is this repository's.
    ///
    /// A snapshot, taken once at open like everything else here. A sibling that
    /// opens afterwards adds skills this list does not name, and one that drops
    /// takes its own away — since mentra 0.24 a workspace hands its roots back
    /// when it goes, so a shared runtime no longer accumulates the skills of
    /// every repository a host has ever opened on it.
    pub fn skills(&self) -> &[LoadedSkill] {
        &self.skills
    }

    /// The prompt templates this workspace defines, after layering,
    /// name-ordered. Over ACP these become the client's commands.
    pub fn templates(&self) -> &[Template] {
        &self.templates
    }

    /// The MCP servers connected at open, by the names that took effect —
    /// which is the configured name unless another workspace on the shared
    /// runtime already held it, in which case it carries a deterministic
    /// suffix. Names only: nothing here echoes a command or a credential.
    pub fn mcp_servers(&self) -> &[String] {
        &self.mcp_servers
    }

    /// The tools this workspace's manifests declared, by name, after layering
    /// — this workspace's own first, name-ordered within each manifest.
    ///
    /// Names only, for [`mcp_servers`](Self::mcp_servers)'s reason: nothing
    /// here echoes a command or a credential.
    pub fn declared_tools(&self) -> &[String] {
        &self.declared_tools
    }

    /// What `config.json` said about this workspace, and which file said it.
    ///
    /// The answers here are already *in force* — the model below is what they
    /// resolved to — so this is for the host that reports its own
    /// configuration, or that wants to hand the same value to a shared
    /// [`Runtime`]'s builder rather than read the files twice.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The config files that took effect, most specific first.
    ///
    /// Reported the way `.mcp.json`'s and `.basis/tools.json`'s sources are,
    /// and for a milder version of their reason: what this file decides —
    /// which model, which provider — a run already names in its own header, so
    /// the question left is *which file said so*, and a repository that
    /// resolved a model nobody expected should be able to find out in one
    /// place.
    pub fn config_files(&self) -> &[ContextFile] {
        &self.config.files
    }

    /// The tool manifests that took effect, most specific first.
    ///
    /// A file that says which programs the model may run is the last thing that
    /// should apply invisibly, which is why discovery reports its sources the
    /// way `.mcp.json`'s does.
    pub fn declared_tool_files(&self) -> &[ContextFile] {
        &self.declared_tool_files
    }
    /// The mentra runtime the runs are minted on, for a host that wants
    /// mentra's own surface — the task board, teams, the store — alongside
    /// basis's.
    ///
    /// The same bargain as [`PreparedRun::session`]: basis does not hide mentra,
    /// and reaching past basis's surface is a supported thing to do rather than
    /// a workaround. Renamed from `runtime()` when ADR-0018 gave basis a
    /// `Runtime` of its own, so the name says whose surface comes back.
    pub fn mentra_runtime(&self) -> &mentra::Runtime {
        self.runtime.mentra_runtime()
    }

    /// The agent config this mint offers the model: the one built at open, with
    /// every tool on the shared registry that belongs to another workspace
    /// hidden — bridged `mcp__*` tools, and tools a sibling's
    /// `.basis/tools.json` declared.
    ///
    /// Per mint rather than per open, because the shared registry moves as
    /// sibling workspaces come and go, and a roster is honest only about the
    /// registry it was minted against. Hidden rather than unregistered because
    /// these tools belong to a sibling that is still open and still serving
    /// them; what a *dropped* sibling registered is gone from the registry
    /// altogether, taken off with the claim it was held under.
    ///
    /// The same set is published to this workspace's dispatcher entry on the
    /// way out, because one more consumer needs it and cannot ask mentra:
    /// `spawn`, when a [`ChildSpec`](crate::ChildSpec) roster override
    /// replaces the child's cloned `ToolProfile`, has to put these names back
    /// or hand a delegated child the sibling tools its own parent is denied.
    /// Written here rather than at open so both readers see one snapshot —
    /// the config below freezes it for this mint, and the cell carries the
    /// same names to whatever that mint delegates.
    fn minted_agent(&self, profile: &RunProfile) -> AgentConfig {
        // A run profile replaces workspace defaults first. Foreign tools are
        // then denied against the live shared registry, so an exact roster can
        // narrow what the workspace offered but can never grant a sibling's
        // capability.
        let mut agent = profile.apply_to(self.agent.clone());
        let mut foreign = BTreeSet::new();

        for name in self
            .runtime
            .foreign_declared_tools(self.declared_registration.root())
        {
            agent.tool_profile.hidden_tools.insert(name.clone());
            foreign.insert(name);
        }

        #[cfg(feature = "mcp")]
        for descriptor in self.runtime.mentra_runtime().tools() {
            let name = &descriptor.provider.name;
            if let Some((server, _)) = mentra::mcp::parse_mcp_tool_name(name)
                && !self.mcp_servers.iter().any(|own| own == server)
            {
                agent.tool_profile.hidden_tools.insert(name.clone());
                foreign.insert(name.clone());
            }
        }

        *self
            .foreign_tools
            .write()
            .expect("foreign tool set poisoned") = foreign;

        agent
    }

    /// Wraps a freshly created or resumed session in the run context this
    /// workspace describes.
    ///
    /// Shared by [`prepare`](Self::prepare) and [`resume`](Self::resume) so the
    /// two cannot disagree about what a run from this workspace reports.
    fn minted(
        &self,
        session: Session,
        spec: RunSpec,
        model: String,
        context_snapshot: Option<String>,
    ) -> PreparedRun {
        let bounds = spec.turn_options();

        PreparedRun::new(
            session,
            RunContext {
                workspace: self.root.clone(),
                prompt: spec.prompt,
                provider: self.provider.clone(),
                model,
                context: self.context.clone(),
                skills_dirs: self.skills_registration.dirs().to_vec(),
                skills: self.skills.clone(),
                templates_dirs: self.templates_dirs.clone(),
                templates: self.templates.clone(),
                mcp_files: self.mcp_files.clone(),
                mcp_servers: self.mcp_servers.clone(),
            },
        )
        .with_bounds(bounds)
        // The runtime's answer to how patiently a failing provider is waited
        // out, for the same reason and in the same place: it describes the
        // provider connection this workspace borrows (ADR-0018), and mentra
        // takes it per run, so the mint is where a runtime-scoped knob becomes
        // a per-run option.
        .with_retry_policy(self.runtime.retry_policy())
        .with_context_snapshot(context_snapshot)
    }
}

/// Ensures host-resolved model metadata names the provider this workspace's
/// runtime actually registered.
///
/// Kept in the synchronous prepare/resume path so the refusal precedes mint,
/// session lookup, provider requests, and tool activity.
fn validate_model_provider(model: &ModelInfo, runtime_provider: &str) -> Result<(), RunError> {
    if model.provider.as_str() == runtime_provider {
        return Ok(());
    }

    Err(RunError::ResolvedModelProviderMismatch {
        model: model.id.clone(),
        model_provider: model.provider.as_str().to_string(),
        runtime_provider: runtime_provider.to_string(),
    })
}

/// Whether `session`'s live model is still the one this workspace resolved.
///
/// True immediately after [`Runtime::mint`](crate::runtime::Runtime::mint) —
/// a fresh session is always created on `model` — and after [`Workspace::resume`]
/// reapplies it; false when a resumed conversation had
/// [`PreparedRun::set_model`] move it somewhere else, which nothing here may
/// overwrite with a guess.
fn session_on_resolved_model(session: &Session, model: &ModelInfo) -> bool {
    session.metadata().model == model.id
}

/// Asks the model for a reasoning effort, when one was requested.
///
/// The spec's own answer first, then whatever `config.json` set for this
/// workspace: a flag or a `RunSpec` describes this invocation and the file
/// describes the repository, so the more specific one holds — the same
/// ordering every other key in that file follows.
///
/// `None` on both leaves the session untouched instead of sending a default
/// nobody asked for. Mentra's provider adapter validates the requested level and maps
/// it to that API's wire format.
fn apply_effort(session: &mut Session, effort: Option<Effort>) -> Result<(), RunError> {
    let Some(effort) = effort else {
        return Ok(());
    };

    session.set_reasoning(Some(ReasoningOptions {
        effort: Some(effort.into()),
        summary: None,
    }))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concurrent minting is the point of the split, and it holds only if a
    /// workspace can be shared across tasks and threads. Asserted at compile
    /// time so a future field that is neither cannot slip in unnoticed.
    #[test]
    fn a_workspace_can_be_shared_across_tasks() {
        const fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<Workspace>();
        assert_send_sync::<RunSpec>();
    }

    /// A session freshly created on `model` is trivially on it — the case
    /// every `prepare` mint is in, and what makes `resume`'s own check of the
    /// same function correct: nothing distinguishes "just minted" from
    /// "resumed and still on the same model" once the session exists.
    #[test]
    fn a_freshly_created_session_is_on_its_own_model() {
        let mock = mentra::test::MockRuntime::builder()
            .model("gpt-5", "openai")
            .build()
            .expect("mock runtime builds");
        let model = mock.model();
        let session = mock
            .runtime()
            .create_session("s", model.clone())
            .expect("session");

        assert!(session_on_resolved_model(&session, &model));
    }

    /// The case `resume` must not paper over: a conversation `PreparedRun::set_model`
    /// already moved elsewhere is not this workspace's window to guess at.
    #[test]
    fn a_session_on_a_different_model_does_not_match() {
        let mock = mentra::test::MockRuntime::builder()
            .model("gpt-5", "openai")
            .build()
            .expect("mock runtime builds");
        let session = mock
            .runtime()
            .create_session("s", mock.model())
            .expect("session");
        let workspace_model = ModelInfo::new("gpt-6", "openai");

        assert!(!session_on_resolved_model(&session, &workspace_model));
    }
}
