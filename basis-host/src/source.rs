//! What basis's own sessions are built on: one runtime for the server process,
//! one workspace for each directory a client asks about.
//!
//! ADR-0018's host shape. The original ACP source used to open a
//! whole workspace per
//! session, minting one run from it and dropping it — so
//! a server holding N editor sessions held N mentra runtimes, N provider
//! resolutions and N store handles, and no session outlived the workspace
//! whose MCP connections and hooks it was supposed to be running with. What is
//! here instead is the two lifetimes the ADR named: the runtime belongs to the
//! process, the workspace belongs to the repository, and a session is minted
//! from both.
//!
//! # Built late, and once
//!
//! Both are built on the first session that needs them, not at
//! a server is constructed. Neither cell remembers a failure: a request that
//! failed for want of a credential must fail the same way next time rather
//! than be answered out of a poisoned cache.
//!
//! # What a cached workspace holds open
//!
//! Its MCP connections and its hook registrations on the runtime, both of
//! which have to outlive every session minted from it — a workspace
//! dropped while a session still runs takes that session's servers and its
//! `.basis/hooks.json` with it. Nothing here evicts, therefore: `session/close`
//! reaches basis after the fact, and a turn still unwinding would be the one
//! paying for the tidiness. The bound is the number of distinct directories one
//! server is asked about, which is the number of repositories a person has
//! open.

