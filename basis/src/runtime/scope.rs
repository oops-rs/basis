//! What a workspace's sessions run *as*: their policy, their audience, and
//! their persisted identity.
//!
//! mentra keeps none of these in the agent it persists — they are session
//! options — so basis restates all three on every mint and every resume, and
//! this is the one place either happens. Split out of [`super`] because it is a
//! separate question from the ledgers beside it: those say what is *registered*
//! on a shared runtime and who holds it, and this says which of it a given
//! session can see and what that session may do.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mentra::{
    ModelInfo, RuntimePolicy, Session,
    agent::AgentConfig,
    runtime::{SessionOptions, SessionResumeOptions},
    session::PermissionRuleScope,
    tool::ToolAudience,
};

use crate::{error::RunError, shell::ShellAccess};

use super::{Runtime, builder::execution::workspace_policy};

/// The live scope one workspace's sessions run in: what they may do, and whose
/// tools they can see.
///
/// Both halves are session options upstream, both are deliberately left out of
/// the persisted `AgentConfig`, and both therefore have to be restated on every
/// resume — so they travel as one value rather than as arguments each caller
/// has to remember to keep in step.
#[derive(Debug, Clone)]
pub(crate) struct SessionScope {
    /// [`store::runtime_identifier`](crate::store::runtime_identifier) for the
    /// workspace, in its two roles: the tag this session's persisted rows
    /// carry, and the name of the tool audience it resolves in.
    ///
    /// One identity rather than two, because there is one workspace. A second
    /// string would only be an opportunity for the listing and the roster to
    /// disagree about which repository a session belongs to, and mentra treats
    /// an audience as opaque — it compares for equality and reads nothing into
    /// the value — so the identity basis already derives is exactly what an
    /// audience wants.
    pub(crate) identifier: String,
    /// The complete policy for this session and its descendants; see
    /// [`Runtime::session_policy`].
    pub(crate) policy: RuntimePolicy,
}

impl SessionScope {
    /// The namespace this workspace's own tools are registered under, and the
    /// one its sessions resolve names in.
    pub(crate) fn audience(&self) -> ToolAudience {
        ToolAudience::new(self.identifier.clone())
    }
}

impl Runtime {
    /// The complete policy one workspace's sessions run under.
    ///
    /// Derived here rather than on the workspace because half of it is the
    /// runtime's: [`workspace_policy`] states what the repository asked for,
    /// and [`PolicyShaping`] re-applies what the *builder* was told, which a
    /// per-session policy would otherwise drop — mentra replaces the runtime's
    /// policy wholesale for a session rather than merging with it.
    ///
    /// On a private runtime this is byte-identical to the policy
    /// [`RuntimeBuilder::build_for`] baked, and handing it over again costs
    /// nothing. On a shared one it is the whole point: it is what makes a
    /// `ShellAccess::Denied` workspace, its `.git` carve-out and its memory
    /// roots hold for its own sessions and for nobody else's.
    pub(crate) fn session_policy(
        &self,
        workspace: &Path,
        shell: ShellAccess,
        memory_roots: &[PathBuf],
    ) -> RuntimePolicy {
        self.policy_shaping
            .apply_to(workspace_policy(workspace, shell, memory_roots))
    }

    /// The one place a workspace's sessions are created.
    ///
    /// Every field of the scope is applied per session rather than per runtime,
    /// and for one reason: a shared runtime is built before any workspace
    /// exists, so anything fixed on it is fixed for all of them. The
    /// identifier tags this session's persisted rows, without which a
    /// per-workspace listing could not tell one repository's conversations from
    /// another's; the policy says what this repository's runs may do; and the
    /// audience decides which of the registry's tools they can see. The private
    /// path is unaffected by any of it — a runtime with one workspace already
    /// agreed with itself about all three.
    pub(crate) fn mint(
        &self,
        name: impl Into<String>,
        model: ModelInfo,
        config: AgentConfig,
        scope: &SessionScope,
    ) -> Result<Session, RunError> {
        let options = SessionOptions {
            config,
            policy: Some(scope.policy.clone()),
            tool_audience: Some(scope.audience()),
            project_id: None,
            runtime_identifier: Some(Arc::from(scope.identifier.as_str())),
        };

        Ok(self
            .mentra
            .create_session_with_options(name, model, options)?)
    }

