//! Where and how this runtime runs commands — the execution half of
//! [`RuntimeBuilder`](super::RuntimeBuilder).
//!
//! The knobs: how patiently a command is waited out, the fixed environment
//! every spawned process receives, and the named executors `!@<target>`
//! routes to (ADR-0021). And the derivations `build` reaches for from them:
//! the name validation a routable target has to pass, and the two
//! `RuntimePolicy` recipes — one shared across workspaces, one bound to a
//! single one — that say what a command is allowed to do.
//!
//! Grouped because they answer one question between them and are read
//! together: a target name is validated against the same rule the `!@`
//! parser applies, and the policies are where a command timeout and a shell
//! posture actually land.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mentra::RuntimePolicy;

use crate::{
    error::RunError,
    shell::ShellAccess,
    tools::spawn::{LOCAL_TARGET, is_target_name},
};

use super::{CommandTargets, RuntimeBuilder, RuntimeExecutor};

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

    /// Registers an executor this runtime's commands can be routed to by name.
    ///
    /// ADR-0021. `spawn` is still the model's one door, and *where a command
    /// runs* is a dimension of a call through it rather than a second tool:
    /// `!@<name> <command>` reaches the executor registered here under `name`,
    /// and a command with no `@` reaches the local one exactly as before. The
    /// case this exists for is basis running inside a Linux container on a
    /// macOS build machine, where `cargo test` belongs in the container and
    /// `xcodebuild` does not exist there at all.
    ///
    /// **basis ships no executors and claims nothing about what one reaches.**
    /// The host writes it — SSH to a forced command, `docker exec`, an agent
    /// on a build box — and a target is exactly as trusted as that code.
    /// `docs/targets.md` has the worked pattern, what the executor receives,
    /// and the honesty this cannot be written without: routing a command
    /// elsewhere is not confinement, and nothing here may be described as a
    /// sandbox (ADR-0013).
    ///
    /// What the executor is handed is a `CommandRequest` with this runtime's
    /// fixed command environment already merged, a timeout mentra has already
    /// clamped, and the `target` name still on it, so one executor registered
    /// under two names can tell which it was called as. The `cwd` is
    /// **advisory**: it is a path in *this* process's filesystem, and what it
    /// means on the far side is the executor's to decide.
    ///
    /// The trait and everything an implementation of it names are re-exported
    /// as [`crate::runtime`]'s executor types, so a host writes one against
    /// `basis` alone and never adds mentra to its own manifest.
    ///
    /// A later call with the same name replaces the earlier one, the same rule
    /// [`with_command_environment`](Self::with_command_environment) follows.
    /// Names are `[A-Za-z0-9_-]+` and may not be `local`, which is the wire
    /// word for *here*; a name that breaks either rule is a
    /// [`RunError::CommandTarget`] from [`build`](Self::build) rather than a
    /// panic here, because a host reading its targets out of its own
    /// configuration should be able to report a bad one the way it reports
    /// every other bad setting.
    ///
    /// Runtime-scoped, for ADR-0018's reason and one of its own: a target that
    /// changed per repository would be a different machine per repository,
    /// which is not a thing a repository knows.
    pub fn with_command_target(
        mut self,
        name: impl Into<String>,
        executor: impl RuntimeExecutor + 'static,
    ) -> Self {
        self.command_targets.insert(name.into(), Arc::new(executor));
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

/// The policy a private runtime bakes for one workspace:
/// `git_protected(workspace_bounded(path))`, the caller's shell posture as a
/// second belt beside the dispatcher's guard, and the memory roots.
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
/// to read. The shared policy deliberately gets none of this — it is fixed
/// before any workspace exists and a per-workspace root added there could not
/// be unsaid — so on a shared runtime the index renders and these writes are
/// refused, a recorded cost of sharing beside the others.
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

/// The command posture a shared runtime grants: shell and background on, with
/// `workspace_bounded`'s timeouts, and no path roots of its own.
///
/// Commands are on because ADR-0013 grants them by default and a shared policy
/// cannot say otherwise per workspace — the dispatcher's guard is where a
/// `ShellAccess::Denied` workspace is enforced. No roots, because mentra's
/// file bounding always allows under the calling agent's `base_dir`: with the
/// list empty, each workspace's agents are confined to their own directory and
/// no workspace's root widens another's.
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

/// Applies a host's chosen command timeout, raising the ceiling to match.
///
/// The ceiling moves with the default because the two mean different things to
/// mentra — one is what a command gets when it asks for nothing, the other is
/// the most it may ask for — and a host setting the first past the second
/// would otherwise be silently clamped back to a number it did not choose.
pub(super) fn with_command_patience(
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
/// basis does not ship one. On shared runtimes the same rule is enforced by the
/// hook dispatcher, which knows which workspace a call belongs to; the private
/// path keeps this policy baking as a second belt.
fn git_protected(policy: RuntimePolicy, workspace: &Path) -> RuntimePolicy {
    let git = workspace.join(".git");
    policy
        .with_denied_write_root(git.join("hooks"))
        .with_denied_write_root(git.join("config"))
}
