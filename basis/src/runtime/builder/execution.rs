//! Where and how this runtime runs commands — the execution half of
//! [`RuntimeBuilder`](super::RuntimeBuilder).
//!
//! The knobs: how patiently a command is waited out, the fixed environment
//! every spawned process receives, and the named executors `!@<target>`
//! routes to (ADR-0021). And the derivations `build` reaches for from them:
//! the name validation a routable target has to pass, the two
//! `RuntimePolicy` recipes — one bound to a single workspace, one for
//! everything on the runtime that belongs to no workspace — and the
//! [`PolicyShaping`] both of them pass through.
//!
//! Grouped because they answer one question between them and are read
//! together: a target name is validated against the same rule the `!@`
//! parser applies, and the policies are where a command timeout and a shell
//! posture actually land.

use std::path::{Path, PathBuf};

use mentra::RuntimePolicy;

use crate::{
    error::RunError,
    shell::ShellAccess,
    tools::spawn::{LOCAL_TARGET, is_target_name},
};

use super::{CommandTargets, RuntimeBuilder, ToolResultPolicy};

impl RuntimeBuilder {
    /// How long a command may run before it is killed.
    ///
    /// Two minutes by default, which suits the commands a harness usually runs
    /// and does not suit the ones that build software. A host whose agent runs
    /// container builds, test suites, or archives needs to say so: past the
    /// limit the process is killed mid-stream, and what reaches the caller is
    /// truncated output with no error in it — a build that looks like it
    /// failed silently rather than one that was stopped.
    ///
    /// Clamped by mentra's ceiling for the runtime's policy; asking for longer
    /// than that grants the ceiling rather than failing, because a host that
    /// asked for patience should not get less than the default for asking.
    #[must_use]
    pub fn with_command_timeout(self, timeout: std::time::Duration) -> Self {
        Self {
            command_timeout: Some(timeout),
            ..self
        }
    }

    /// Adds one fixed environment value to every process this runtime spawns.
    ///
    /// Mentra clears the ambient environment before running a model command, so
    /// a host must state execution context explicitly. A later call with the
    /// same name replaces the earlier value. Debug output names variables but
    /// redacts values.
    ///
    /// **Every process** is meant literally, and it did not used to be: a
    /// command through [`spawn`](crate::tools::spawn) received these pairs and
    /// a declared tool's program did not, so a host that had told the runtime
    /// where its service lived watched `.basis/tools.json` tools fail at the
    /// far end asking for a variable the runtime was holding. Both get them
    /// now. A declared tool's own `env` block still wins for a name they share,
    /// because that is the tool's own statement about itself
    /// ([`crate::tools::declared`]).
    ///
    /// Runtime-scoped, so on a shared runtime every workspace's commands see
    /// the same pairs. A host that wants two concurrently driven workspaces to
    /// carry different identities gives each its own runtime through
    /// [`WorkspaceBuilder::with_runtime_builder`](crate::WorkspaceBuilder::with_runtime_builder),
    /// which is what the local task service does.
    pub fn with_command_environment(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.command_environment.insert(name.into(), value.into());
        self
    }
}

/// Refuses a target name basis cannot route on, before a runtime is built
/// around it.
///
/// Two rules, and both are about what the name has to survive downstream. It
/// is glob-matched inside a serialized rule pattern and printed into refusals
/// the model reads, so a name carrying a quote, a slash or a space would mean
/// one thing to the operator who wrote the rule and another to the matcher
/// reading it — hence the charset, which is the same predicate the `!@` parser
/// applies, from the same function, so the two can never disagree about which
/// names exist. And `local` is the wire word for *here*
/// ([`LOCAL_TARGET`]), so a target answering to it would make
/// `"target":"local"` mean two things in one field.
///
/// Dormant while nothing can register a target — `with_command_target` was
/// withdrawn unadopted — kept beside the table it validates for the day a
/// registration seam returns.
pub(super) fn validate_target_names(targets: &CommandTargets) -> Result<(), RunError> {
    for name in targets.keys() {
        if !is_target_name(name) {
            return Err(RunError::CommandTarget {
                name: name.clone(),
                reason: "a target name is one or more of letters, digits, `_` and `-`".to_string(),
            });
        }

        if name == LOCAL_TARGET {
            return Err(RunError::CommandTarget {
                name: name.clone(),
                reason: format!(
                    "`{LOCAL_TARGET}` is what the wire contract calls a command that names no \
                     target, so nothing may be registered under it"
                ),
            });
        }
    }

    Ok(())
}

