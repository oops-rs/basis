//! This binary's [`basis_tasks::PromptHost`]: a terminal, or nothing.
//!
//! `basis-tasks` has no terminal to ask at (ADR-0011) — the same reason
//! [`TerminalApprover`](crate::approver::TerminalApprover) lives here and not
//! in `basis`. This is the seam that lends it one: whichever process holds a
//! task's attach lock answers `Approve::Prompt` through its own stdin, exactly
//! as the pre-extraction daemon-less CLI did.

use std::io::IsTerminal;

use basis_tasks::PromptHost;

use crate::approver::TerminalApprover;

#[derive(Debug, Default)]
pub(crate) struct CliPromptHost;

impl PromptHost for CliPromptHost {
    fn can_ask(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn approver(&self) -> Box<dyn basis::Approver> {
        Box::new(TerminalApprover::new())
    }
}
