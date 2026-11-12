//! Holding every request to the workflows it is allowed.
//!
//! The allowed set is not decided here. A request's
//! [`Principal`] carries the workflows its authenticator verified for it —
//! read off a token a gateway signed — and this module turns that set into
//! three things: whether the request is served at all, what a run started
//! for it is built with, and whether a task started earlier may go on.
//!
//! A task is bound to the workflows it was built with, and two requests can
//! show that one of them has been withdrawn: a `GetTask`, and a reply to the
//! task. When the token of either no longer names a workflow the task is
//! bound to, the task is marked as stopped with the reason, its run is
//! cancelled, and from that moment it is reported failed, whatever its
//! backend later says of it. A task that had already settled is left as it
//! settled.
//!
//! Which token a request carries is the gateway's to decide, and the split
//! follows what a gateway that lets a task keep its workflows sends (dial's
//! ADR-0054). The task's own caller reads, watches and cancels it with the
//! set it started with, so those pass; the gateway's own look at a task
//! whose workflows were withdrawn is a `GetTask` carrying only what is still
//! allowed, and a reply carries the same. Nothing else is taken for a
//! withdrawal. `CancelTask` is never judged: it can only stop work.
//! `ListTasks`, a stream and a new message in the task's conversation carry
//! sets decided for themselves — a message may be narrowed to one workflow
//! by its caller — and a narrower set there says nothing about the task.

use std::sync::Arc;

use a2a::A2AError;
use basis_tasks::now_ms;

use super::{A2aService, Backend};
use crate::{
    config::{Principal, Workflows},
    index::{Record, Withdrawn},
    project::internal,
};

/// What a request that may not be served for want of a workflow is told.
fn refused(message: impl Into<String>) -> A2AError {
    A2AError::unsupported_operation(message)
}

impl A2aService {
    pub(crate) fn workflows(&self) -> Option<&Workflows> {
        self.inner.workflows.as_ref()
    }

    /// The workflows `caller` may use here, in card order — never empty — or
    /// why the request is refused. `None` on a server configured with no
    /// workflows, which holds nobody to any.
    pub(crate) fn allowed(&self, caller: &Principal) -> Result<Option<Vec<String>>, A2AError> {
        let Some(workflows) = self.workflows() else {
            return Ok(None);
        };
        let allowed = workflows.offered(granted(caller)?);
        if allowed.is_empty() {
            return Err(refused(format!(
                "None of this agent's workflows is open to this caller. {} can open one.",
                capitalized(workflows.contact())
            )));
        }
        Ok(Some(allowed))
    }

    /// Refuses a request that says nothing of workflows, on a server that
    /// holds its callers to them — whatever it asks about, and whether or
    /// not it names a task.
    pub(crate) fn require_workflows(&self, caller: &Principal) -> Result<(), A2AError> {
        if self.workflows().is_some() {
            granted(caller)?;
        }
        Ok(())
    }

    /// `record`, held to what this request's token allows: stopped, and
    /// marked so, when a workflow its task was built with is not in the
    /// token's set. A task that has settled, or was already stopped, is
    /// returned as it is. Called for a `GetTask` and for a reply, and for
    /// nothing else (see the module's account).
    ///
    /// The task's workflows are looked for in the token's set as it was
    /// signed, not in what this server offers of it: a token carrying the
    /// set a task started with always passes, whatever has changed here
    /// since.
    pub(crate) fn held(&self, caller: &Principal, record: Record) -> Result<Record, A2AError> {
        let Some(workflows) = self.workflows() else {
            return Ok(record);
        };
        let allowed = granted(caller)?;
        if record.withdrawn.is_some() || self.has_settled(&record) {
            return Ok(record);
        }
        let contact = capitalized(workflows.contact());
        // A task with no recorded workflows is not judged here: one accepted
        // before this server held anyone to them is stopped when the server
        // opens (`stop_unbound`), never by a request.
        let reason = record.workflows.as_ref().and_then(|bound| {
            bound.iter().find(|id| !allowed.contains(id)).map(|id| {
                format!(
                    "The workflow `{id}` is no longer open to this caller, so this task was \
                     stopped. {contact} can open it."
                )
            })
        });
        let Some(reason) = reason else {
            return Ok(record);
        };
        let withdrawn = Withdrawn {
            reason,
            at_ms: now_ms(),
        };
        let stopped = self
            .inner
            .index
            .withdraw(&record.id, withdrawn)
            .map_err(internal)?
            .unwrap_or(record);
        self.stop(&stopped, false);
        Ok(stopped)
    }

