//! The end basis serves from: a scripted runtime, and a source over it.
//!
//! Substituting the model is what makes these tests free, offline, and
//! repeatable, so everything that does the substituting is here — the two mock
//! runtimes the tests script, and the `SessionSource` that hands basis sessions
//! built on one. `client` is the other end and takes one of these; nothing
//! here knows anything about it.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use basis::{
    PreparedRun, RunError, TurnOptions, approval::ApprovalGate, run::prepare_with_session,
};
use basis_acp::SessionSource;
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};
use tokio::sync::Notify;

/// The runtime identifier every mock here files its agents under. Each mock
/// still gets its own store root, so sharing the name costs nothing and means
/// a test can ask for its own sessions back by a value it knows.
pub(crate) const MOCK_RUNTIME: &str = "basis-acp-tests";

/// Serves sessions over a scripted runtime, so the protocol is exercised
/// without a provider.
pub(crate) struct MockSource {
    mock: Arc<MockRuntime>,
    workspace: PathBuf,
    /// What every turn on a session from this source may spend. Unset for all
    /// but the bound tests, which are the only ones with an allowance to run
    /// out of.
    bounds: TurnOptions,
}

impl MockSource {
    pub(crate) fn new(mock: &Arc<MockRuntime>, workspace: &tempfile::TempDir) -> Self {
        Self {
            mock: Arc::clone(mock),
            workspace: workspace.path().to_path_buf(),
            bounds: TurnOptions::default(),
        }
    }

    /// Bounds every session this source hands out, which is how a test reaches
    /// the bound: `session/prompt` builds its own [`TurnOptions`] for the
    /// cancellation token, and a run's configured bounds are what fill in the
    /// rest.
    pub(crate) fn with_bounds(self, bounds: TurnOptions) -> Self {
        Self { bounds, ..self }
    }

    /// The workspace is the temp dir rather than the client's cwd: the
    /// scripted runtime is what is under test, not path discovery.
    fn context(&self) -> basis::ContextConfig {
        basis::ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        }
    }
}

