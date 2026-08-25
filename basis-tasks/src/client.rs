//! `Tasks`: the crate's front door.
//!
//! One workspace's durable tasks, over one global [`DataDir`]. `spawn` mints
//! a task and returns immediately — the mint is a `mkdir` and an atomic
//! write, nothing durable ever blocks on a model. Everything that *drives* a
//! task (`ask`, `wait`, `wait_message`) attaches only when no other process
//! already holds it, and every other verb (`send`, `cancel`, `watch`,
//! `list`, `terminal`, ...) only ever reads or writes small files under the
//! attach lock's protection, never taking it. No verb here leaves a resident
//! process behind (ADR-0019).
//!
//! `workspace` matters to exactly two of these: [`spawn`](Tasks::spawn),
//! which mints under it, and [`list`](Tasks::list), which scans it. Every
//! other method takes a [`TaskHandle`] that already names its own workspace
//! key, and resolves purely from that — a host holding a handle can `wait`
//! or `cancel` it regardless of which workspace `Tasks::open` was given.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::Value;

use crate::{
    Error,
    approve::PromptHost,
    attach,
    data_dir::{self, DataDir, canonical_workspace},
    events::EventTail,
    handle::TaskHandle,
    inbox,
    live::{DriveContext, LiveSink},
    lock, policy,
    spec::{Continuation, DEFAULT_DEADLINE, RunSpec},
    state::{self, RunOptions, TaskMeta},
    tasks::{self, TaskSummary},
    watch::EventCursor,
};

/// The current attach's outcome, or a bounded timeout — see
/// [`attach::WaitOutcome`], which this re-exports as the return type of
/// [`Tasks::wait`] and [`Tasks::wait_message`].
pub use attach::WaitOutcome;

/// What [`Tasks::ask`] enqueued and what came of waiting for it.
///
/// `message_id` is the durable correlation handle — the same id [`Tasks::send`]
/// would have returned for the enqueue alone — carried back so a caller can
/// retry through [`Tasks::wait_message`] if the wait itself times out, or
/// report it in an "accepted" record either way.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub message_id: String,
    pub outcome: WaitOutcome,
}

/// One workspace's durable tasks.
pub struct Tasks {
    data: DataDir,
    workspace: PathBuf,
    prompt_host: Option<Arc<dyn PromptHost>>,
}

impl Tasks {
    /// Opens the durable task store: `BASIS_DATA_DIR`, else an absolute
    /// `XDG_DATA_HOME`, else the platform data home (created private on first
    /// use). `workspace` is read by [`spawn`](Self::spawn) and
    /// [`list`](Self::list) only; every handle-scoped method resolves
    /// straight from the handle, so opening `Tasks` is cheap and does not
    /// itself touch `workspace` on disk.
    pub fn open(workspace: impl Into<PathBuf>) -> Result<Self, Error> {
        let data = DataDir::discover()
            .map_err(|error| Error::new(format!("open task data directory: {error}")))?;
        Ok(Self {
            data,
            workspace: workspace.into(),
            prompt_host: None,
        })
    }

    /// Opens the durable task store at an explicit root, bypassing the
    /// `BASIS_DATA_DIR`/XDG discovery [`open`](Self::open) performs.
    ///
    /// For a host that manages its own data directory location rather than
    /// the process environment — several `Tasks` in one process, each with
    /// its own root, for one — and for a test that wants no dependency on
    /// `std::env` at all.
    pub fn open_at(
        data_dir: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
    ) -> Result<Self, Error> {
        let data = DataDir::from_path(data_dir.into())
            .map_err(|error| Error::new(format!("open task data directory: {error}")))?;
        Ok(Self {
            data,
            workspace: workspace.into(),
            prompt_host: None,
        })
    }

    /// Supplies how this `Tasks` answers `Approve::Prompt` while it drives a
    /// task, and whether it can be asked at all — see [`PromptHost`]. Without
    /// one, `Prompt` refuses the same way an unaskable process always did;
    /// `basis-cli` is the first caller of this.
    #[must_use]
    pub fn with_prompt_host(self, prompt_host: Arc<dyn PromptHost>) -> Self {
        Self {
            prompt_host: Some(prompt_host),
            ..self
        }
    }