    /// Stops a task this server accepted before it held anyone to
    /// workflows, as the server opens: its run was built with every tool
    /// the workspace has, and nothing says it stayed inside what its caller
    /// is allowed. Returns the record to go on with — marked, and its run
    /// asked to stop before anything drives it again — or `None` when the
    /// stop could not be recorded, in which case the task is not driven.
    ///
    /// Done here and not on a request, so that no request from a task's own
    /// caller is ever what stops it.
    pub(super) fn stop_unbound(&self, record: Record) -> Option<Record> {
        if self.workflows().is_none()
            || record.workflows.is_some()
            || record.withdrawn.is_some()
            || record.finished_ms.is_some()
        {
            return Some(record);
        }
        let withdrawn = Withdrawn {
            reason: "This task was started before this agent held its callers to workflows, so \
                     it was stopped. Send the request again."
                .to_string(),
            at_ms: now_ms(),
        };
        match self.inner.index.withdraw(&record.id, withdrawn) {
            Ok(Some(stopped)) => {
                self.stop(&stopped, true);
                Some(stopped)
            }
            Ok(None) => Some(record),
            Err(error) => {
                eprintln!(
                    "basis-a2a: task {} predates this server's workflows and could not be \
                     marked stopped, so it is not driven: {error}",
                    record.id
                );
                None
            }
        }
    }

    /// Whether `record`'s task has settled, as far as can be read without
    /// waiting: this server saw it settle, or its `basis-tasks` terminal is
    /// on disk.
    fn has_settled(&self, record: &Record) -> bool {
        if record.finished_ms.is_some() {
            return true;
        }
        match self.backend() {
            Backend::Tasks(backend) => record
                .handle()
                .is_some_and(|handle| matches!(backend.tasks.terminal(handle), Ok(Some(_)))),
            Backend::Hosted(_) => false,
        }
    }

    /// Asks `record`'s run to stop, without waiting for it to: the task is
    /// already reported failed, and the run ends at its next boundary.
    /// `before_driving` asks a `basis-tasks` run on this thread, for a task
    /// about to be driven again: the request must be on disk before its
    /// driver attaches.
    fn stop(&self, record: &Record, before_driving: bool) {
        let id = record.id.clone();
        match self.backend() {
            Backend::Tasks(backend) => {
                if let Some(handle) = record.handle().cloned() {
                    let tasks = backend.tasks.clone();
                    let cancel = move || {
                        if let Err(error) = tasks.cancel(&handle, None) {
                            eprintln!("basis-a2a: stop task {id}: {error}");
                        }
                    };
                    match tokio::runtime::Handle::try_current() {
                        Ok(runtime) if !before_driving => drop(runtime.spawn_blocking(cancel)),
                        _ => cancel(),
                    }
                }
                // A turn parked on a question would never reach the boundary
                // the cancel lands at.
                if let Some(questions) = self.questions(&record.id) {
                    questions.cancel();
                }
            }
            Backend::Hosted(host) => {
                let (Some(host_id), Ok(runtime)) = (
                    record.host_id().map(str::to_string),
                    tokio::runtime::Handle::try_current(),
                ) else {
                    eprintln!("basis-a2a: task {id} was withdrawn but could not be stopped here");
                    return;
                };
                let host = Arc::clone(host);
                runtime.spawn(async move {
                    if let Err(error) = host.cancel(&host_id).await {
                        eprintln!("basis-a2a: stop hosted task {id}: {error}");
                    }
                });
            }
        }
    }
}

/// The workflows `caller`'s authenticator verified for this request. One
/// that said nothing did not come through whatever decides them.
fn granted(caller: &Principal) -> Result<&[String], A2AError> {
    caller.workflows().ok_or_else(|| {
        refused(
            "This agent runs only the workflows a request is allowed, and this request came \
             without any. Call it through the gateway it is registered with.",
        )
    })
}

/// `text` with its first letter capitalized, to open a sentence with.
fn capitalized(text: &str) -> String {
    let mut letters = text.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}
