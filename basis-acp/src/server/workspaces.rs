//! What basis's own sessions are built on: one runtime for the server process,
//! one workspace for each directory a client asks about.
//!
//! ADR-0018's shape at the protocol layer. The default source used to open a
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
//! [`ServeConfig::new`](super::ServeConfig::new). A missing credential is the
//! one setup failure ACP has a remedy for, and it has to arrive as
//! `auth_required` on `session/new` — as it always has — rather than as a
//! server that refused to start. Neither cell remembers a failure either: a
//! `session/new` that failed for want of a credential must fail the same way
//! next time rather than be answered out of a poisoned cache.
//!
//! # What a cached workspace holds open
//!
//! Its MCP connections and its registration on the runtime's hook dispatcher,
//! both of which have to outlive every session minted from it — a workspace
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

use mentra::{
    McpSseLimits, McpSseServerConfig, McpStreamableHttpLimits, McpStreamableHttpServerConfig,
    mcp::SecretString,
};

use basis::{
    McpConfig, McpServer, PersistedSession, PreparedRun, RunError, RunSpec, Runtime,
    RuntimeBuilder, Workspace, WorkspaceBuilder,
};
use tokio::sync::OnceCell;

use super::config::{SessionSource, SessionTemplate};

/// The default source: sessions basis builds itself, on the process's runtime.
pub(super) struct ConfiguredSource {
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
    pub(super) fn new(template: Option<SessionTemplate>) -> Self {
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
    /// [`ServeConfig`](super::ServeConfig) takes. A host that wants to supply
    /// its own runtime supplies its own
    /// [`SessionSource`](super::SessionSource), which is what that seam is for.
    #[cfg(test)]
    pub(super) fn on_runtime(runtime: Arc<Runtime>, template: Option<SessionTemplate>) -> Self {
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
    /// [`mode`](crate::mode)).
    pub(super) fn parts_for(
        &self,
        cwd: PathBuf,
        mcp: Vec<McpServer>,
    ) -> (WorkspaceBuilder, RunSpec) {
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
    pub(super) fn session_mcp(&self, mcp: Vec<McpServer>) -> McpConfig {
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
    pub(super) fn opened(&self) -> Vec<Arc<Workspace>> {
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
    /// would resolve a model over the network to answer a question about a
    /// SQLite table.
    ///
    /// This depends on a conversation being tagged with
    /// [`store::runtime_identifier`](basis::store::runtime_identifier) for
    /// its workspace, and since ADR-0018 that is exactly what a *shared*
    /// runtime cannot do: mentra 0.18 fixes the tag per runtime at build time,
    /// so conversations opened over ACP are filed under the process's tag and
    /// stay out of every per-workspace list until the per-session override
    /// lands upstream — see [`Runtime`](basis::Runtime)'s `mint`, the one
    /// line that closes it. What this still answers for is every conversation
    /// the CLI and the free functions wrote, which do run on a private runtime
    /// per workspace; a row re-files itself the next time it persists under a
    /// runtime that knows its workspace.
    ///
    /// The capability is claimed regardless, because the alternative is worse
    /// in both directions: withdrawing `session/list` hides the conversations
    /// that *are* listed correctly, and re-claiming it later would move the
    /// advertised surface under a client that had already read it.
    async fn list_sessions(&self, cwd: PathBuf) -> Result<Vec<PersistedSession>, RunError> {
        basis::store::list(&cwd)
    }

    fn deletes_sessions(&self) -> bool {
        true
    }

    /// Writes to mentra's store directly, for the reason
    /// [`list_sessions`](Self::list_sessions) reads it directly: opening a
    /// workspace to delete a row would resolve a model over the network to
    /// answer a question about SQLite — and would fail for want of a
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
/// which is the failure [`from_acp`](crate::from_acp) refuses to make, because
/// a session that came up without its servers looks exactly like one whose
/// servers had nothing to offer.
///
/// Two workspaces on one directory is the cost, and it is bounded: they differ
/// only in their supplied servers, so they discover the same hooks and carry
/// the same command posture, and basis's dispatcher — which keys on the
/// directory — is consulting equivalent guards whichever of them it finds.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct WorkspaceKey {
    workspace: PathBuf,
    /// A digest, so a `{:?}` of this key repeats nothing a client configured.
    supplied: u64,
}

impl WorkspaceKey {
    pub(super) fn new(cwd: &Path, supplied: &[McpServer]) -> Self {
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
            // `#[non_exhaustive]`. The name is the one field every variant
            // offers, so it is what keeps two differently named unknown
            // servers from keying one workspace; two unknown servers
            // differing only below the name will collide until this match
            // learns the variant, which the basis-side canary forces.
            server => {
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
    use basis::McpServer;
    use mentra::{McpSseServerConfig, McpStreamableHttpServerConfig};

    use super::digest;

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
}