    /// Whether this `Tasks` can currently put an `Approve::Prompt` question
    /// to whoever answers for it — `false` with no [`PromptHost`] supplied.
    pub fn can_ask(&self) -> bool {
        self.prompt_host.as_deref().is_some_and(PromptHost::can_ask)
    }

    /// The mentra store directory `workspace` resolves to, without minting or
    /// reading any task.
    ///
    /// What a host's attended, one-shot route — no durable task, no handle —
    /// needs to land its conversation store on the same directory
    /// [`spawn`](Self::spawn) would use for the same workspace, so the two
    /// share one conversation history and one memory root rather than
    /// falling back to two.
    pub fn store_dir(workspace: &Path) -> Result<PathBuf, Error> {
        let data = DataDir::discover()
            .map_err(|error| Error::new(format!("open task data directory: {error}")))?;
        data.resolve_store_dir(workspace).map_err(Error::new)
    }

    fn ctx(&self, live: Option<Arc<dyn LiveSink>>) -> DriveContext {
        DriveContext::new(live, self.prompt_host.clone())
    }

    /// Mints a task and returns its handle immediately. Durable and resumable
    /// from the moment this returns: nothing has attached yet, and nothing
    /// has to for the handle to be good.
    pub fn spawn(&self, spec: RunSpec) -> Result<TaskHandle, Error> {
        let RunSpec {
            prompt,
            provider,
            base_url,
            model,
            shell,
            system_prompt,
            effort,
            approve,
            deadline,
            tool_budget,
            token_budget,
            detached,
            continuation,
        } = spec;
        if prompt.trim().is_empty() {
            return Err(Error::new("prompt is empty"));
        }
        if prompt.len() > state::MAX_PROMPT {
            return Err(Error::new(format!(
                "prompt is {} bytes; the limit is {}",
                prompt.len(),
                state::MAX_PROMPT
            )));
        }
        // Refused here, before a task directory is even minted: a task whose
        // approval mode can never be honored by this `Tasks` — no
        // `PromptHost`, or one that cannot currently ask — is refused as a
        // cheaper, clearer failure than one that could never make progress.
        crate::approve::validate_approval(approve, self.can_ask())?;

        let deadline_ms = duration_ms(deadline.unwrap_or(DEFAULT_DEADLINE));
        let options = RunOptions {
            provider,
            base_url,
            model,
            no_shell: !shell,
            system_prompt,
            append_system_prompt: None,
            effort,
            approve,
            deadline_ms: Some(deadline_ms),
            tool_budget,
            token_budget,
        };

        let canonical = canonical_workspace(&self.workspace).map_err(|error| {
            Error::new(format!(
                "resolve workspace {}: {error}",
                self.workspace.display()
            ))
        })?;
        let key = self.data.ensure_workspace(&canonical).map_err(Error::new)?;

        // Read off the published BASIS_TASK_ID protocol: a task spawning
        // another is, by default, that task's parent (ADR-0017's tree).
        let caller = crate::current_task();
        if let Some(caller) = &caller
            && !detached
            && caller.key() != key
        {
            return Err(Error::new(format!(
                "current task {caller} belongs to another workspace; spawn a detached task to \
                 start work here"
            )));
        }
        let parent = if detached { None } else { caller };

        let continues = self.resolve_continuation(&canonical, &key, continuation)?;

        let requested_deadline = Some(
            state::now_ms()
                .checked_add(deadline_ms)
                .ok_or_else(|| Error::new("task deadline exceeds the system clock range"))?,
        );
        let deadline_at = match &parent {
            Some(parent_handle) => {
                let paths = attach::resolve(&self.data, parent_handle.as_str()).map_err(|_| {
                    Error::new(format!("parent task {parent_handle} does not exist"))
                })?;
                let owner = state::load_meta(&paths).map_err(Error::new)?;
                let accepts = state::read_terminal(&paths).map_err(Error::new)?.is_none()
                    && owner.pending_terminal.is_none()
                    && !state::cancel_requested(&paths)
                    && !owner.deadline_passed();
                if !accepts {
                    return Err(Error::new(format!(
                        "parent task {parent_handle} is no longer running"
                    )));
                }
                attach::earlier_deadline(requested_deadline, owner.deadline_at_ms)
            }
            None => requested_deadline,
        };

        let agents = self.data.agents_dir(&key);
        let existing = std::fs::read_dir(&agents)
            .map_err(|error| Error::new(format!("scan workspace agents: {error}")))?
            .count();
        if existing >= state::MAX_TASKS {
            return Err(Error::new(format!(
                "workspace has {} tasks (the limit); archive old agent directories under {}",
                state::MAX_TASKS,
                agents.display()
            )));
        }

        // `create_dir` is the atomic claim on the handle; a uuid collision retries.
        let (task, paths) = loop {
            let task = format!("{key}/{}", uuid::Uuid::new_v4().simple());
            let paths = self
                .data
                .agent_dir(&task)
                .expect("a minted handle is well-formed");
            match std::fs::create_dir(paths.dir()) {
                Ok(()) => {
                    data_dir::restrict_directory(paths.dir())
                        .map_err(|error| Error::new(format!("restrict task directory: {error}")))?;
                    break (task, paths);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(Error::new(format!("create task directory: {error}"))),
            }
        };
        let meta = TaskMeta::new(
            task.clone(),
            parent.map(|handle| handle.as_str().to_string()),
            detached,
            canonical.to_string_lossy().into_owned(),
            prompt,
            options,
            deadline_at,
        )
        .continuing(continues);
        state::save_meta(&paths, &meta).map_err(Error::new)?;

        TaskHandle::parse(task)
    }

    /// The conversation `continuation` names, when it names one: `New`
    /// mints, `Latest` picks the conversation this workspace was last worked
    /// in, `Named` picks a specific one — the same resolution `--continue`
    /// and `--continue --session <ID>` need.
    fn resolve_continuation(
        &self,
        workspace: &Path,
        key: &str,
        continuation: Continuation,
    ) -> Result<Option<String>, Error> {
        let requested = match continuation {
            Continuation::New => return Ok(None),
            Continuation::Latest => None,
            Continuation::Named(handle) => Some(handle),
        };
        let summaries = tasks::workspace_tasks(&self.data, workspace)
            .map_err(Error::new)?
            .unwrap_or_default();
        let chosen = match &requested {
            // `named` tells a bad argument (malformed, or another workspace's
            // handle) apart from a state fact (well-formed, this workspace,
            // simply not recorded) — the same distinction `Error` carries.
            Some(handle) => {
                tasks::named(&summaries, key, handle.as_str()).map_err(|error| match error {
                    tasks::NamedError::InvalidReference(message) => {
                        Error::invalid_reference(message)
                    }
                    tasks::NamedError::NotFound(message) => Error::new(message),
                })?
            }
            None => tasks::latest_conversation(&summaries).ok_or_else(|| {
                Error::new("no task in this workspace has a conversation to continue")
            })?,
        };
        if chosen.state == "running" {
            return Err(Error::new(format!(
                "task {} is running; its attach lock is what keeps one conversation to one \
                 executor",
                chosen.task
            )));
        }
        if chosen.agent_id.is_empty() {
            return Err(Error::new(format!(
                "task {} has no conversation yet: nothing has attached to it",
                chosen.task
            )));
        }
        Ok(Some(chosen.agent_id.clone()))
    }

    /// Enqueues a follow-up turn. Never blocks and never drives: the message
    /// is durable the moment this returns, and progress happens only once
    /// something attaches — `ask`, `wait`, or any other attacher.
    pub fn send(&self, handle: &TaskHandle, message: impl Into<String>) -> Result<String, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        inbox::enqueue(&paths, handle.as_str(), message.into()).map_err(Error::new)
    }

