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
pub(crate) mod dispatch;
mod environment;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mentra::{BuiltinProvider, ModelInfo, ModelSelector, Session, agent::AgentConfig};

pub use builder::RuntimeBuilder;

use crate::{approval::SideEffectLevels, run::RunError};

use dispatch::{HookDispatch, HookRegistration, WorkspaceGuardEntry};

/// One process's substrate: mentra's runtime plus the resolution policy and
/// host guards that were fixed when it was built.
///
/// Shared through an `Arc` by every [`Workspace`](crate::Workspace) opened on
/// it, so N repositories cost one provider resolution and one store handle.
/// `Send` and `Sync`; sessions are minted from `&self`.
pub struct Runtime {
    mentra: mentra::Runtime,
    /// The resolved choice, kept for model resolution at workspace open.
    provider: BuiltinProvider,
    /// The `ProviderId` string workspaces copy into their run headers.
    provider_label: String,
    /// The default model *policy*; a workspace may override the selector, and
    /// the resolved id is always the workspace's own fact.
    model: ModelSelector,
    /// The one pre-hook basis registered, and the registry workspaces join.
    dispatch: Arc<HookDispatch>,
    /// The reading end of the approval gate's side channel, handed to every run
    /// minted on this runtime so an [`ApprovalRequest`](crate::ApprovalRequest)
    /// can say how far the call reaches.
    ///
    /// Held here because the gate is fixed when mentra's runtime is built and
    /// mentra never hands it back, so this is the only moment the handle can be
    /// kept. **Interim**; see
    /// [`SideEffectLevels`](crate::approval::SideEffectLevels) and
    /// [mentra#21](https://github.com/oops-rs/mentra/issues/21).
    levels: SideEffectLevels,
    /// Which workspace owns each MCP server name on this runtime's single tool
    /// registry — bridged tools are namespaced by server, so two workspaces
    /// configuring one name must be told apart here.
    #[cfg(feature = "mcp")]
    mcp_claims: Mutex<HashMap<String, PathBuf>>,
    /// Which workspace owns each declared tool name on the same single
    /// registry. See [`Runtime::claim_declared_tool`] for why this exists and
    /// why a released claim is remembered rather than removed.
    declared_claims: Mutex<HashMap<String, DeclaredClaim>>,
}

/// A declared tool name that has been registered on this runtime at least once.
///
/// `holders` rather than a bare owner because one root may be open twice — a
/// host that opens the same repository for two concurrent callers — and the
/// first of those to drop must not free a name the second is still serving.
#[derive(Debug)]
struct DeclaredClaim {
    root: PathBuf,
    holders: usize,
}

/// Hand-written because mentra's runtime is not `Debug`. No credential lives
/// here — the key was consumed building the provider — so nothing is redacted.
impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("provider", &self.provider_label)
            .field("model", &self.model)
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

    /// The provider this runtime resolves models against, as its `ProviderId`
    /// string — what every run from every workspace on it reports.
    pub fn provider(&self) -> &str {
        &self.provider_label
    }

    /// Resolves the model a workspace will use: its own override when it has
    /// one, this runtime's policy otherwise. The result is the workspace's
    /// fact; the policy is the runtime's (ADR-0018).
    pub(crate) async fn resolve_model(
        &self,
        selector: Option<ModelSelector>,
    ) -> Result<ModelInfo, RunError> {
        let selector = selector.unwrap_or_else(|| self.model.clone());

        Ok(self.mentra.resolve_model(self.provider, selector).await?)
    }

    /// The one place a workspace's sessions are created.
    ///
    /// `persist_identifier` is the workspace tag the session's rows *should*
    /// carry ([`store::runtime_identifier`](crate::store::runtime_identifier)).
    /// mentra 0.18 persists under the runtime-wide identifier fixed at build,
    /// so on a shared runtime the tag cannot yet be applied — the parameter is
    /// accepted and unread so that the upstream per-session override, once it
    /// exists, lands in this one line. The private path is unaffected: its
    /// runtime-wide identifier already is this value.
    pub(crate) fn mint(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
        _persist_identifier: &str,
    ) -> Result<Session, RunError> {
        Ok(self
            .mentra
            .create_session_with_config(name, model, config)?)
    }

    /// The one place a workspace's sessions are resumed; see
    /// [`mint`](Self::mint) for why it is a place at all.
    pub(crate) fn resume_minted(&self, agent_id: &str) -> Result<Session, RunError> {
        Ok(self.mentra.resume_session(agent_id)?)
    }

    /// The side channel this runtime's approval gate writes to, for the mint
    /// that attaches it to a run.
    pub(crate) fn side_effect_levels(&self) -> SideEffectLevels {
        self.levels.clone()
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
    /// A released claim is kept with `holders: 0` rather than removed, because
    /// mentra has no public unregister: the entry a dropped workspace left
    /// behind is still in the registry, and forgetting that basis put it there
    /// would make the same workspace's next open refuse its own tool.
    pub(crate) fn claim_declared_tool(&self, name: &str, root: &Path) -> Result<(), String> {
        let mut claims = self
            .declared_claims
            .lock()
            .expect("declared tool claim map poisoned");

        match claims.get_mut(name) {
            Some(claim) if claim.holders > 0 && claim.root != root => Err(
                "another workspace open on this runtime declares a tool by that name".to_string(),
            ),
            Some(claim) => {
                claim.root = root.to_path_buf();
                claim.holders += 1;
                Ok(())
            }
            None if self.registers_tool(name) => {
                Err("this runtime already offers a tool by that name".to_string())
            }
            None => {
                claims.insert(
                    name.to_string(),
                    DeclaredClaim {
                        root: root.to_path_buf(),
                        holders: 1,
                    },
                );
                Ok(())
            }
        }
    }

    /// Releases a claim [`claim_declared_tool`](Self::claim_declared_tool)
    /// granted. Only the owning root can release, so one workspace's drop
    /// cannot free a name another still serves.
    pub(crate) fn release_declared_tool(&self, name: &str, root: &Path) {
        let mut claims = self
            .declared_claims
            .lock()
            .expect("declared tool claim map poisoned");

        if let Some(claim) = claims.get_mut(name)
            && claim.root == root
        {
            claim.holders = claim.holders.saturating_sub(1);
        }
    }

    /// Every declared tool name on this runtime that belongs to some *other*
    /// workspace, including one that has since been dropped.
    ///
    /// What a mint hides, for the reason it hides another workspace's `mcp__*`
    /// tools: the registry is the runtime's and single, but a tool declared by
    /// a repository is that repository's, and offering it to a run in a
    /// different one would run a program that workspace never asked for.
    pub(crate) fn foreign_declared_tools(&self, root: &Path) -> Vec<String> {
        self.declared_claims
            .lock()
            .expect("declared tool claim map poisoned")
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
