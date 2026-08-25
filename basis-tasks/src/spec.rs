//! `RunSpec`: one task's worth of spawn request.
//!
//! Built the way `basis::RunSpec` is — `new` plus `with_*` methods that
//! return new values — because this is the same kind of thing one layer up:
//! `basis::RunSpec` is a run's per-turn intent against an already-open
//! [`basis::Workspace`]; this is what a *durable* task additionally records
//! so a later attach, in a process that may not be this one, can open that
//! workspace itself and mint the run.

use std::time::Duration;

use basis::{Effort, SystemPrompt};

use crate::{approve::Approve, handle::TaskHandle};

/// The default deadline an unattended task is given when nothing else names
/// one: 30 minutes. A spawned task may never be waited on by an attentive
/// caller, so — unlike an attended one-shot, which is unbounded unless asked
/// — it always gets a finite service bound (`with_deadline` narrows it).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Which conversation a spawned task picks up, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Continuation {
    /// Opens a new conversation. The ordinary case.
    #[default]
    New,
    /// The conversation this workspace was last worked in — what a bare
    /// `--continue` resolves against.
    Latest,
    /// A specific conversation, by the task that opened or last continued
    /// it — what `--continue --session <ID>` resolves against.
    Named(TaskHandle),
}

/// One task's worth of spawn request: `basis::RunSpec`'s per-run intent,
/// plus the workspace-level overrides a [`basis::Workspace`] normally fixes
/// once, and the facts a task that may run unattended, in another process,
/// additionally needs recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    pub(crate) prompt: String,
    pub(crate) provider: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) shell: bool,
    pub(crate) system_prompt: Option<SystemPrompt>,
    pub(crate) effort: Option<Effort>,
    pub(crate) approve: Approve,
    pub(crate) deadline: Option<Duration>,
    pub(crate) tool_budget: Option<usize>,
    pub(crate) token_budget: Option<u64>,
    pub(crate) detached: bool,
    pub(crate) continuation: Continuation,
}

impl RunSpec {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            provider: None,
            base_url: None,
            model: None,
            shell: true,
            system_prompt: None,
            effort: None,
            approve: Approve::default(),
            deadline: None,
            tool_budget: None,
            token_budget: None,
            detached: false,
            continuation: Continuation::default(),
        }
    }

    pub fn with_provider(self, provider: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            ..self
        }
    }

    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            ..self
        }
    }

    pub fn with_model(self, model: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
            ..self
        }
    }

    /// Refuses the task shell access — sugar over the workspace's own
    /// `ShellAccess::from_flag`.
    pub fn without_shell(self) -> Self {
        Self {
            shell: false,
            ..self
        }
    }

    pub fn with_system_prompt(self, system_prompt: SystemPrompt) -> Self {
        Self {
            system_prompt: Some(system_prompt),
            ..self
        }
    }

    pub fn with_effort(self, effort: Effort) -> Self {
        Self {
            effort: Some(effort),
            ..self
        }
    }

    /// Every consequential call this task's turns make is put to this.
    pub fn with_approve(self, approve: Approve) -> Self {
        Self { approve, ..self }
    }

    /// Gives up on the task after `deadline`, counted from spawn. Defaults to
    /// [`DEFAULT_DEADLINE`] when never set — an unattended task always gets a
    /// finite service bound.
    pub fn with_deadline(self, deadline: Duration) -> Self {
        Self {
            deadline: Some(deadline),
            ..self
        }
    }

    pub fn with_tool_budget(self, tool_budget: usize) -> Self {
        Self {
            tool_budget: Some(tool_budget),
            ..self
        }
    }

    pub fn with_token_budget(self, token_budget: u64) -> Self {
        Self {
            token_budget: Some(token_budget),
            ..self
        }
    }

    /// Spawns outside the calling task's ownership tree even when this
    /// process is itself executing one — see [`crate::current_task`]. A
    /// detached task inherits no scope: nothing cancels it downward, and
    /// nothing waits for it before its would-be parent settles.
    pub fn detached(self) -> Self {
        Self {
            detached: true,
            ..self
        }
    }

    /// Picks up an existing conversation instead of opening a new one.
    pub fn continuing(self, continuation: Continuation) -> Self {
        Self {
            continuation,
            ..self
        }
    }
}