/// The policy one workspace runs under:
/// `git_protected(workspace_bounded(path))`, the caller's shell posture, and
/// the memory roots.
///
/// One recipe for both runtime shapes. A private runtime bakes it at build,
/// and every workspace — private or sharing — also hands it to its own
/// sessions through
/// [`SessionOptions::policy`](mentra::runtime::SessionOptions), which is what
/// makes a shared runtime enforce a per-workspace posture in mentra's own
/// words rather than in a hook of basis's.
///
/// Path roots are hygiene, not a boundary: per ADR-0004 that is the kernel's
/// job, and per ADR-0013 basis ships no instance of one. What the caller said
/// about commands is passed through as written.
///
/// The memory roots ([`crate::memory`]) sit outside the workspace — that is
/// what makes them memory rather than working files — so recall (`read`,
/// `grep`) and writing a memory (`write`, `edit`) need them stated here, on
/// both the read and the write lists. Stated whether or not a directory
/// exists yet: the first memory is written by exactly the run that finds none
/// to read.
///
/// # The shell posture is enforced *inside* the call, and that has a cost
///
/// `allow_shell_commands(shell.is_granted())` is where a workspace's answer
/// about commands stops being a guard of basis's own and becomes a statement
/// in mentra's policy — which is the whole point of this recipe, because a
/// policy is the only thing a shared runtime can carry per session. Mentra's
/// admission order is hooks, then the schema, then the
/// [`ToolAuthorizer`](mentra::tool::ToolAuthorizer); the shell check fires
/// later still, inside the tool's own execution. So on a
/// [`ShellAccess::Denied`] workspace a command reaches
/// [`Approver`](crate::Approver) **first** and is refused **after** it is
/// answered.
///
/// For a `Prompt`-mode approver that is a real cost, and it is not a bug to be
/// fixed here: the person is shown `!curl … | sh` and asked whether to allow
/// it, their yes is recorded, and the model is then told commands are
/// disabled — a prompt about something that could never have run. Nothing is
/// weakened by it (the command does not run either way, and a *deny* is still
/// a deny), and the alternative is worse: refusing before the authorizer takes
/// a second implementation of the shell posture — the pre-hook guard the
/// dispatcher used to carry — which is exactly the duplicate this recipe
/// removed, and which a shared runtime could only apply by routing on a
/// directory. A host that wants the prompt suppressed can read the posture
/// itself and answer [`ApprovalDecision::Deny`](crate::ApprovalDecision)
/// without asking, or refuse in an
/// [`Interceptor`](crate::hooks::Interceptor), which does run before the
/// authorizer. Pinned by `a_denied_command_is_put_to_the_approver_before_the
/// _policy_refuses_it` in `basis/tests/hooks/guarded.rs`.
pub(crate) fn workspace_policy(
    workspace: &Path,
    shell: ShellAccess,
    memory_roots: &[PathBuf],
) -> RuntimePolicy {
    let policy = git_protected(RuntimePolicy::workspace_bounded(workspace), workspace)
        .allow_shell_commands(shell.is_granted())
        .allow_background_commands(shell.is_granted());

    memory_roots.iter().fold(policy, |policy, root| {
        policy
            .with_allowed_read_root(root.clone())
            .with_allowed_write_root(root.clone())
    })
}

