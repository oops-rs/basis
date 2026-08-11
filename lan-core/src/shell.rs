//! Whether a run may execute commands.
//!
//! Per [ADR-0013] the host owns the boundary. Commands are enabled by default:
//! a harness that cannot run `cargo test` does very little real work, and the
//! flag that used to gate them was theater once a process spawned — an
//! in-process path check cannot confine a command that is already running.
//!
//! So lan claims nothing. The process holds whatever authority the user who
//! started it holds, and confinement, where it is wanted, comes from the OS —
//! `docs/containerization.md` has the patterns. Denying is one line for a run
//! that is meant to read and report.
//!
//! [ADR-0013]: https://github.com/oops-rs/lan/blob/main/docs/adr/0013-the-host-owns-the-boundary.md

/// Whether the agent may run commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellAccess {
    /// No command execution.
    ///
    /// For a run that inspects a workspace and reports on it. Worth being
    /// precise about what this is: one route closed, not a boundary. The file
    /// tools still write, and the process still runs as its user — see
    /// [`ApprovalPolicy::Never`](crate::ApprovalPolicy::Never) for a run that
    /// changes nothing at all.
    Denied,
    /// Commands allowed.
    ///
    /// The default. A command reaches whatever the user account running lan
    /// can reach, which is the honest description of a process on a host and
    /// is why lan neither warns about it per run nor pretends otherwise.
    #[default]
    Granted,
}

impl ShellAccess {
    pub fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }

    /// Maps a yes-or-no answer onto the enum, for a caller holding a bool —
    /// a CLI flag, a config field, an environment variable of its own.
    pub fn from_flag(granted: bool) -> Self {
        if granted { Self::Granted } else { Self::Denied }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_the_default() {
        // ADR-0013: the first `lan "run the tests"` has to work, and a default
        // of denied made the tool's purpose opt-in.
        assert_eq!(ShellAccess::default(), ShellAccess::Granted);
        assert!(ShellAccess::default().is_granted());
    }

    #[test]
    fn a_flag_maps_both_ways() {
        assert_eq!(ShellAccess::from_flag(true), ShellAccess::Granted);
        assert_eq!(ShellAccess::from_flag(false), ShellAccess::Denied);
    }
}
