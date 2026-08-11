//! Opening a workspace: everything a run should only have to discover once.
//!
//! This is the resolution that used to happen inside `prepare()`, per run —
//! context discovery, credential lookup, the runtime build that opens MCP
//! connections, model resolution, skill registration, template loading, hook
//! loading. ADR-0010 asked for it to happen once and for runs to be minted from
//! the result, because a twenty-agent fan-out should read `AGENTS.md` once
//! rather than twenty times, and should not open twenty copies of every MCP
//! server.
//!
//! Everything settled here is settled for the life of the [`Workspace`]. What a
//! caller can still change per run lives in [`RunSpec`](super::RunSpec).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mentra::{
    BuiltinProvider, ModelSelector, ProviderId, Runtime, RuntimePolicy,
    agent::{AgentConfig, WorkspaceConfig as MentraWorkspaceConfig},
    provider_core::{StaticCredentialSource, responses, responses::ResponsesProvider},
};

#[cfg(feature = "mcp")]
use crate::mcp::{self, McpConfig};
use crate::{
    approval::ApprovalGate,
    context::{ContextConfig, WorkspaceContext},
    event::ContextFile,
    hooks::{self, HookRunner, HooksConfig, Interceptor},
    provider,
    run::{LoadedSkill, RunError},
    shell::ShellAccess,
    skills::{self, SkillsConfig},
    store,
    templates::{self, Template, TemplatesConfig},
};

use super::Workspace;

/// How a workspace is opened.
///
/// Named a builder rather than a config because it is one: it exists to be
/// filled in and then consumed by [`open`](Self::open). The type mentra calls
/// `WorkspaceConfig` is a different thing entirely — the agent's base directory
/// — and lan sets that from this one rather than exposing it.
///
/// Fields are private, unlike [`RunConfig`](crate::RunConfig)'s, because one of
/// them is a credential. `with_*` returns a new value, so a host can keep a
/// half-configured builder and finish it differently per workspace.
pub struct WorkspaceBuilder {
    path: PathBuf,
    provider: Option<BuiltinProvider>,
    base_url: Option<String>,
    api_key: Option<String>,
    model: ModelSelector,
    context: ContextConfig,
    skills: SkillsConfig,
    #[cfg(feature = "mcp")]
    mcp: McpConfig,
    templates: TemplatesConfig,
    hooks: HooksConfig,
    interceptors: Vec<Arc<dyn Interceptor>>,
    shell: ShellAccess,
    history: Option<History>,
}

/// What a caller said about where this workspace's conversations go.
///
/// One field rather than a directory beside a flag, so that the two knobs which
/// set it cannot both be in force: whichever was called last is the one that is
/// read, and there is no state in which they disagree. `None` is *unsaid* —
/// mentra chooses, which is neither of these.
#[derive(Debug, Clone, PartialEq, Eq)]
enum History {
    /// [`WorkspaceBuilder::with_store_dir`]: kept in this directory.
    Directory(PathBuf),
    /// [`WorkspaceBuilder::with_ephemeral_history`]: kept in memory, and
    /// nowhere else.
    Ephemeral,
}

/// Hand-written so a supplied credential cannot reach a log through a
/// `{:?}`. Everything else is printed as it is.
impl std::fmt::Debug for WorkspaceBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceBuilder")
            .field("path", &self.path)
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("context", &self.context)
            .field("skills", &self.skills)
            .field("templates", &self.templates)
            .field("hooks", &self.hooks)
            .field(
                "interceptors",
                &self
                    .interceptors
                    .iter()
                    .map(|interceptor| interceptor.name())
                    .collect::<Vec<_>>(),
            )
            .field("shell", &self.shell)
            .field("history", &self.history)
            .finish_non_exhaustive()
    }
}

