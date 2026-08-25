//! Approval policy: what a task's recorded spawn request says about
//! consequential calls, and who answers `Prompt` while a process executes it.
//!
//! A task's `approve` mode is durable — recorded in `meta.json` at spawn and
//! read back at every attach, exactly as `--provider` or `--model` are — so
//! it lives beside them here rather than in the CLI that merely parses it off
//! a flag.

use basis::Approver;

/// Every consequential call is put to this, exactly once per task: `Always`
/// allows, `Never` refuses, `Prompt` asks whoever is executing the task's
/// current turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Approve {
    /// Allow consequential calls without asking.
    Always,
    /// Ask whatever [`PromptHost`] the executing process supplied. Refused at
    /// spawn and at attach for a process with none, or with one that cannot
    /// currently ask (see [`validate_approval`]).
    #[default]
    Prompt,
    /// Refuse anything that changes state outside the process.
    Never,
}

/// How a process answers `Approve::Prompt` while it executes a task's turns,
/// and whether there is anyone there to ask at all.
///
/// The executor is whichever process holds a task's attach lock (ADR-0019),
/// and only that process's own environment knows whether a person — or any
/// other approving party — is behind it. A library has no terminal to ask at
/// (ADR-0011, the same reason `basis-cli`'s `TerminalApprover` lives in the
/// binary and not in `basis`), so `basis-tasks` does not decide this; a host
/// that wants `Prompt` to work supplies one via
/// [`Tasks::with_prompt_host`](crate::Tasks::with_prompt_host). A `Tasks`
/// with none refuses `Prompt` the same way an unaskable process always did —
/// safely, and by name.
pub trait PromptHost: Send + Sync {
    /// Whether this process can currently put a question to whoever answers
    /// for it. Checked before a `Prompt`-mode task is allowed to spawn, and
    /// again at every attach.
    fn can_ask(&self) -> bool;

    /// The approver used for one task's turns while this process drives it.
    /// Called once per attach, only when [`can_ask`](Self::can_ask) said yes.
    fn approver(&self) -> Box<dyn Approver>;
}

/// `Prompt` is answerable exactly when a process is driving the task *and*
/// has somewhere to put the question — see [`PromptHost`]. `Always` and
/// `Never` ask nobody, so they need no host at all.
///
/// Public because a host wants this checked as early as possible: refusing
/// `Prompt` at spawn, before a task directory is even minted, is a cheaper
/// and clearer failure than minting one that can never make progress.
pub fn validate_approval(approve: Approve, interactive: bool) -> Result<(), crate::Error> {
    match approve {
        Approve::Always | Approve::Never => Ok(()),
        Approve::Prompt if interactive => Ok(()),
        Approve::Prompt => Err(crate::Error::new(
            "`Approve::Prompt` needs a process driving the task with a prompt host that can \
             currently ask; use `Always` or `Never` for work nobody is attached to",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_approval_needs_a_driver_that_can_ask() {
        for approve in [Approve::Always, Approve::Never] {
            assert!(
                validate_approval(approve, false).is_ok(),
                "{approve:?} asks nobody"
            );
            assert!(
                validate_approval(approve, true).is_ok(),
                "{approve:?} asks nobody"
            );
        }

        assert!(
            validate_approval(Approve::Prompt, true).is_ok(),
            "a host that can ask is exactly what `Prompt` needs"
        );

        let refused = validate_approval(Approve::Prompt, false)
            .expect_err("nobody able to ask means nobody to ask")
            .to_string();
        assert!(refused.contains("ask"), "{refused}");
    }
}