#[async_trait::async_trait]
impl SessionSource for MockSource {
    async fn create(
        &self,
        _cwd: PathBuf,
        _mcp: Vec<basis::McpServer>,
    ) -> Result<PreparedRun, RunError> {
        let session = self
            .mock
            .runtime()
            .create_session_with_config(
                "test",
                self.mock.model(),
                mentra::agent::AgentConfig {
                    workspace: mentra::agent::WorkspaceConfig {
                        base_dir: self.workspace.clone(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .expect("session");

        Ok(prepare_with_session(
            session,
            &self.workspace,
            "",
            &self.context(),
            "openai",
            "mock-model",
        )?
        .with_bounds(self.bounds.clone()))
    }

    async fn resume(
        &self,
        agent_id: &str,
        _cwd: PathBuf,
        _mcp: Vec<basis::McpServer>,
    ) -> Result<PreparedRun, RunError> {
        // The mock persists to a real store, so this replays the transcript
        // the way a second process's `session/load` would. It is NOT the
        // production resume whole: `Workspace::resume` goes through
        // `Runtime::resume_minted`, which also clears the conversation's
        // session-scope approval rules at attach — this raw
        // `resume_session` skips that, so a protocol-level test of
        // for-this-session answers dying at `session/load` cannot be pinned
        // through this harness.
        let session = self.mock.runtime().resume_session(agent_id)?;

        Ok(prepare_with_session(
            session,
            &self.workspace,
            "",
            &self.context(),
            "openai",
            "mock-model",
        )?
        .with_bounds(self.bounds.clone()))
    }

    fn lists_sessions(&self) -> bool {
        true
    }

    fn deletes_sessions(&self) -> bool {
        true
    }

    /// Removes from the mock's own store, the same one `list_sessions` reads
    /// and `resume` opens — so a deletion here is observable both ways.
    async fn delete(&self, agent_id: &str) -> Result<(), RunError> {
        Ok(self.mock.runtime().delete_agent(agent_id)?)
    }

    /// Enumerates the mock's own store.
    ///
    /// Scoped by the mock's runtime identifier rather than by `cwd`: what is
    /// under test here is the protocol — that a client asks, and gets back the
    /// conversations as `SessionInfo` — not basis's workspace-scoping scheme.
    /// That scheme is what `ConfiguredSource` uses in the served binary, and
    /// `basis`'s `tests/workspace.rs` is where a conversation is actually
    /// written and then found again under its workspace's tag.
    async fn list_sessions(&self, _cwd: PathBuf) -> Result<Vec<basis::PersistedSession>, RunError> {
        Ok(self
            .mock
            .runtime()
            .list_persisted_agents(MOCK_RUNTIME)?
            .into_iter()
            .filter(|agent| !agent.is_teammate)
            .map(|agent| basis::PersistedSession {
                agent_id: agent.id,
                name: agent.name,
                messages: agent.history_len,
                created_at: agent.created_at,
                updated_at: agent.updated_at,
            })
            .collect())
    }
}

pub(crate) fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

pub(crate) fn text_mock(chunks: &[&str]) -> MockRuntime {
    MockRuntime::builder()
        .model("mock-model", "openai")
        .runtime_identifier(MOCK_RUNTIME)
        .with_policy(RuntimePolicy::permissive())
        .stream_text(chunks.to_vec())
        .build()
        .expect("mock runtime builds")
}

/// Synchronization and wire capture for the in-flight `/compact` regression.
///
/// The provider blocks only after accepting the compaction request. The test
/// can therefore issue `session/cancel` while a real provider future is still
/// in flight, then release the old implementation to prove that it ignored
/// the notification rather than merely timing out.
#[derive(Clone)]
pub(crate) struct CompactBlocker {
    started: Arc<AtomicBool>,
    entered: Arc<Notify>,
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<mentra::Request<'static>>>>,
}

impl CompactBlocker {
    pub(crate) async fn wait_until_compaction_started(&self) {
        if !self.started.load(Ordering::Acquire) {
            self.entered.notified().await;
        }
    }

    pub(crate) fn requests(&self) -> Vec<mentra::Request<'static>> {
        self.requests
            .lock()
            .expect("compact request log poisoned")
            .clone()
    }
}

struct BlockingCompactProvider {
    model: mentra::ModelInfo,
    blocker: CompactBlocker,
}

#[async_trait::async_trait]
impl mentra::Provider for BlockingCompactProvider {
    fn descriptor(&self) -> mentra::ProviderDescriptor {
        mentra::ProviderDescriptor::new(self.model.provider.clone())
    }

    fn capabilities(&self) -> mentra::ProviderCapabilities {
        mentra::ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<mentra::ModelInfo>, mentra::ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(
        &self,
        request: mentra::Request<'_>,
    ) -> Result<mentra::ProviderEventStream, mentra::ProviderError> {
        let call = self.blocker.calls.fetch_add(1, Ordering::AcqRel);
        self.blocker
            .requests
            .lock()
            .expect("compact request log poisoned")
            .push(request.into_owned());

        if call == 2 {
            self.blocker.started.store(true, Ordering::Release);
            self.blocker.entered.notify_one();
            std::future::pending::<()>().await;
        }

        let text = match call {
            0 => "seed answer",
            1 => "second answer",
            2 => r#"{"goal":"compacted","progress":"seed"}"#,
            _ => "after cancel answer",
        };

        Ok(mentra::provider_event_stream_from_response(
            mentra::provider::Response {
                id: format!("compact-test-{call}"),
                model: self.model.id.clone(),
                role: mentra::Role::Assistant,
                content: vec![mentra::ContentBlock::text(text)],
                stop_reason: None,
                usage: None,
            },
        ))
    }
}

/// A source backed by a provider whose compaction request can be held open.
/// This stays in the test harness: production sources continue to use the
/// ordinary `MockRuntime` and no public API is widened for a test seam.
pub(crate) struct BlockingCompactSource {
    runtime: Arc<mentra::Runtime>,
    model: mentra::ModelInfo,
    workspace: PathBuf,
}

pub(crate) fn blocking_compact_source(
    workspace: &tempfile::TempDir,
) -> (BlockingCompactSource, CompactBlocker) {
    let blocker = CompactBlocker {
        started: Arc::new(AtomicBool::new(false)),
        entered: Arc::new(Notify::new()),
        calls: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let model = mentra::ModelInfo::new("mock-model", "openai");
    let runtime = mentra::Runtime::empty_builder()
        .with_provider_instance(BlockingCompactProvider {
            model: model.clone(),
            blocker: blocker.clone(),
        })
        .with_store(mentra::runtime::VolatileRuntimeStore::new())
        .build()
        .expect("blocking runtime builds");

    (
        BlockingCompactSource {
            runtime: Arc::new(runtime),
            model,
            workspace: workspace.path().to_path_buf(),
        },
        blocker,
    )
}

#[async_trait::async_trait]
impl SessionSource for BlockingCompactSource {
    async fn create(
        &self,
        _cwd: PathBuf,
        _mcp: Vec<basis::McpServer>,
    ) -> Result<PreparedRun, RunError> {
        let session = self
            .runtime
            .create_session_with_config(
                "test",
                self.model.clone(),
                mentra::agent::AgentConfig {
                    workspace: mentra::agent::WorkspaceConfig {
                        base_dir: self.workspace.clone(),
                        ..Default::default()
                    },
                    compaction: mentra::agent::CompactionConfig {
                        transcript_dir: self.workspace.join(".transcripts"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .expect("session");

        Ok(prepare_with_session(
            session,
            &self.workspace,
            "",
            &self.context(),
            "openai",
            "mock-model",
        )?)
    }

    fn lists_sessions(&self) -> bool {
        false
    }
}

impl BlockingCompactSource {
    fn context(&self) -> basis::ContextConfig {
        basis::ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        }
    }
}

/// A runtime that wants to write one file, and an authorizer that surfaces the
/// attempt. Without the authorizer nothing is ever asked, so the permission
/// path under test would silently not happen.
pub(crate) fn writing_mock(workspace: &tempfile::TempDir) -> MockRuntime {
    MockRuntime::builder()
        .model("mock-model", "openai")
        .runtime_identifier(MOCK_RUNTIME)
        // Not permissive: the authorizer must have something to prompt about.
        .with_policy(RuntimePolicy::workspace_bounded(workspace.path()))
        .with_tool_authorizer(ApprovalGate::new())
        .tool_calls(vec![MockToolCall::new(
            "files",
            serde_json::json!({
                "operations": [{ "op": "create", "path": "made.txt", "content": "hi" }]
            }),
        )])
        .text("done")
        .build()
        .expect("mock runtime builds")
}
