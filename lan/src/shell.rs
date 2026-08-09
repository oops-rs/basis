//! Whether a run may execute commands.
//!
//! Per [ADR-0006], command execution is denied unless something outside the
//! process is confining it, and lan never infers that — the grant is always an
//! explicit act by whoever knows the boundary holds.
//!
//! [ADR-0006]: https://github.com/oops-rs/lan/blob/main/docs/adr/0006-shell-requires-an-explicit-grant.md

use mentra::runtime::{ExecutionEnvironment, detect_environment};

/// Environment variable granting command execution.
pub const ALLOW_SHELL_VAR: &str = "LAN_ALLOW_SHELL";

/// Whether the agent may run commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellAccess {
    /// No command execution.
    ///
    /// The default. An in-process path check cannot confine a process once it
    /// is running, so granting this by default would be claiming a boundary
    /// lan does not have.
    #[default]
    Denied,
    /// Commands allowed.
    ///
    /// Only sound when something outside the process enforces the workspace
    /// boundary — a container with the workspace as its sole writable mount,
    /// or a per-command sandbox.
    Granted,
}

impl ShellAccess {
    pub fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }

    /// Reads the grant from the environment.
    ///
    /// Any value except `0`, `false`, or empty counts as a grant: someone who
    /// set the variable meant to set it.
    pub fn from_env() -> Self {
        match std::env::var(ALLOW_SHELL_VAR) {
            Ok(value) => Self::from_flag(!matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no"
            )),
            Err(_) => Self::Denied,
        }
    }

    pub fn from_flag(granted: bool) -> Self {
        if granted { Self::Granted } else { Self::Denied }
    }
}

/// A warning to emit when commands were granted with nothing enforcing the
/// boundary, or `None` when the grant is unremarkable.
///
/// Detection informs; it never decides. Being inside a container does not
/// prove the container was run with a constrained mount set, so this cannot
/// be used to grant — only to point out the most obviously unconfined case.
pub fn unconfined_warning(access: ShellAccess) -> Option<String> {
    if !access.is_granted() {
        return None;
    }

    match detect_environment() {
        ExecutionEnvironment::Host => Some(
            "commands are enabled on the host: nothing outside this process is \
             confining them, so a command can reach anything your user account \
             can. Run inside the container image if that is not what you want."
                .to_string(),
        ),
        // Inside a container or CI the operator chose the mounts and the blast
        // radius; lan cannot check them and does not pretend to.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_is_the_default() {
        assert_eq!(ShellAccess::default(), ShellAccess::Denied);
        assert!(!ShellAccess::default().is_granted());
    }

    #[test]
    fn a_flag_maps_both_ways() {
        assert_eq!(ShellAccess::from_flag(true), ShellAccess::Granted);
        assert_eq!(ShellAccess::from_flag(false), ShellAccess::Denied);
    }

    #[test]
    fn denial_never_warns() {
        assert_eq!(unconfined_warning(ShellAccess::Denied), None);
    }

    #[test]
    fn a_grant_on_the_host_is_called_out() {
        // The suite runs on a host or in CI; only assert the host branch when
        // that is actually where we are, so the test says something true
        // either way.
        let warning = unconfined_warning(ShellAccess::Granted);
        match detect_environment() {
            ExecutionEnvironment::Host => {
                let warning = warning.expect("an unconfined grant must be called out");
                assert!(warning.contains("nothing outside this process"));
                assert!(
                    !warning.contains(ALLOW_SHELL_VAR),
                    "the grant can come from a flag or the variable, so the \
                     warning must not assume one of them"
                );
            }
            _ => assert_eq!(
                warning, None,
                "only the plainly unconfined case is worth a warning"
            ),
        }
    }
}