/// What a shared runtime falls back to for anything running on it that belongs
/// to no workspace: shell and background on, `workspace_bounded`'s timeouts,
/// and no path roots of its own.
///
/// Every session basis mints carries its workspace's own
/// [`workspace_policy`] instead, so this governs only what a host reaches
/// through [`Runtime::mentra_runtime`](crate::Runtime::mentra_runtime) and
/// creates for itself. Commands are on because ADR-0013 grants them by
/// default. No roots, because mentra's file bounding always allows under the
/// calling agent's `base_dir`: with the list empty, such an agent is confined
/// to its own directory and no workspace's root widens it.
pub(crate) fn shared_policy() -> RuntimePolicy {
    RuntimePolicy::default()
        .allow_shell_commands(true)
        .allow_background_commands(true)
        // workspace_bounded's numbers, restated because that constructor also
        // sets roots this policy must not have. A drift here would give shared
        // and private runtimes different command patience.
        .with_default_command_timeout(std::time::Duration::from_secs(120))
        .with_max_command_timeout(std::time::Duration::from_secs(600))
}

/// What a runtime's builder says about *every* policy it hands out, whichever
/// recipe produced it.
///
/// A per-session policy replaces the runtime's wholesale — mentra's
/// [`SessionOptions::policy`](mentra::runtime::SessionOptions) does not merge
/// or intersect — so a knob a host set on the builder has to be re-applied to
/// each workspace's policy or it silently stops holding for every session on
/// the runtime. Carried as one value, and applied through one function, so the
/// runtime's own policy and a workspace's cannot come to disagree about what
/// the builder was told.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PolicyShaping {
    /// [`RuntimeBuilder::with_command_timeout`].
    command_timeout: Option<std::time::Duration>,
    /// [`RuntimeBuilder::with_tool_result_policy`].
    tool_results: Option<ToolResultPolicy>,
}

impl PolicyShaping {
    pub(super) const fn new(
        command_timeout: Option<std::time::Duration>,
        tool_results: Option<ToolResultPolicy>,
    ) -> Self {
        Self {
            command_timeout,
            tool_results,
        }
    }

    /// One recipe, shaped by what the builder was told.
    pub(crate) fn apply_to(self, policy: RuntimePolicy) -> RuntimePolicy {
        let policy = with_command_patience(policy, self.command_timeout);
        match self.tool_results {
            Some(tool_results) => tool_results.apply_to(policy),
            None => policy,
        }
    }
}

/// Applies a host's chosen command timeout, raising the ceiling to match.
///
/// The ceiling moves with the default because the two mean different things to
/// mentra — one is what a command gets when it asks for nothing, the other is
/// the most it may ask for — and a host setting the first past the second
/// would otherwise be silently clamped back to a number it did not choose.
fn with_command_patience(
    policy: RuntimePolicy,
    timeout: Option<std::time::Duration>,
) -> RuntimePolicy {
    match timeout {
        None => policy,
        Some(timeout) => policy
            .with_default_command_timeout(timeout)
            .with_max_command_timeout(timeout),
    }
}

/// Keeps the parts of `.git` that decide what *runs* out of reach.
///
/// `.git/hooks` holds programs git executes on ordinary operations, and
/// `.git/config` can name more of them (`core.hooksPath`, and the `filter`/
/// `diff` drivers that run on checkout). Writing either turns a file edit into
/// code execution outside anything basis's policy or approval covers, which is
/// why they are singled out rather than denying `.git` wholesale — an agent
/// legitimately reads `.git`, and `git` itself must keep writing objects and
/// refs underneath it.
///
/// **This binds the builtin file tools, not the shell.** A command like
/// `sh -c 'echo … > .git/hooks/pre-commit'` still reaches the path, because
/// nothing here parses shell. It closes the route a model actually takes and
/// remains hygiene; per ADR-0004 and ADR-0013 the boundary is the OS's, and
/// basis does not ship one.
fn git_protected(policy: RuntimePolicy, workspace: &Path) -> RuntimePolicy {
    let git = workspace.join(".git");
    policy
        .with_denied_write_root(git.join("hooks"))
        .with_denied_write_root(git.join("config"))
}
