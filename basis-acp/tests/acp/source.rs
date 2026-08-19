//! The end basis serves from: a scripted runtime, and a source over it.
//!
//! Substituting the model is what makes these tests free, offline, and
//! repeatable, so everything that does the substituting is here — the two mock
//! runtimes the tests script, and the `SessionSource` that hands basis sessions
//! built on one. `client` is the other end and takes one of these; nothing
//! here knows anything about it.

use std::{path::PathBuf, sync::Arc};

use basis::{PreparedRun, RunConfig, RunError, approval::ApprovalGate, run::prepare_with_session};
use basis_acp::SessionSource;
use mentra::{
    RuntimePolicy,
    test::{MockRuntime, MockToolCall},
};

/// The runtime identifier every mock here files its agents under. Each mock
/// still gets its own SQLite file, so sharing the name costs nothing and means
/// a test can ask for its own sessions back by a value it knows.
pub(crate) const MOCK_RUNTIME: &str = "basis-acp-tests";

/// Serves sessions over a scripted runtime, so the protocol is exercised
/// without a provider.
pub(crate) struct MockSource {
    mock: Arc<MockRuntime>,
    workspace: PathBuf,
}

impl MockSource {
    pub(crate) fn new(mock: &Arc<MockRuntime>, workspace: &tempfile::TempDir) -> Self {
        Self {
            mock: Arc::clone(mock),
            workspace: workspace.path().to_path_buf(),
        }
    }

    /// The workspace is the temp dir rather than the client's cwd: the
    /// scripted runtime is what is under test, not path discovery.
    fn config(&self) -> RunConfig {
        RunConfig::new(&self.workspace, "").with_context(basis::ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
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

        prepare_with_session(session, &self.config(), "openai", "mock-model")
    }

    async fn resume(
        &self,
        agent_id: &str,
        _cwd: PathBuf,
        _mcp: Vec<basis::McpServer>,
    ) -> Result<PreparedRun, RunError> {
        // The mock persists to a real store, so this is the same resume a
        // second process would perform — which is what `session/load` is for.
        let session = self.mock.runtime().resume_session(agent_id)?;

        prepare_with_session(session, &self.config(), "openai", "mock-model")
    }

    fn lists_sessions(&self) -> bool {
        true
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