impl WorkspaceBuilder {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            provider: None,
            base_url: None,
            api_key: None,
            model: ModelSelector::NewestAvailable,
            context: ContextConfig::default(),
            skills: SkillsConfig::default(),
            #[cfg(feature = "mcp")]
            mcp: McpConfig::default(),
            templates: TemplatesConfig::default(),
            hooks: HooksConfig::default(),
            interceptors: Vec::new(),
            // Granted, per ADR-0013, and from the enum's own default rather
            // than from anything ambient: what a run may do is stated here, in
            // configuration, not read out of the environment behind the caller.
            shell: ShellAccess::default(),
            history: None,
        }
    }

    pub fn with_provider(self, provider: BuiltinProvider) -> Self {
        Self {
            provider: Some(provider),
            ..self
        }
    }

    /// Points the workspace at an OpenAI-compatible endpoint. A trailing `/v1`
    /// is stripped during resolution — paste the URL a gateway publishes.
    /// Compatible endpoints use complete local replay rather than automatic
    /// `previous_response_id` chaining.
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            ..self
        }
    }

    /// Supplies the provider credential directly, instead of having lan read it
    /// from the environment.
    ///
    /// ADR-0010 puts provider setup on the workspace, and a host whose key
    /// lives in a vault, a keychain, or a token it just exchanged should not
    /// have to export an environment variable for lan to find it again. Unset
    /// by default, which is the behavior every existing caller has: the key is
    /// looked up by the variable names the ecosystem already uses (see
    /// [`crate::provider`]).
    ///
    /// A key with no [`with_provider`](Self::with_provider) and no
    /// [`with_base_url`](Self::with_base_url) is refused rather than guessed
    /// at — with nothing to attribute it to, lan would be picking a service to
    /// send someone's credential to.
    pub fn with_api_key(self, api_key: impl Into<String>) -> Self {
        Self {
            api_key: Some(api_key.into()),
            ..self
        }
    }

    pub fn with_model(self, model: ModelSelector) -> Self {
        Self { model, ..self }
    }

    pub fn with_context(self, context: ContextConfig) -> Self {
        Self { context, ..self }
    }

    pub fn with_skills(self, skills: SkillsConfig) -> Self {
        Self { skills, ..self }
    }

    /// Sets which MCP servers this workspace connects.
    ///
    /// Servers arrive from three places — the caller's own list, the
    /// workspace's `.mcp.json`, and the global one — and this is where the
    /// first of those goes. See [`crate::mcp`] for the precedence.
    ///
    /// The connections are opened once, by [`open`](Self::open), and every run
    /// minted from the workspace shares them.
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
    /// breaks. [`with_interceptor`](Self::with_interceptor) is the same say,
    /// in this process.
    pub fn with_hooks(self, hooks: HooksConfig) -> Self {
        Self { hooks, ..self }
    }

    /// Gives the host's own code a say over each tool call.
    ///
    /// The in-process binding of ADR-0012's interception contract, and the
    /// sibling of [`with_hooks`](Self::with_hooks): same vocabulary — allow,
    /// deny with a reason, modify with a replacement input — and the same
    /// chain. What it buys is the case a subprocess answers badly, because the
    /// judgement needs something the embedding program is already holding: the
    /// vault handle, the token it just exchanged, the policy it parsed at
    /// startup. Redacting a credential out of a tool's input is the worked
    /// example.
    ///
    /// Appends, so a host may register several; they are consulted in the order
    /// registered, and **before** any subprocess hook. The rule is that the
    /// further a participant is from the workspace's own data, the earlier it
    /// speaks — an interceptor is compiled into this program, while
    /// `.lan/hooks.json` came with a repository — and since the first refusal
    /// short-circuits, that is what lets the host's own guard stop a
    /// repository's program from being spawned at all. It is not a claim of
    /// precedence: a hook still sees, and can still refuse, whatever an
    /// interceptor rewrote.
    ///
    /// Fail-closed carries over unchanged: an interceptor that returns an error
    /// or panics denies the call, and says which one it was.
    ///
    /// Deliberately absent from [`RunConfig`](crate::RunConfig), for the reason
    /// its `api_key` and `store_dir` are: a one-prompt config describes an
    /// invocation and is shaped by an environment, and in-process code cannot
    /// ride on one. A one-shot caller that needs an interceptor takes the
    /// builder from [`RunConfig::split`](crate::RunConfig::split), which is the
    /// documented migration path.
    pub fn with_interceptor(self, interceptor: impl Interceptor + 'static) -> Self {
        Self {
            interceptors: {
                let mut interceptors = self.interceptors;
                interceptors.push(Arc::new(interceptor));
                interceptors
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
    /// Workspace-level rather than per-run because it is baked into the
    /// runtime's policy at build time, and the runtime is what is shared.
    pub fn with_shell(self, shell: ShellAccess) -> Self {
        Self { shell, ..self }
    }

    /// Keeps this workspace's conversations in `dir` rather than in the
    /// machine-wide default.
    ///
    /// Unset, mentra chooses, and what it chooses is keyed by the **process's
    /// current directory** rather than by the workspace lan opened — so a host
    /// that opens two workspaces from one place writes both histories to one
    /// file, and a test suite writes to a real database under the user's data
    /// directory whatever temp directory it opened. Two callers want to say
    /// otherwise: a host that keeps lan's history inside its own application
    /// data, and a test that wants no persistent side effect at all. Both are
    /// asking the same question — *where* — so that is what this takes.
    /// [`with_ephemeral_history`](Self::with_ephemeral_history) answers it with
    /// *nowhere*, and is the last word between the two: whichever was called
    /// last decides.
    ///
    /// Not the store itself, though mentra's `RuntimeBuilder::with_store` would
    /// take one. `RuntimeStore` is a composition of nine traits, and under the
    /// rule written on [`CancellationToken`](crate::CancellationToken) — every
    /// mentra type lan's surface makes a caller *name*, lan re-exports — that
    /// shape would cost the re-export of all nine plus the record types they
    /// pass. What it would buy is reachable without it: mentra ships two
    /// stores, a SQLite file and an in-memory one, and between this and
    /// [`with_ephemeral_history`](Self::with_ephemeral_history) a caller
    /// already picks either without naming a mentra type. A caller that
    /// genuinely wants its own backend still has one, on
    /// [`Workspace::runtime`](super::Workspace::runtime)'s side of the bargain:
    /// build the `Runtime` and drive it directly.
    ///
    /// The directory is created on first write, and lan names the file inside
    /// it — [`store::list_in`](crate::store::list_in) is how the same
    /// conversations are read back, and it has to be able to find them.
    /// Pointing this at [`store::default_directory`](crate::store::default_directory)
    /// is exactly the default.
    ///
    /// Deliberately absent from [`RunConfig`](crate::RunConfig), for the reason
    /// its `api_key` is: a one-prompt config describes an invocation, and where
    /// a machine keeps its history is not something an invocation decides. A
    /// one-shot caller that needs it takes the builder from
    /// [`RunConfig::split`](crate::RunConfig::split), which is the documented
    /// migration path.
    pub fn with_store_dir(self, dir: impl Into<PathBuf>) -> Self {
        Self {
            history: Some(History::Directory(dir.into())),
            ..self
        }
    }

    /// Keeps this workspace's conversations in memory, and nowhere else.
    ///
    /// The sibling of [`with_store_dir`](Self::with_store_dir), for the caller
    /// whose answer to *where* is *nowhere*. mentra's in-memory store backs it:
    /// no database file is opened, no transcript snapshot is written, no
    /// directory is created, and dropping the [`Workspace`] takes the history
    /// with it.
    ///
    /// **Nothing survives the process.** Inside the workspace a conversation
    /// behaves as it always does — [`resume`](super::Workspace::resume) finds
    /// an agent this workspace minted, because the store lives exactly as long
    /// as the workspace does. Past that edge there is nothing to find. A later
    /// process cannot resume one of these by agent id; a second [`Workspace`]
    /// opened on the same path gets its own empty store rather than this one's
    /// history; and [`store::list_in`](crate::store::list_in) has no file to
    /// read whichever directory it is pointed at, so `session/list` over ACP
    /// reports nothing. There is no flush and no export: a conversation started
    /// here cannot be made durable afterwards, so a host that might want one
    /// later wants [`with_store_dir`](Self::with_store_dir) now.
    ///
    /// Who asks for it. A test suite, which otherwise writes to the real
    /// database under the user's data directory and leaves a temp directory per
    /// run behind to avoid it. And a host whose conversations are genuinely
    /// disposable — a request-scoped run inside a server, a one-shot
    /// classifier — where keeping a transcript is a cost and a disclosure
    /// rather than a feature.
    ///
    /// Setting this and [`with_store_dir`](Self::with_store_dir) is not an
    /// error: they write one field, so the last call wins. That is what every
    /// single-valued knob on this builder already does —
    /// [`with_model`](Self::with_model), [`with_base_url`](Self::with_base_url)
    /// and the rest overwrite, and only [`with_interceptor`](Self::with_interceptor),
    /// which is a list, appends — and it is what makes the half-configured
    /// builder this type advertises usable: a helper that hands out ephemeral
    /// builders can be overridden by the one caller that needs its history kept.
    pub fn with_ephemeral_history(self) -> Self {
        Self {
            history: Some(History::Ephemeral),
            ..self
        }
    }

    /// Does all of it: discovery, credential, runtime, model, skills,
    /// templates, hooks, MCP connections.
    ///
    /// This is the expensive call, and the only one. Everything it settles is
    /// fixed for the life of the returned [`Workspace`]; a run minted from that
    /// workspace does no I/O of its own.
    ///
    /// # What this workspace's conversations are tagged with
    ///
    /// Every agent persisted from here carries
    /// [`store::runtime_identifier`](crate::store::runtime_identifier) for this
    /// workspace, which is what makes [`store::list`](crate::store::list) — and
    /// therefore ACP's `session/list` — able to answer *which conversations
    /// belong to this repository*. Until this was set, lan wrote every
    /// conversation under mentra's `"default"` tag while listing filtered on
    /// the workspace's, so listing had never returned anything.
    ///
    /// Rows written before the fix keep the `"default"` tag and do not appear
    /// in any workspace's list. That is deliberately not migrated, and it costs
    /// nothing measurable: listing never worked, so no client has ever seen
    /// those conversations, and none of them stops being *resumable* —
    /// mentra loads an agent by id alone (`load_agent`), never by identifier,
    /// so [`Workspace::resume`](super::Workspace::resume) still finds them.
    /// Better still, mentra re-tags an agent from the live runtime each time it
    /// persists, so an old conversation joins its workspace's list the first
    /// time it is resumed and used.
    pub async fn open(self) -> Result<Workspace, RunError> {
        let context = WorkspaceContext::discover_with(&self.path, &self.context)?;
        let choice = provider::resolve_with(
            self.provider,
            self.base_url.as_deref(),
            self.api_key.as_deref(),
        )?;

        let builder = Runtime::builder()
            // Which conversations belong to this workspace, which is the only
            // question `session/list` can honestly answer (see `crate::store`).
            // Unset, mentra tags every agent `"default"` and lan's own listing
            // — which filters on this — finds nothing, whatever was persisted.
            .with_runtime_identifier(store::runtime_identifier(&self.path))
            // Path roots are hygiene, not a boundary: per ADR-0004 that is the
            // kernel's job, and per ADR-0013 lan ships no instance of one. What
            // the caller said about commands is passed through as written.
            .with_policy(
                git_protected(RuntimePolicy::workspace_bounded(&self.path), &self.path)
                    .allow_shell_commands(self.shell.is_granted())
                    .allow_background_commands(self.shell.is_granted()),
            )
            // Without an authorizer mentra allows every call unconditionally,
            // and no permission request can ever be raised — so the gate goes
            // on even for a workspace whose runs approve everything (see
            // `crate::approval`).
            .with_tool_authorizer(ApprovalGate::new());

        // Left alone unless the caller said something, because mentra's default
        // is a real database a host may already have history in — moving it, or
        // dropping it on the floor, is a thing to be asked for and never a
        // thing to happen by upgrade.
        let builder = match &self.history {
            Some(History::Directory(dir)) => builder.with_store(store::store_in(dir)),
            Some(History::Ephemeral) => builder.with_store(store::volatile()),
            None => builder,
        };

        // Loaded before the build so a hooks file that does not parse fails the
        // open loudly, rather than at the first tool call — or worse, never.
        //
        // One runner for both bindings rather than one registration each:
        // `with_pre_hook` appends, so several would work, but lan wants the
        // ordering and the short-circuit to be its own (see `crate::hooks`). A
        // workspace with neither an interceptor nor a hooks file registers
        // nothing, so the mechanism costs nothing until someone asks for it.
        let hooks = hooks::load(&self.path, &self.hooks)?;
        let runner = self
            .interceptors
            .into_iter()
            .fold(HookRunner::new(&self.path, hooks), |runner, interceptor| {
                runner.with_interceptor(interceptor)
            });
        let builder = if runner.is_empty() {
            builder
        } else {
            builder.with_pre_hook(runner)
        };

        // Both lists reach the header whether or not this build has MCP in it:
        // what a run reports is a schema clients parse, and a field that
        // vanished with a cargo feature would make the stream's shape depend on
        // how lan was built.
        #[cfg(feature = "mcp")]
        let (builder, mcp_files, mcp_servers) = {
            let (files, servers) = discovered_mcp(&self.path, &self.mcp)?;
            let names: Vec<String> = servers
                .iter()
                .map(|server| server.name().to_string())
                .collect();
            let builder = servers
                .into_iter()
                .fold(builder, |builder, server| match server {
                    mcp::McpServer::Stdio(server) => builder.with_mcp_server(server),
                    mcp::McpServer::Sse(server) => builder.with_mcp_sse_server(server),
                });

            (builder, files, names)
        };
        #[cfg(not(feature = "mcp"))]
        let (mcp_files, mcp_servers): (Vec<ContextFile>, Vec<String>) = (Vec::new(), Vec::new());

        let runtime = match &choice.base_url {
            Some(base_url) => {
                builder.with_registered_provider(compatible_provider(base_url, &choice.api_key))
            }
            None => builder.with_provider(choice.provider, choice.api_key.clone()),
        }
        // `build` ignores MCP configuration outright; only `build_async` opens
        // the connections. Always the async one, so a server can never be
        // dropped by the choice of constructor.
        .build_async()
        .await?;

        let model = runtime.resolve_model(choice.provider, self.model).await?;

        // Skills must be registered on the runtime before any session spawns,
        // so every agent's tool roster includes `load_skill`.
        let skills_dirs = register_skills(&runtime, &self.path, &self.skills)?;
        let skills = runtime
            .skills()
            .into_iter()
            .map(|skill| LoadedSkill {
                name: skill.name,
                description: skill.description,
                path: skill.path,
            })
            .collect();

        // Templates need no runtime registration — they are lan-side convention
        // data, rendered into a prompt by whatever surface offers them.
        let (templates_dirs, templates) = load_templates(&self.path, &self.templates)?;

        Ok(Workspace {
            root: resolved_workspace(&self.path, &context),
            agent: agent_config(&self.path, &context),
            path: self.path,
            runtime,
            provider: ProviderId::from(choice.provider).to_string(),
            model,
            context,
            skills_dirs,
            skills,
            templates_dirs,
            templates,
            mcp_files,
            mcp_servers,
        })
    }
}

/// Registers the MCP servers this workspace connects, and reports what took
/// effect.
///
/// Servers are registered on the builder and connected by `build_async`, so
/// this must happen before the build. mentra's `McpRegistration` is private,
/// which is why the fold matches in [`WorkspaceBuilder::open`] rather than in
/// [`crate::mcp`].
///
/// Discovery runs for its own sake as well: the header names which files took
/// effect, and an `.mcp.json` is the last thing that should apply invisibly —
/// it says which programs to spawn.
#[cfg(feature = "mcp")]
fn discovered_mcp(
    workspace: &Path,
    config: &McpConfig,
) -> Result<(Vec<ContextFile>, Vec<mcp::McpServer>), RunError> {
    let files: Vec<ContextFile> = mcp::discover(workspace, config)?
        .iter()
        .map(|source| ContextFile {
            path: source.path.clone(),
            scope: source.scope.label(),
        })
        .collect();

    Ok((files, mcp::servers(workspace, config)?))
}

/// Registers every skills directory that exists, most specific first.
///
/// Roots layer rather than replace, so a workspace skill shadows a personal one
/// of the same name and everything else from the global root still loads.
fn register_skills(
    runtime: &Runtime,
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
/// A root that exists but holds a file lan cannot read is an error rather than
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

/// Builds a provider aimed at an OpenAI-compatible endpoint.
///
/// mentra's OpenAI preset is the right shape — the Responses wire format and
/// bearer auth — so lan takes that definition, swaps the base URL, and disables
/// automatic Hybrid HTTP state chaining. Building on the preset avoids
/// describing a provider from scratch and drifting from whatever mentra learns
/// next.
fn compatible_provider(base_url: &str, api_key: &str) -> ResponsesProvider<StaticCredentialSource> {
    let mut definition = responses::openai_definition();
    definition.base_url = Some(base_url.to_string());
    definition.descriptor.display_name = Some(format!("OpenAI-compatible ({base_url})"));

    // A compatible endpoint promises the Responses wire shape, not every
    // optional OpenAI extension. LAN already replays the complete local
    // transcript, so do not probe `previous_response_id` support with a
    // request that may fail; native provider presets retain Hybrid chaining.
    ResponsesProvider::new(definition, StaticCredentialSource::new(api_key))
        .without_hybrid_http_previous_response_id()
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

/// Keeps the parts of `.git` that decide what *runs* out of reach.
///
/// `.git/hooks` holds programs git executes on ordinary operations, and
/// `.git/config` can name more of them (`core.hooksPath`, and the `filter`/
/// `diff` drivers that run on checkout). Writing either turns a file edit into
/// code execution outside anything lan's policy or approval covers, which is
/// why they are singled out rather than denying `.git` wholesale — an agent
/// legitimately reads `.git`, and `git` itself must keep writing objects and
/// refs underneath it.
///
/// **This binds the builtin file tools, not the shell.** A command like
/// `sh -c 'echo … > .git/hooks/pre-commit'` still reaches the path, because
/// nothing here parses shell. It closes the route a model actually takes and
/// remains hygiene; per ADR-0004 and ADR-0013 the boundary is the OS's, and
/// lan does not ship one.
fn git_protected(policy: RuntimePolicy, workspace: &Path) -> RuntimePolicy {
    let git = workspace.join(".git");
    policy
        .with_denied_write_root(git.join("hooks"))
        .with_denied_write_root(git.join("config"))
}

/// Turns discovered context into the agent's system prompt, and scopes the
/// agent to the workspace. Everything else stays at mentra's defaults —
/// opinions belong in the prompt and the workspace, not here.
///
/// Built once and cloned per run, because none of its inputs are per-run.
fn agent_config(workspace: &Path, context: &WorkspaceContext) -> AgentConfig {
    AgentConfig {
        system: context.render(),
        workspace: MentraWorkspaceConfig {
            base_dir: workspace.to_path_buf(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