    /// The one place a workspace's sessions are resumed; see
    /// [`mint`](Self::mint) for why it is a place at all.
    ///
    /// The policy and the audience are restated here because mentra
    /// deliberately keeps neither in the persisted agent: a resumed session
    /// handed no policy would inherit the runtime's, which on a shared runtime
    /// is the posture of no workspace at all, and one handed no audience would
    /// resolve only global names — losing this workspace's own bridged and
    /// declared tools between one process and the next.
    ///
    /// **And restating them is exactly why the binding is checked first.** An
    /// agent id says nothing about where its conversation ran — mentra's store
    /// is keyed by agent, not by path — so a caller that picked the workspace
    /// by a client's `cwd` and the conversation by an id it was handed can
    /// bring the two together wrongly. Everything this method then states is
    /// `workspace`'s: the policy carrying its `.git` carve-out, shell posture
    /// and memory roots; the audience deciding which of the shared registry's
    /// tools the model sees. The agent's own `base_dir` does not move with any
    /// of it, and mentra's file tools always allow writes under an agent's
    /// base directory — so a repository whose workspace denies commands and
    /// carves out `.git` would find both true of *another* repository's posture
    /// and neither of its own. The persisted agent's base directory is checked
    /// against this workspace's identity before anything is stated onto it.
    ///
    /// **A "…for this session" answer this session itself gives needs nothing
    /// stated here at all.** mentra 0.27's `PermissionRuleScope::Process`
    /// (mentra#53) is what basis's own approval flow remembers into for that
    /// duration now — a rung owned by one live `SessionPermissionHandle`,
    /// never written to the runtime store — and `resume_session_with_options`
    /// above hands back a session with a fresh handle and an empty rung, for
    /// the same stable agent id or any other. A *new* such answer therefore
    /// needs no clearing, ever.
    ///
    /// **A row a pre-0.12 basis binary already wrote is a different
    /// question, and the clear below still answers it.** Before this
    /// workspace's approval flow existed to remember into `Process`, an
    /// `AllowForSession`/`DenyForSession` answer was remembered into the
    /// durable `Session` scope instead — mentra 0.27 still loads and matches
    /// `Session`-scope rows exactly as it always has (only `Process` rows are
    /// excluded from `load_applicable_rules`), so a row a person answered
    /// under an older basis binary sits in `rules.json` today and would
    /// otherwise be replayed forever against the very session it was
    /// supposed to die with. This is a one-way migration cleanup, not this
    /// workspace's live contract — every row it can ever find here from now
    /// on is somebody else's leftover — and it stays exactly as fallible as
    /// it always was: a store that cannot be rewritten fails the resume
    /// closed rather than silently leaving a stale grant in place. Safe to
    /// retire once no supported basis version can have left a `Session`-scope
    /// row behind.
    pub(crate) fn resume_minted(
        &self,
        agent_id: &str,
        workspace: &Path,
        scope: &SessionScope,
    ) -> Result<Session, RunError> {
        let session = self.mentra.resume_session_with_options(
            agent_id,
            SessionResumeOptions {
                project_id: None,
                policy: Some(scope.policy.clone()),
                tool_audience: Some(scope.audience()),
            },
        )?;

        // Compared as identities rather than as paths, through the one
        // function that decides what "the same workspace" means for the store
        // — so a symlinked or relative spelling on either side is the same
        // answer here as it is in a session listing.
        let based_in = session.config().workspace.base_dir.clone();
        if crate::store::runtime_identifier(&based_in) != scope.identifier {
            return Err(RunError::WorkspaceMismatch {
                agent_id: agent_id.to_owned(),
                workspace: workspace.to_path_buf(),
                agent_workspace: based_in,
            });
        }

        // Legacy-row cleanup only — see the doc comment above. `?` fails the
        // whole resume on a store that reads but cannot be rewritten, because
        // the alternative is a stale grant from a pre-0.12 binary silently
        // applying to a session that never gave it. A store that cannot be
        // *read* would already have failed closed at point of use, so the
        // refusal here adds determinism rather than protection on that half.
        session
            .permission_handle()
            .clear_scope(PermissionRuleScope::Session)
            .map_err(|error| RunError::SessionRulesNotCleared {
                agent_id: agent_id.to_owned(),
                error,
            })?;

        Ok(session)
    }
}