use std::{
    collections::{BTreeMap, HashMap},
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use basis::{
    McpConfig, McpServer, McpSseServerConfig, McpStreamableHttpServerConfig, PersistedSession,
    PreparedRun, RunError, RunSpec, Runtime, RuntimeBuilder, Workspace, WorkspaceBuilder,
    mcp::{McpSseLimits, McpStreamableHttpLimits, SecretString},
};
use tokio::sync::OnceCell;

use crate::{SessionSource, SessionTemplate};

/// The default source: sessions basis builds itself, on the process's runtime.
pub struct ConfiguredSource {
    /// What the operator said and a client cannot: which model and endpoint,
    /// whether commands are granted, the product's voice. No workspace lives
    /// here — every session brings its own `cwd`.
    template: SessionTemplate,
    /// One per process, however many sessions and workspaces it carries.
    runtime: OnceCell<Arc<Runtime>>,
    /// One per [`WorkspaceKey`], each behind its own cell.
    ///
    /// A sync lock on the map and a cell per entry, rather than one async lock
    /// around the whole thing: opening a workspace spawns MCP servers and may
    /// resolve a model over the network, and two clients on two repositories
    /// must not queue behind each other for that. The map lock is only ever
    /// held long enough to look one entry up.
    workspaces: Mutex<HashMap<WorkspaceKey, Arc<OnceCell<Arc<Workspace>>>>>,
}

impl ConfiguredSource {
    pub fn new(template: Option<SessionTemplate>) -> Self {
        Self {
            template: template.unwrap_or_default(),
            runtime: OnceCell::new(),
            workspaces: Mutex::new(HashMap::new()),
        }
    }

    /// The same source over a runtime the caller already built.
    ///
    /// Test-only because building one resolves a provider credential, which is
    /// exactly what an offline test cannot do and exactly what
    /// [`RuntimeBuilder::with_api_key`](basis::RuntimeBuilder::with_api_key)
    /// answers — and it is not on [`SessionTemplate`], which is the only thing
    /// a configured source takes. A host that wants to supply its own runtime
    /// supplies its own [`SessionSource`], which is what that seam is for.
    #[cfg(test)]
    pub fn on_runtime(runtime: Arc<Runtime>, template: Option<SessionTemplate>) -> Self {
        let source = Self::new(template);
        source
            .runtime
            .set(runtime)
            .unwrap_or_else(|_| unreachable!("a runtime that was just constructed is empty"));
        source
    }

    /// Builds the two halves of one session, in the client's working
    /// directory: the workspace to open and the per-run spec to mint on it.
    ///
    /// Nothing here says anything about approval. A runtime's authorizer is
    /// fixed for its life, so basis installs one that surfaces every
    /// consequential call and answers none of them; which of those the client
    /// actually sees is the session's mode, which can still change (see
    /// [`SessionApproval`](crate::SessionApproval)).
    fn parts_for(&self, cwd: PathBuf, mcp: Vec<McpServer>) -> (WorkspaceBuilder, RunSpec) {
        let template = &self.template;

        let mut builder = Workspace::builder(cwd).with_shell(template.shell);
        if let Some(model) = &template.model {
            builder = builder.with_model(model.clone());
        }
        if let Some(system_prompt) = &template.system_prompt {
            builder = builder.with_system_prompt(system_prompt.clone());
        }
        // Discovery is the template's promise (`SessionTemplate::with_discovery`)
        // — the operator's to point, defaulting to basis's own roots.
        let discovery = &template.discovery;
        let builder = builder
            .with_context(discovery.context.clone())
            .with_skills(discovery.skills.clone())
            .with_templates(discovery.templates.clone())
            .with_hooks(discovery.hooks.clone())
            .with_tools(discovery.tools.clone());

        // The client's servers outrank the workspace's own: it is answering
        // for this session in particular. Discovery still runs, so a
        // `.mcp.json` the client said nothing about is still honored.
        let builder = builder.with_mcp(self.session_mcp(mcp));

        let mut spec = RunSpec::default();
        if let Some(session_name) = &template.session_name {
            spec = spec.with_session_name(session_name.clone());
        }
        if let Some(effort) = template.effort {
            spec = spec.with_effort(effort);
        }

        (builder, spec)
    }

    /// One session's MCP config: where discovery starts, with the client's
    /// servers landing on top — they outrank the workspace's own, because the
    /// client is answering for this session in particular.
    fn session_mcp(&self, mcp: Vec<McpServer>) -> McpConfig {
        self.template.discovery.mcp.clone().with_supplied(mcp)
    }

    /// The workspace this session is minted from, and the per-run half that
    /// mints it.
    async fn workspace_for(
        &self,
        cwd: PathBuf,
        mcp: Vec<McpServer>,
    ) -> Result<(Arc<Workspace>, RunSpec), RunError> {
        let key = WorkspaceKey::new(&cwd, &mcp);
        let (builder, spec) = self.parts_for(cwd, mcp);

        Ok((self.open(key, builder).await?, spec))
    }

    /// The workspace for `key`, opening it if this is the first session to ask.
    async fn open(
        &self,
        key: WorkspaceKey,
        builder: WorkspaceBuilder,
    ) -> Result<Arc<Workspace>, RunError> {
        let cell = Arc::clone(self.lock().entry(key).or_default());

        // Concurrent openers of one key wait on this cell and share its result;
        // openers of different keys never meet. A failed open leaves the cell
        // empty, so the next session tries again rather than inheriting a
        // failure that may have been about the network.
        let workspace = cell
            .get_or_try_init(|| async move {
                let runtime = Arc::clone(self.runtime().await?);

                Ok::<_, RunError>(Arc::new(builder.with_runtime(runtime).open().await?))
            })
            .await?;

        Ok(Arc::clone(workspace))
    }

    /// The process's runtime, built on the first session that needs one.
    async fn runtime(&self) -> Result<&Arc<Runtime>, RunError> {
        self.runtime
            .get_or_try_init(|| async { Ok(Arc::new(self.recipe().build()?)) })
            .await
    }

    /// The process half of the template: what ADR-0018 moved onto the runtime.
    ///
    /// Provider and endpoint seed the private runtime's recipe, plus the model
    /// as this runtime's *policy*. [`parts_for`](Self::parts_for) restates
    /// that model per workspace as an override, so the policy is belt to its
    /// braces — but a runtime that reported a model none of its workspaces use
    /// would be describing a process nobody is running.
    fn recipe(&self) -> RuntimeBuilder {
        let template = &self.template;

        let mut recipe = Runtime::builder();
        if let Some(model) = &template.model {
            recipe = recipe.with_model(model.clone());
        }
        if let Some(provider) = template.provider {
            recipe = recipe.with_provider(provider);
        }
        if let Some(base_url) = &template.base_url {
            recipe = recipe.with_base_url(base_url.clone());
        }

        recipe
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<WorkspaceKey, Arc<OnceCell<Arc<Workspace>>>>> {
        // Poisoned means some other task panicked mid-insert. The map is still
        // structurally sound, and refusing every later session over one panic
        // would turn it into a dead connection — the same ruling the session
        // registry makes.
        self.workspaces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The workspaces this source has open, for the acceptance test that one
    /// directory is one workspace however many sessions were minted on it.
    #[cfg(test)]
    pub fn opened(&self) -> Vec<Arc<Workspace>> {
        self.lock()
            .values()
            .filter_map(|cell| cell.get().map(Arc::clone))
            .collect()
    }
}

#[async_trait::async_trait]
impl SessionSource for ConfiguredSource {
    async fn create(&self, cwd: PathBuf, mcp: Vec<McpServer>) -> Result<PreparedRun, RunError> {
        let (workspace, spec) = self.workspace_for(cwd, mcp).await?;

        workspace.prepare(spec)
    }

    /// The workspace comes from the client's `cwd` and the conversation from
    /// an id the client kept, and nothing here pairs them: a client is free to
    /// send a `cwd` that has nothing to do with the session it names.
    /// `Workspace::resume` is what refuses that pairing — it compares the
    /// persisted agent's own base directory against the workspace's identity
    /// and answers `RunError::WorkspaceMismatch` — which is where the check
    /// belongs, since a resume states *that* workspace's policy and tool
    /// audience onto whatever it picks up.
    async fn resume(
        &self,
        agent_id: &str,
        cwd: PathBuf,
        mcp: Vec<McpServer>,
    ) -> Result<PreparedRun, RunError> {
        let (workspace, spec) = self.workspace_for(cwd, mcp).await?;

        workspace.resume(agent_id, spec)
    }

    fn lists_sessions(&self) -> bool {
        true
    }

    /// Reads mentra's store directly. Building a session to enumerate sessions
    /// would resolve a model over the network to answer a question about files
    /// on disk.
    ///
    /// This depends on a conversation being tagged with
    /// [`store::runtime_identifier`](basis::store::runtime_identifier) for its
    /// workspace, and every session opened here is: `Runtime::mint` states the
    /// identifier per session, so the shared runtime this source runs on files
    /// each `session/new` under the `cwd` it was opened for.
    ///
    /// A conversation that has been *resumed* and run again stays on this
    /// list too: mentra 0.27 retains a persisted agent's own stored runtime
    /// identifier through every later save ([`basis::store`] has the account),
    /// so a row this source's shared runtime resumed re-files under this
    /// workspace's own tag rather than the runtime's.
    async fn list_sessions(&self, cwd: PathBuf) -> Result<Vec<PersistedSession>, RunError> {
        basis::store::list(&cwd)
    }

    fn deletes_sessions(&self) -> bool {
        true
    }

    /// Writes to mentra's store directly, for the reason
    /// [`list_sessions`](Self::list_sessions) reads it directly: opening a
    /// workspace to delete a record would resolve a model over the network to
    /// answer a question about files on disk — and would fail for want of a
    /// credential on a connection that has done nothing but list.
    async fn delete(&self, agent_id: &str) -> Result<(), RunError> {
        basis::store::forget(agent_id)
    }
}

/// What makes two sessions the same workspace.
///
/// The directory is ADR-0018's key and the obvious half. The servers are the
/// other half: `mcpServers` arrives per session, and it is the client
/// answering for *this session in particular*, so two sessions on one
/// repository need not answer alike. Keying on the directory alone would hand
/// the second session the first one's servers and silently drop its own —
/// because a session that came up without its servers looks exactly like one
/// whose servers had nothing to offer.
///
/// Two workspaces on one directory is the cost, and it is bounded: they differ
/// only in their supplied servers, so they discover the same hooks and carry
/// the same command posture.
///
/// That equality is load-bearing now, and worth stating. A workspace registers
/// its interception chain live, for its own tool audience, and two opens of one
/// directory share that audience — so basis counts holders of one chain rather
/// than registering two, and refuses a same-root open whose chain differs
/// (`RunError::WorkspaceGuardConflict`). Nothing *this source decides* can
/// provoke that refusal: every session's hooks come from one `SessionTemplate`'s
/// discovery configuration, which does not vary per session, and the servers a
/// key varies on are not part of a chain at all. What the configuration points
/// at is another matter — the chain is read off disk at each open, so editing
/// `.basis/hooks.json` between two opens of one directory *does* refuse the
/// second, and the refusal names the directory. That is the honest boundary:
/// this source never asks for two chains, and cannot promise a repository
/// stopped changing under it.
///
/// What two live opens of one directory cost is what they always cost — a
/// second set of MCP connections and a second discovery — and not a second pass
/// through the repository's hook programs on every call.
#[derive(Debug, PartialEq, Eq, Hash)]
struct WorkspaceKey {
    workspace: PathBuf,
    /// A digest, so a `{:?}` of this key repeats nothing a client configured.
    supplied: u64,
}

impl WorkspaceKey {
    fn new(cwd: &Path, supplied: &[McpServer]) -> Self {
        Self {
            // Canonicalized so a symlinked spelling and its target are one
            // workspace rather than two, and used as written when it does not
            // resolve — the same ruling as `store::runtime_identifier`, for the
            // same reason: keying a map is not the place to validate a path.
            workspace: std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
            supplied: digest(supplied),
        }
    }
}

/// A digest of the servers a client supplied, values included.
///
/// Everything that decides *what runs with what authority* is hashed: two
/// sessions that named one server with different commands, or one command with
/// a different token, are not asking for the same workspace. A digest rather
/// than the values because this is a map key that outlives the request, and
/// nothing a client configured should be held here in the clear — the same
/// line [`McpServer`]'s own `Debug` draws.
fn digest(servers: &[McpServer]) -> u64 {
    let mut hasher = DefaultHasher::new();

    for server in servers {
        match server {
            McpServer::Stdio(config) => {
                "stdio".hash(&mut hasher);
                config.name.hash(&mut hasher);
                config.command.hash(&mut hasher);
                config.args.hash(&mut hasher);
                config.cwd.hash(&mut hasher);

                // A `HashMap` has no order of its own, and a key that depended
                // on one would differ between two identical requests.
                let mut env: Vec<(&String, &String)> = config.env.iter().collect();
                env.sort_unstable();
                env.hash(&mut hasher);
            }
            // Both remote arms destructure exhaustively — config and limits —
            // so a field mentra adds upstream is a compile error here rather
            // than a value two different sessions silently share a workspace
            // over. The discriminant is hashed like the values: two servers
            // sharing a name and URL but not a transport are not asking for
            // the same workspace.
            McpServer::Sse(config) => {
                let McpSseServerConfig {
                    name,
                    url,
                    headers,
                    allow_plaintext_credentials,
                    limits,
                } = config;
                hash_remote(
                    &mut hasher,
                    "sse",
                    name,
                    url,
                    headers,
                    *allow_plaintext_credentials,
                );

                let McpSseLimits {
                    connect_timeout,
                    initialize_timeout,
                    list_tools_timeout,
                    call_tool_timeout,
                    stream_idle_timeout,
                    max_event_bytes,
                    max_endpoint_bytes,
                    max_tool_pages,
                    max_tools,
                } = limits;
                (
                    connect_timeout,
                    initialize_timeout,
                    list_tools_timeout,
                    call_tool_timeout,
                    stream_idle_timeout,
                )
                    .hash(&mut hasher);
                (
                    max_event_bytes,
                    max_endpoint_bytes,
                    max_tool_pages,
                    max_tools,
                )
                    .hash(&mut hasher);
            }
            McpServer::Http(config) => {
                let McpStreamableHttpServerConfig {
                    name,
                    url,
                    headers,
                    allow_plaintext_credentials,
                    limits,
                } = config;
                hash_remote(
                    &mut hasher,
                    "http",
                    name,
                    url,
                    headers,
                    *allow_plaintext_credentials,
                );

                let McpStreamableHttpLimits {
                    connect_timeout,
                    initialize_timeout,
                    list_tools_timeout,
                    call_tool_timeout,
                    stream_idle_timeout,
                    max_event_bytes,
                    max_response_bytes,
                    max_tool_pages,
                    max_tools,
                } = limits;
                (
                    connect_timeout,
                    initialize_timeout,
                    list_tools_timeout,
                    call_tool_timeout,
                    stream_idle_timeout,
                )
                    .hash(&mut hasher);
                (
                    max_event_bytes,
                    max_response_bytes,
                    max_tool_pages,
                    max_tools,
                )
                    .hash(&mut hasher);
            }
            // A transport this build does not know — the enum is
            // `#[non_exhaustive]`. The discriminant separates two unknown
            // *variants*; the name separates servers within one. What neither
            // separates — two unknown servers of one variant differing only
            // below the name — will key one workspace until this match learns
            // the variant, and the consequence is bounded: the second session
            // inherits the first one's connections rather than being handed a
            // credential. basis's own same-crate matches (`McpServer::name`,
            // its hand-written `Debug`) fail to compile the moment the
            // variant lands, which is what forces this arm to become a real
            // one.
            server => {
                std::mem::discriminant(server).hash(&mut hasher);
                "unknown-transport".hash(&mut hasher);
                server.name().hash(&mut hasher);
            }
        }
    }

    hasher.finish()
}

/// The remote transports' shared shape: discriminant tag, name, URL, ordered
/// headers with their secrets fed into the hash, and the plaintext override —
/// which decides where a credential may travel, so two configs differing only
/// there are not the same authority.
fn hash_remote(
    hasher: &mut DefaultHasher,
    transport: &str,
    name: &str,
    url: &str,
    headers: &BTreeMap<String, SecretString>,
    allow_plaintext_credentials: bool,
) {
    transport.hash(hasher);
    name.hash(hasher);
    url.hash(hasher);
    // Already ordered: mentra types these as a `BTreeMap`.
    for (header, value) in headers {
        header.hash(hasher);
        value.expose_secret().hash(hasher);
    }
    allow_plaintext_credentials.hash(hasher);
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use basis::{
        ContextConfig, McpConfig, McpServer, Runtime, ToolsConfig, hooks::HooksConfig,
        skills::SkillsConfig, templates::TemplatesConfig,
    };
    use mentra::{
        McpServerConfig, McpSseServerConfig, McpStreamableHttpServerConfig, ModelSelector,
    };

    use super::{ConfiguredSource, WorkspaceKey, digest};
    use crate::{Discovery, SessionSource, SessionTemplate};

    #[test]
    fn two_servers_differing_only_in_transport_hash_apart() {
        // Without the discriminant in the hash, an SSE server and a Streamable
        // HTTP server sharing a name and URL would key one workspace, and the
        // second client's transport would silently become the first's.
        let sse = McpServer::Sse(McpSseServerConfig::new("api", "https://example.com/mcp"));
        let http = McpServer::Http(McpStreamableHttpServerConfig::new(
            "api",
            "https://example.com/mcp",
        ));

        assert_ne!(digest(&[sse]), digest(&[http]));
    }

    #[test]
    fn the_plaintext_override_reaches_the_digest() {
        // Latent until basis exposes the knob, armed now: two configs that
        // differ in where a credential may travel are two authorities.
        let base = || McpStreamableHttpServerConfig::new("api", "http://127.0.0.1/mcp");

        assert_ne!(
            digest(&[McpServer::Http(base())]),
            digest(&[McpServer::Http(base().allowing_plaintext_credentials())]),
        );
    }

    #[test]
    fn the_clients_mcp_servers_reach_the_config() {
        let source = ConfiguredSource::new(None);

        let built = source.session_mcp(vec![McpServer::Stdio(McpServerConfig {
            name: "fs".to_string(),
            command: "/bin/mcp-fs".to_string(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
        })]);

        let supplied: Vec<&str> = built.supplied.iter().map(McpServer::name).collect();
        assert_eq!(
            supplied,
            vec!["fs"],
            "the session request is where a client says which servers it wants"
        );
    }

    fn offline_runtime() -> Arc<Runtime> {
        Arc::new(
            Runtime::builder()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_api_key("test-key")
                .with_ephemeral_history()
                .build()
                .expect("a runtime builds without touching the network"),
        )
    }

    fn offline_template() -> SessionTemplate {
        SessionTemplate::new().with_model(ModelSelector::Id("test-model".to_string()))
    }

    fn offline_discovery() -> Discovery {
        Discovery {
            context: ContextConfig {
                file_name: "AGENTS.md".to_string(),
                global_dir: None,
                walk_parents: false,
            },
            skills: SkillsConfig {
                workspace_subdir: Some(PathBuf::from(".basis/skills")),
                shared_workspace_dir: true,
                global_dir: None,
                shared_home_dir: false,
            },
            templates: TemplatesConfig {
                workspace_subdir: PathBuf::from(".basis/templates"),
                global_dir: None,
            },
            hooks: HooksConfig {
                workspace_file: PathBuf::from(".basis/hooks.json"),
                global_dir: None,
                supplied: Vec::new(),
            },
            tools: ToolsConfig {
                workspace_file: PathBuf::from(".basis/tools.json"),
                global_dir: None,
                supplied: Vec::new(),
            },
            mcp: McpConfig {
                workspace_file: PathBuf::from(".mcp.json"),
                global_dir: None,
                supplied: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn two_sessions_on_one_workspace_share_one_runtime() {
        let repository = tempfile::tempdir().expect("tempdir");
        let runtime = offline_runtime();
        let source = ConfiguredSource::on_runtime(
            Arc::clone(&runtime),
            Some(offline_template().with_discovery(offline_discovery())),
        );

        let first = source
            .create(repository.path().to_path_buf(), Vec::new())
            .await
            .expect("the first session opens");
        let second = source
            .create(repository.path().to_path_buf(), Vec::new())
            .await
            .expect("the second session opens");

        assert_ne!(
            first.agent_id(),
            second.agent_id(),
            "two sessions, not one conversation handed out twice"
        );

        let opened = source.opened();
        assert_eq!(
            opened.len(),
            1,
            "one directory is one workspace, however many sessions were minted on it"
        );
        assert!(
            std::ptr::eq(opened[0].mentra_runtime(), runtime.mentra_runtime()),
            "and both were minted on the one runtime this process built"
        );
    }

    /// The claim `list_sessions` is built on, and the one nothing checked: a
    /// session this source opens is filed under the directory it was opened
    /// for, so a client asking about that directory gets it back.
    ///
    /// Worth a test of its own because the doc beside `list_sessions` asserted
    /// the opposite for two releases after it stopped being true — the tag is a
    /// per-session fact now (`Runtime::mint`), and a shared runtime files each
    /// workspace's conversations apart.
    ///
    /// Read through `store::list_in` rather than through `list_sessions`
    /// itself, which resolves the store root the process would use. Opening
    /// that directory in a test is a legitimate refusal on any machine that
    /// ran basis 0.6 (`RunError::LegacyStore`), and that the two roots are one
    /// path is pinned in `basis`'s own `store` tests.
    #[tokio::test]
    async fn a_session_this_source_opens_is_listed_for_its_own_directory() {
        let store = tempfile::tempdir().expect("tempdir");
        let repository = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(
            Runtime::builder()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_api_key("test-key")
                .with_store_dir(store.path())
                .build()
                .expect("a runtime builds without touching the network"),
        );
        let source = ConfiguredSource::on_runtime(
            runtime,
            Some(offline_template().with_discovery(offline_discovery())),
        );

        let opened = source
            .create(repository.path().to_path_buf(), Vec::new())
            .await
            .expect("the session opens");

        let listed: Vec<String> = basis::store::list_in(store.path(), repository.path())
            .expect("lists")
            .into_iter()
            .map(|session| session.agent_id)
            .collect();
        assert_eq!(
            listed,
            vec![opened.agent_id().to_string()],
            "a client asking about this directory must be handed the session it opened here"
        );
        assert!(
            basis::store::list_in(store.path(), elsewhere.path())
                .expect("lists")
                .is_empty(),
            "and no other directory may claim it"
        );
    }

    #[test]
    fn a_session_that_asked_for_different_servers_is_a_different_workspace() {
        let server = |command: &str| {
            vec![McpServer::Stdio(McpServerConfig {
                name: "fs".to_string(),
                command: command.to_string(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
            })]
        };
        let key = |mcp: Vec<McpServer>| WorkspaceKey::new(Path::new("/repo"), &mcp);

        assert_eq!(
            key(Vec::new()),
            key(Vec::new()),
            "the same directory asked about the same way is one workspace"
        );
        assert_ne!(
            key(Vec::new()),
            key(server("/bin/mcp-fs")),
            "a supplied server is not none"
        );
        assert_ne!(
            key(server("/bin/mcp-fs")),
            key(server("/bin/other-fs")),
            "one name, two commands: a client that named a different program must get it"
        );
        assert_ne!(
            key(Vec::new()),
            WorkspaceKey::new(Path::new("/other-repo"), &[]),
            "and a directory is a key"
        );
    }
}