    /// Enqueues a follow-up turn and awaits its correlated reply, attaching
    /// to drive the task whenever the attach lock is free. `send` plus
    /// [`wait_message`](Self::wait_message), as one call — the edge is
    /// validated before the enqueue, so a rejected wait cannot leave a
    /// message behind.
    pub async fn ask(
        &self,
        handle: &TaskHandle,
        caller: Option<&TaskHandle>,
        message: impl Into<String>,
        timeout: Duration,
    ) -> Result<Reply, Error> {
        policy::validate_wait_edge(&self.data, caller.map(TaskHandle::as_str), handle.as_str())
            .map_err(Error::new)?;
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        let message_id =
            inbox::enqueue(&paths, handle.as_str(), message.into()).map_err(Error::new)?;
        let outcome = attach::wait_for_message(
            &self.data,
            handle.as_str(),
            &message_id,
            timeout,
            self.prompt_host.clone(),
        )
        .await
        .map_err(Error::new)?;
        Ok(Reply {
            message_id,
            outcome,
        })
    }

    /// Awaits one already-enqueued message's correlated reply — the `wait
    /// --message` shape: repeatable without any policy check once the reply
    /// exists, and edge-validated only for the wait that has to actually
    /// happen.
    pub async fn wait_message(
        &self,
        handle: &TaskHandle,
        caller: Option<&TaskHandle>,
        message_id: &str,
        timeout: Duration,
    ) -> Result<WaitOutcome, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        let messages = inbox::load(&paths).map_err(Error::new)?;
        let terminal = state::read_terminal(&paths).map_err(Error::new)?;
        if let Some(payload) = inbox::message_payload_for_dispatch(
            handle.as_str(),
            &messages,
            message_id,
            terminal.as_ref(),
        )
        .map_err(Error::new)?
        {
            return Ok(WaitOutcome::Terminal(payload));
        }
        policy::validate_wait_edge(&self.data, caller.map(TaskHandle::as_str), handle.as_str())
            .map_err(Error::new)?;
        attach::wait_for_message(
            &self.data,
            handle.as_str(),
            message_id,
            timeout,
            self.prompt_host.clone(),
        )
        .await
        .map_err(Error::new)
    }

    /// Awaits the task's terminal record, attaching to drive it whenever the
    /// attach lock is free — repeatable: a settled task's record is read
    /// straight off disk, never rerun. `live`, when given, is shown every
    /// event while (and only while) this call is the one driving.
    pub async fn wait(
        &self,
        handle: &TaskHandle,
        caller: Option<&TaskHandle>,
        timeout: Duration,
        live: Option<Arc<dyn LiveSink>>,
    ) -> Result<WaitOutcome, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        if let Some(terminal) = state::read_terminal(&paths).map_err(Error::new)? {
            return Ok(WaitOutcome::Terminal(terminal));
        }
        policy::validate_wait_edge(&self.data, caller.map(TaskHandle::as_str), handle.as_str())
            .map_err(Error::new)?;
        let ctx = self.ctx(live);
        attach::wait_for_terminal(&self.data, handle.as_str(), timeout, &ctx)
            .await
            .map_err(Error::new)
    }

    /// Whether `caller` (or nobody, for a host outside any task) may
    /// [`wait`](Self::wait) or [`ask`](Self::ask) `target` — the ownership
    /// rule ADR-0017 states: a descendant or an independent root is safe, an
    /// ancestor or a peer is not (send it instead, and read the reply from
    /// [`inbox`](Self::inbox)). `Err` names which rule the edge breaks; a
    /// caller that only wants the yes/no can ask for `.is_ok()`.
    pub fn validate_wait_edge(
        &self,
        caller: Option<&TaskHandle>,
        target: &TaskHandle,
    ) -> Result<(), Error> {
        policy::validate_wait_edge(&self.data, caller.map(TaskHandle::as_str), target.as_str())
            .map_err(Error::new)
    }

    /// Whether `caller` (or nobody, for a host outside any task) may
    /// [`cancel`](Self::cancel) `target` — downward-only, ADR-0017's rule:
    /// itself or a descendant, never an ancestor or a peer. `Err` names which
    /// rule the target breaks; a caller that wants the refusal to win over an
    /// idempotent observation of an already-settled target checks this
    /// first, the way `basis cancel` does.
    pub fn validate_cancel_target(
        &self,
        caller: Option<&TaskHandle>,
        target: &TaskHandle,
    ) -> Result<(), Error> {
        policy::validate_cancel_target(&self.data, caller.map(TaskHandle::as_str), target.as_str())
            .map_err(Error::new)
    }

    /// Requests downward cancellation of `target` and every attached,
    /// non-terminal descendant. Idempotent: cancelling an already-settled
    /// task is a no-op, and this call never blocks on one settling.
    pub fn cancel(&self, handle: &TaskHandle, caller: Option<&TaskHandle>) -> Result<(), Error> {
        // Existence is checked here rather than left to `cancel_tree`, which
        // silently skips a directory that is not there: a caller cancelling a
        // handle that never existed should hear about it.
        attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        policy::validate_cancel_target(&self.data, caller.map(TaskHandle::as_str), handle.as_str())
            .map_err(Error::new)?;
        attach::cancel_tree(&self.data, handle.as_str()).map_err(Error::new)
    }

    /// Opens a cursor over the task's event journal, replay-from-start. Pure
    /// observation — this never attaches or drives; poll it in a loop beside
    /// [`terminal`](Self::terminal) for `basis watch`'s own shape, or just
    /// long enough to catch up on a run already in progress.
    pub fn watch(&self, handle: &TaskHandle) -> Result<EventCursor, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        Ok(EventCursor::new(EventTail::new(&paths, 0)))
    }

    /// The raw terminal record, or `None` for a task still resumable.
    /// Repeatable and lock-free: existence of `terminal.json` *is* the
    /// completion signal (ADR-0019).
    pub fn terminal(&self, handle: &TaskHandle) -> Result<Option<Value>, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        state::read_terminal(&paths).map_err(Error::new)
    }

    /// Whether a live executor currently holds the task's attach lock.
    pub fn is_attached(&self, handle: &TaskHandle) -> Result<bool, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        Ok(lock::is_held(&paths.attach_lock()))
    }

    /// Every message accepted on the task's inbox, bounded 4 KiB summaries
    /// with truncation metadata — the `basis inbox` payload shape.
    pub fn inbox(&self, handle: &TaskHandle) -> Result<Value, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        let messages = inbox::load(&paths).map_err(Error::new)?;
        Ok(inbox::inbox_payload(handle.as_str(), &messages))
    }

    /// The workspace the task was spawned against, as it was recorded at
    /// spawn — not necessarily `Tasks::open`'s own workspace, since a handle
    /// resolves purely from itself.
    pub fn workspace_of(&self, handle: &TaskHandle) -> Result<PathBuf, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        let meta = state::load_meta(&paths).map_err(Error::new)?;
        Ok(PathBuf::from(meta.workspace))
    }

    /// Every task recorded for this `Tasks`'s workspace, last worked in
    /// first. Empty for a workspace nothing has ever run in — that is a
    /// complete answer, not an error.
    pub fn list(&self) -> Result<Vec<TaskSummary>, Error> {
        Ok(tasks::workspace_tasks(&self.data, &self.workspace)
            .map_err(Error::new)?
            .unwrap_or_default())
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Approve;

    fn tasks() -> (tempfile::TempDir, tempfile::TempDir, Tasks) {
        let data_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let tasks = Tasks::open_at(data_dir.path(), workspace.path()).unwrap();
        (data_dir, workspace, tasks)
    }

    /// `Approve::Prompt` names a question nobody can answer without a
    /// `PromptHost`; a `Tasks` with none refuses it at spawn, before a task
    /// directory even exists, rather than minting one that could never make
    /// progress — the doc on [`crate::Approve::Prompt`] promises exactly
    /// this.
    #[test]
    fn spawn_refuses_prompt_mode_with_no_prompt_host() {
        let (_data_dir, _workspace, tasks) = tasks();

        let error = tasks
            .spawn(RunSpec::new("hello").with_approve(Approve::Prompt))
            .expect_err("no PromptHost means Prompt can never be answered");
        assert!(error.to_string().contains("ask"), "{error}");
    }

    /// ADR-0019: an unattended task always gets a finite service bound, even
    /// when the caller named none — `attach::run_model` enforces deadlines
    /// for agents nobody attached to in time, and that has nothing to bound
    /// against without this.
    #[test]
    fn an_unset_deadline_records_the_default_at_spawn() {
        let (_data_dir, _workspace, tasks) = tasks();

        let handle = tasks
            .spawn(RunSpec::new("hello").with_approve(Approve::Always))
            .expect("spawns");

        // Grey-box: `Tasks` exposes no deadline accessor — the durable
        // record is basis-tasks's own business — so the test reads it the
        // way `attach`'s own tests read `meta.json` directly.
        let paths = attach::resolve(&tasks.data, handle.as_str()).expect("agent dir");
        let meta = state::load_meta(&paths).expect("meta.json");
        assert_eq!(
            meta.options.deadline_ms,
            Some(duration_ms(DEFAULT_DEADLINE))
        );
        assert!(
            meta.deadline_at_ms.is_some(),
            "an unattended task always gets a finite service bound"
        );
    }
}
