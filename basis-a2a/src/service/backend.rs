//! Who runs a served task: a workspace's `basis-tasks` (the default), or a
//! host through [`TaskHost`] (ADR-0033). Everything specific to one of the
//! two lives in its arm; the rest of the service is shared.

use std::sync::Arc;

use a2a::A2AError;
use basis_tasks::Tasks;

use crate::{
    config::{InboundApproval, RunDefaults},
    host::{HostError, TaskHost},
    index::Record,
    project::internal,
};

/// The backend one service serves. One per service, so the larger arm is
/// boxed rather than every `Backend` sized to it.
pub(crate) enum Backend {
    Tasks(Box<TasksBackend>),
    Hosted(Arc<dyn TaskHost>),
}

/// The workspace's `basis-tasks`, and what every task spawned there is
/// given.
pub(crate) struct TasksBackend {
    pub(crate) tasks: Tasks,
    /// The workspace the tasks run in, where the files a caller sends are
    /// saved (ADR-0041 §6).
    pub(crate) workspace: std::path::PathBuf,
    pub(crate) approval: InboundApproval,
    pub(crate) shell: bool,
    pub(crate) run: RunDefaults,
}

impl Backend {
    /// Whether consequential calls are put to the caller — a `basis-tasks`
    /// mechanism (ADR-0033 Context 4).
    pub(crate) fn asks_caller(&self) -> bool {
        match self {
            Self::Tasks(backend) => matches!(backend.approval, InboundApproval::AskCaller { .. }),
            Self::Hosted(_) => false,
        }
    }
}

/// The host's id for the task `record` names. A record of a `basis-tasks`
/// task is one a hosted server never wrote, and is this server's fault to
/// report, not the caller's.
pub(crate) fn host_id_of(record: &Record) -> Result<&str, A2AError> {
    record
        .host_id()
        .ok_or_else(|| internal(format!("task {} is not a hosted task", record.id)))
}

/// What a caller is told when its context is still working on an earlier
/// message — from either backend, in the same words.
pub(crate) fn still_working() -> A2AError {
    A2AError::unsupported_operation(
        "a task in this context is still working; wait for it to settle, then send again",
    )
}

/// A host's refusal, as the caller is told it (ADR-0033 §1).
pub(crate) fn from_host(error: HostError) -> A2AError {
    match error {
        HostError::Busy(_) => still_working(),
        HostError::Refused(message) => A2AError::invalid_params(message),
        HostError::Internal(message) => internal(format!("the task host failed: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use a2a::error_code;

    use super::*;

    #[test]
    fn a_hosts_refusals_say_whose_fault_they_are() {
        let busy = from_host(HostError::Busy("run 7 is working".into()));
        assert_eq!(busy.code, still_working().code);
        assert_eq!(busy.message, still_working().message);
        let refused = from_host(HostError::Refused("too long".into()));
        assert_eq!(refused.code, error_code::INVALID_PARAMS);
        assert_eq!(refused.message, "too long");
        let failed = from_host(HostError::Internal("db at /var/lib/x is down".into()));
        assert_eq!(failed.code, error_code::INTERNAL_ERROR);
        assert!(!failed.message.contains("/var/lib"), "{}", failed.message);
    }
}
