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

use std::path::{Path, PathBuf};

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
    hooks::{self, HookRunner, HooksConfig},
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
    shell: ShellAccess,
    store_dir: Option<PathBuf>,
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
            .field("shell", &self.shell)
            .field("store_dir", &self.store_dir)
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
            // Granted, per ADR-0013, and from the enum's own default rather
            // than from anything ambient: what a run may do is stated here, in
            // configuration, not read out of the environment behind the caller.
            shell: ShellAccess::default(),
            store_dir: None,
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
    /// breaks.
    pub fn with_hooks(self, hooks: HooksConfig) -> Self {
        Self { hooks, ..self }
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
    ///
    /// Not the store itself, though mentra's `RuntimeBuilder::with_store` would
    /// take one. `RuntimeStore` is a composition of nine traits, and under the
    /// rule written on [`CancellationToken`](crate::CancellationToken) — every
    /// mentra type lan's surface makes a caller *name*, lan re-exports — that
    /// shape would cost the re-export of all nine plus the record types they
    /// pass. What it would buy is a choice nobody can make: mentra's stores are
    /// SQLite files, and both of them are constructed from a path. A caller
    /// that genuinely wants its own backend still has one, on
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
            store_dir: Some(dir.into()),
            ..self
        }
    }

    /// Does all of it: discovery, credential, runtime, model, skills,
    /// templates, hooks, MCP connections.
    ///
    /// This is the expensive call, and the only one. Everything it settles is
    /// fixed for the life of the returned [`Workspace`]; a run minted from that
    /// workspace does no I/O of its own.
    pub async fn open(self) -> Result<Workspace, RunError> {
        let context = WorkspaceContext::discover_with(&self.path, &self.context)?;
        let choice = provider::resolve_with(
            self.provider,
            self.base_url.as_deref(),
            self.api_key.as_deref(),
        )?;

        let builder = Runtime::builder()
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

        // Left alone unless the caller said where, because mentra's default is
        // a real database a host may already have history in — moving it is a
        // thing to be asked for, never a thing to happen by upgrade.
        let builder = match &self.store_dir {
            Some(dir) => builder.with_store(store::store_in(dir)),
            None => builder,
        };

        // Loaded before the build so a hooks file that does not parse fails the
        // open loudly, rather than at the first tool call — or worse, never.
        //
        // One runner for every hook rather than one registration each:
        // `with_pre_hook` appends, so both work, but lan wants the ordering and
        // the short-circuit to be its own (see `crate::hooks`). A workspace
        // with no hooks registers nothing, so the mechanism costs nothing until
        // someone writes the file.
        let hooks = hooks::load(&self.path, &self.hooks)?;
        let builder = if hooks.is_empty() {
            builder
        } else {
            builder.with_pre_hook(HookRunner::new(&self.path, hooks))
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
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use crate::context::{ContextDocument, ContextScope};

    use super::*;

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut header_end = None;
        let mut content_length = 0_usize;

        loop {
            let read = stream.read(&mut buffer).expect("read request");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if header_end.is_none()
                && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let end = index + 4;
                header_end = Some(end);
                let headers = String::from_utf8_lossy(&bytes[..end]);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                    .unwrap_or_default();
            }
            if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
                break;
            }
        }

        String::from_utf8(bytes).expect("request should be utf8")
    }

    fn spawn_two_response_server() -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("read server address");
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 1..=2 {
                let (mut stream, _) = listener.accept().expect("accept request");
                requests.push(read_http_request(&mut stream));
                let response_id = format!("resp_{index}");
                let body = format!(
                    concat!(
                        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{}\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}}}\n\n",
                        "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{}\",\"model\":\"gpt-5\",\"status\":\"completed\"}}}}\n\n"
                    ),
                    response_id, response_id
                );
                let response = format!(
                    concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "connection: close\r\n",
                        "content-type: text/event-stream\r\n",
                        "content-length: {}\r\n\r\n",
                        "{}"
                    ),
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
            requests
        });

        (format!("http://{address}/"), handle)
    }

    #[tokio::test]
    async fn compatible_provider_skips_automatic_previous_response_id_chaining() {
        let (base_url, handle) = spawn_two_response_server();
        let provider = compatible_provider(&base_url, "test-key");

        for (index, message) in ["first", "second"].into_iter().enumerate() {
            let request = mentra::provider_core::Request {
                model: Cow::Borrowed("gpt-5"),
                system: None,
                messages: Cow::Owned(vec![mentra::Message::user(mentra::ContentBlock::text(
                    message,
                ))]),
                tools: Cow::Owned(Vec::new()),
                tool_choice: None,
                temperature: None,
                max_output_tokens: None,
                metadata: Cow::Owned(BTreeMap::new()),
                provider_request_options: Default::default(),
            };
            let mut stream = provider
                .session()
                .stream_response(request)
                .await
                .expect("compatible provider should stream");
            while let Some(event) = stream.recv().await {
                event.expect("response event should decode");
            }
            if index == 0 {
                assert_eq!(
                    provider.session().latest_response_id().as_deref(),
                    Some("resp_1"),
                    "the second request must have provider state available to suppress"
                );
            }
        }

        let requests = handle.join().expect("server should capture requests");
        for request in requests {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            let payload: serde_json::Value =
                serde_json::from_str(body).expect("request body should be json");
            assert!(payload.get("previous_response_id").is_none());
        }
    }

    #[test]
    fn context_becomes_the_system_prompt_and_the_workspace_is_scoped() {
        let context = WorkspaceContext::from_documents(vec![ContextDocument {
            path: PathBuf::from("/repo/AGENTS.md"),
            scope: ContextScope::Workspace,
            content: "house rules".to_string(),
        }]);

        let agent = agent_config(Path::new("/repo"), &context);

        assert!(
            agent
                .system
                .expect("a system prompt")
                .contains("house rules")
        );
        assert_eq!(agent.workspace.base_dir, PathBuf::from("/repo"));
    }

    #[test]
    fn an_empty_workspace_context_leaves_the_system_prompt_unset() {
        let agent = agent_config(Path::new("/repo"), &WorkspaceContext::default());

        assert_eq!(agent.system, None);
    }

    #[test]
    fn commands_are_available_unless_the_caller_says_otherwise() {
        // ADR-0013: the first `lan "run the tests"` has to work.
        assert!(WorkspaceBuilder::new("/repo").shell.is_granted());
    }

    #[test]
    fn builders_return_new_values() {
        let base = WorkspaceBuilder::new("/repo");
        let derived = base.with_provider(BuiltinProvider::Anthropic);

        assert_eq!(derived.provider, Some(BuiltinProvider::Anthropic));
        assert_eq!(
            WorkspaceBuilder::new("/repo").provider,
            None,
            "a fresh builder detects the provider"
        );
    }

    #[test]
    fn history_goes_where_mentra_puts_it_unless_the_caller_says_otherwise() {
        // The default must stay the default: a host with conversations already
        // in mentra's database would lose sight of them if opening a workspace
        // started relocating the store on its own.
        assert_eq!(WorkspaceBuilder::new("/repo").store_dir, None);
        assert_eq!(
            WorkspaceBuilder::new("/repo")
                .with_store_dir("/elsewhere")
                .store_dir,
            Some(PathBuf::from("/elsewhere"))
        );
    }

    #[test]
    fn a_supplied_credential_is_not_printed() {
        let printed = format!(
            "{:?}",
            WorkspaceBuilder::new("/repo").with_api_key("sk-secret-value")
        );

        assert!(!printed.contains("sk-secret-value"));
        assert!(printed.contains("redacted"));
    }
}
