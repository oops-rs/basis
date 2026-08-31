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
    Error, Hint,
    approve::PromptHost,
    attach,
    data_dir::{self, DataDir, canonical_workspace},
    events::EventTail,
    handle::TaskHandle,
    inbox,
    live::{DriveContext, LiveSink},
    lock, policy,
    spec::{Continuation, DEFAULT_DEADLINE, RunSpec},
    state::{self, InboxRecord, RunOptions, TaskMeta, TerminalRecord},
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
///
/// `Clone` because a handful of this type's own `async fn`s need an owned
/// copy to move onto a blocking thread (see `blocking`, below) — every field
/// is already cheap to clone (a path, an `Arc`), so this costs nothing a
/// caller could not already do by hand.
#[derive(Clone)]
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
            ))
            .with_hint(Hint::SpawnDetached));
        }
        let parent = if detached { None } else { caller };

        // T2(a): resolving which conversation to continue, minting the task
        // directory, and recording the claim (`continues`) on it are one
        // unit against a second spawn targeting this same workspace —
        // otherwise two concurrent `--continue`s could both resolve the same
        // conversation before either's claim is on disk for the other to
        // see. Held past `save_meta` below, then dropped explicitly: nothing
        // after that point reads or writes what this protects.
        let continue_lock = lock::exclusive(&self.data.continue_lock(&key))
            .map_err(|error| Error::new(format!("acquire workspace continuation lock: {error}")))?;
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
        drop(continue_lock);

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
                    .with_hint(Hint::SpawnFresh)
            })?,
        };
        if chosen.state == "running" {
            return Err(Error::new(format!(
                "task {} is running; its attach lock is what keeps one conversation to one \
                 executor",
                chosen.task
            ))
            .with_hint(Hint::Wait(TaskHandle::parse(chosen.task.clone())?)));
        }
        if chosen.agent_id.is_empty() {
            return Err(Error::new(format!(
                "task {} has no conversation yet: nothing has attached to it",
                chosen.task
            ))
            .with_hint(Hint::Wait(TaskHandle::parse(chosen.task.clone())?)));
        }
        // T2(a): a sibling that already recorded `continues` against this
        // same conversation, and has not yet settled, holds an open claim on
        // it — minting a second claimant here is exactly the race that lets
        // two executors resume one conversation at once.
        if let Some(claimant) =
            tasks::claimed_continuation(&self.data, key, &chosen.agent_id).map_err(Error::new)?
        {
            let claimant = TaskHandle::parse(claimant)?;
            return Err(Error::new(format!(
                "task {claimant} already continues this conversation; a conversation admits \
                 one open claim at a time"
            ))
            .with_hint(Hint::Wait(claimant)));
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
    ///
    /// **Threading:** the edge check and the enqueue are this call's own
    /// synchronous prelude and run through this crate's own `blocking`
    /// helper; the wait after it is `attach::wait_for_message`'s own.
    pub async fn ask(
        &self,
        handle: &TaskHandle,
        caller: Option<&TaskHandle>,
        message: impl Into<String>,
        timeout: Duration,
    ) -> Result<Reply, Error> {
        let data = self.data.clone();
        let target = handle.clone();
        let caller = caller.cloned();
        let message = message.into();
        let message_id = blocking(move || {
            policy::validate_wait_edge(
                &data,
                caller.as_ref().map(TaskHandle::as_str),
                target.as_str(),
            )
            .map_err(Error::new)?;
            let paths = attach::resolve(&data, target.as_str()).map_err(Error::new)?;
            inbox::enqueue(&paths, target.as_str(), message).map_err(Error::new)
        })
        .await?;
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
    ///
    /// **Threading:** the dispatch check and the edge check are this call's
    /// own synchronous prelude and run through this crate's own `blocking`
    /// helper; the wait after it, when one is still needed, is
    /// `attach::wait_for_message`'s own.
    pub async fn wait_message(
        &self,
        handle: &TaskHandle,
        caller: Option<&TaskHandle>,
        message_id: &str,
        timeout: Duration,
    ) -> Result<WaitOutcome, Error> {
        let data = self.data.clone();
        let target = handle.clone();
        let caller = caller.cloned();
        let mid = message_id.to_string();
        let resolved = blocking(move || -> Result<Option<Value>, Error> {
            let paths = attach::resolve(&data, target.as_str()).map_err(Error::new)?;
            let messages = inbox::load(&paths).map_err(Error::new)?;
            let terminal = state::read_terminal(&paths).map_err(Error::new)?;
            if let Some(payload) = inbox::message_payload_for_dispatch(
                target.as_str(),
                &messages,
                &mid,
                terminal.as_ref(),
            )
            .map_err(Error::new)?
            {
                return Ok(Some(payload));
            }
            policy::validate_wait_edge(
                &data,
                caller.as_ref().map(TaskHandle::as_str),
                target.as_str(),
            )
            .map_err(Error::new)?;
            Ok(None)
        })
        .await?;
        match resolved {
            Some(payload) => Ok(WaitOutcome::Terminal(TerminalRecord::from_raw(payload))),
            None => attach::wait_for_message(
                &self.data,
                handle.as_str(),
                message_id,
                timeout,
                self.prompt_host.clone(),
            )
            .await
            .map_err(Error::new),
        }
    }

    /// Awaits the task's terminal record, attaching to drive it whenever the
    /// attach lock is free — repeatable: a settled task's record is read
    /// straight off disk, never rerun. `live`, when given, is shown every
    /// event while (and only while) this call is the one driving.
    ///
    /// **Threading:** the terminal read and the edge check are this call's
    /// own synchronous prelude and run through this crate's own `blocking`
    /// helper; the wait after it, when one is still needed, is this crate's
    /// own `wait_unvalidated`'s.
    pub async fn wait(
        &self,
        handle: &TaskHandle,
        caller: Option<&TaskHandle>,
        timeout: Duration,
        live: Option<Arc<dyn LiveSink>>,
    ) -> Result<WaitOutcome, Error> {
        let data = self.data.clone();
        let target = handle.clone();
        let caller = caller.cloned();
        let resolved = blocking(move || -> Result<Option<Value>, Error> {
            let paths = attach::resolve(&data, target.as_str()).map_err(Error::new)?;
            if let Some(terminal) = state::read_terminal(&paths).map_err(Error::new)? {
                return Ok(Some(terminal));
            }
            policy::validate_wait_edge(
                &data,
                caller.as_ref().map(TaskHandle::as_str),
                target.as_str(),
            )
            .map_err(Error::new)?;
            Ok(None)
        })
        .await?;
        match resolved {
            Some(terminal) => Ok(WaitOutcome::Terminal(TerminalRecord::from_raw(terminal))),
            None => self.wait_unvalidated(handle, timeout, live).await,
        }
    }

    /// [`wait`](Self::wait) minus the edge check — never exposed on its own,
    /// since any caller could reach it for any handle and there would be
    /// nothing left of the wait-edge policy. [`spawn_and_wait`](Self::spawn_and_wait)
    /// is the one legitimate skip: the edge between a call's own caller and
    /// the task it just minted is exactly the one [`spawn`](Self::spawn)
    /// established a moment ago (a fresh descendant, or an independent root
    /// when detached), and re-deriving it through a `caller` a host is free
    /// to pass differently here would recompute what that call already
    /// knows — against a value that, if stale, could strand a task nobody
    /// is left validated to drive.
    ///
    /// **Threading:** the terminal read is this call's own synchronous
    /// prelude and runs through [`blocking`]; the wait after it is
    /// [`attach::wait_for_terminal`]'s own — see its doc for where every lock
    /// and fs read *it* makes runs, model turns included.
    async fn wait_unvalidated(
        &self,
        handle: &TaskHandle,
        timeout: Duration,
        live: Option<Arc<dyn LiveSink>>,
    ) -> Result<WaitOutcome, Error> {
        let data = self.data.clone();
        let target = handle.clone();
        let resolved = blocking(move || -> Result<Option<Value>, Error> {
            let paths = attach::resolve(&data, target.as_str()).map_err(Error::new)?;
            state::read_terminal(&paths).map_err(Error::new)
        })
        .await?;
        if let Some(terminal) = resolved {
            return Ok(WaitOutcome::Terminal(TerminalRecord::from_raw(terminal)));
        }
        let ctx = self.ctx(live);
        attach::wait_for_terminal(&self.data, handle.as_str(), timeout, &ctx)
            .await
            .map_err(Error::new)
    }

    /// Mints a task and immediately attaches to drive it to a terminal
    /// result — `spawn --await`'s shape, and the one place a wait skips edge
    /// validation: see this crate's private `wait_unvalidated` for why that
    /// is safe only here.
    ///
    /// **Threading:** [`spawn`](Self::spawn) is a synchronous unit in its own
    /// right — a directory scan, the continuation lock, `create_dir`,
    /// `save_meta` — and runs through this crate's own `blocking` helper here
    /// exactly as it would if this crate wrote it as its own
    /// `blocking`-wrapped prelude; the wait after it is this crate's own
    /// `wait_unvalidated`'s.
    pub async fn spawn_and_wait(
        &self,
        spec: RunSpec,
        timeout: Duration,
        live: Option<Arc<dyn LiveSink>>,
    ) -> Result<(TaskHandle, WaitOutcome), Error> {
        let tasks = self.clone();
        let handle = blocking(move || tasks.spawn(spec)).await?;
        let outcome = self.wait_unvalidated(&handle, timeout, live).await?;
        Ok((handle, outcome))
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

    /// The terminal record, raw and typed side by side, or `None` for a task
    /// still resumable.
    /// Repeatable and lock-free: existence of `terminal.json` *is* the
    /// completion signal (ADR-0019).
    pub fn terminal(&self, handle: &TaskHandle) -> Result<Option<TerminalRecord>, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        state::read_terminal(&paths)
            .map(|record| record.map(TerminalRecord::from_raw))
            .map_err(Error::new)
    }

    /// Whether a live executor currently holds the task's attach lock.
    pub fn is_attached(&self, handle: &TaskHandle) -> Result<bool, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        Ok(lock::is_held(&paths.attach_lock()))
    }

    /// Every message accepted on the task's inbox, bounded 4 KiB summaries
    /// with truncation metadata — the `basis inbox` payload shape.
    pub fn inbox(&self, handle: &TaskHandle) -> Result<InboxRecord, Error> {
        let paths = attach::resolve(&self.data, handle.as_str()).map_err(Error::new)?;
        let messages = inbox::load(&paths).map_err(Error::new)?;
        Ok(inbox::inbox_record(handle.as_str(), &messages))
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

/// Runs one bounded, synchronous unit of this crate's own blocking work —
/// a lock acquisition, an fs read or write, [`Tasks::spawn`]'s own mint — off
/// the caller's async executor and onto tokio's blocking thread pool (G7's
/// pattern, `ca9ddcb`, applied at this crate's own boundary).
///
/// Every `pub async fn` on [`Tasks`] that has a synchronous prelude (an edge
/// check, an enqueue, a terminal read, `spawn` itself) runs it through this
/// rather than inline on the caller's task — see each method's own
/// `**Threading:**` note for which part that is. [`attach::wait_for_terminal`]
/// and [`attach::wait_for_message`] carry the other half of the same rule for
/// the poll loop and the model turns it drives.
async fn blocking<T, F>(work: F) -> Result<T, Error>
where
    F: FnOnce() -> Result<T, Error> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| Err(Error::new(format!("background task failed: {error}"))))
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

    /// The general hazard `spawn_and_wait` exists to route around: an
    /// ordinary `wait` refuses a caller that does not exist to validate the
    /// edge against — the shape a stale `BASIS_TASK_ID` takes once its own
    /// task directory is gone (archived, or simply never real).
    #[tokio::test]
    async fn wait_refuses_a_caller_that_does_not_exist() {
        let (_data_dir, _workspace, tasks) = tasks();
        let stale_caller = TaskHandle::parse(format!("{:016x}/{:032x}", 1, 1)).unwrap();
        let handle = tasks
            .spawn(
                RunSpec::new("hello")
                    .with_approve(Approve::Always)
                    .detached(),
            )
            .expect("spawns");

        let refused = tasks
            .wait(&handle, Some(&stale_caller), Duration::from_secs(1), None)
            .await
            .expect_err("a caller that does not exist cannot be validated against");
        assert!(refused.to_string().contains("does not exist"), "{refused}");
    }

    /// `spawn_and_wait` never asks the wait-edge question at all, so a stale
    /// `BASIS_TASK_ID` — one naming a caller task that no longer exists —
    /// cannot strand the task this same call just minted, the way routing a
    /// `spawn --await` through the ordinary edge-validated `wait` used to.
    #[tokio::test]
    async fn spawn_and_wait_does_not_edge_validate_the_task_it_just_minted() {
        let (_data_dir, _workspace, tasks) = tasks();
        // A deadline so tight it has certainly passed by the time the attach
        // that follows runs: `run_model` bails on it before ever opening a
        // workspace or touching a provider, which is what keeps this test
        // fast and network-free while still exercising a real attach.
        let spec = RunSpec::new("hello")
            .with_approve(Approve::Always)
            .with_deadline(Duration::from_nanos(1))
            .detached();

        let (_handle, outcome) = tasks
            .spawn_and_wait(spec, Duration::from_secs(5), None)
            .await
            .expect("spawn_and_wait never asks the wait-edge question at all");
        let WaitOutcome::Terminal(payload) = outcome else {
            panic!("a tight deadline settles immediately rather than timing the wait out");
        };
        assert_eq!(payload.raw["state"], "failed");
        assert_eq!(payload.stopped_by, Some(basis::Bound::Deadline));
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

    /// `is_attached` is documented as a read, and `list` asks the same
    /// question of every task it reports. A read that leaves a file behind is
    /// not one: probing a task nothing has ever attached must find no lock
    /// and mint none, so a listing of a workspace is not also a write to
    /// every task directory in it.
    #[test]
    fn probing_attachment_does_not_mint_the_lock_file() {
        let (_data_dir, _workspace, tasks) = tasks();
        let handle = tasks
            .spawn(
                RunSpec::new("hello")
                    .with_approve(Approve::Always)
                    .detached(),
            )
            .expect("spawns");
        let paths = attach::resolve(&tasks.data, handle.as_str()).expect("agent dir");
        assert!(
            !paths.attach_lock().exists(),
            "a task nothing has attached starts with no lock file"
        );

        assert!(!tasks.is_attached(&handle).expect("probes"));
        let listed = tasks.list().expect("lists");
        assert!(listed.iter().any(|task| task.task == handle.as_str()));

        assert!(
            !paths.attach_lock().exists(),
            "observing a task must leave its directory as it found it"
        );
    }

    /// T3: `wait`'s own lock and fs work — the poll loop's `resolve`,
    /// `read_terminal`, `try_attach`, all of it — runs off the tokio worker
    /// thread this test's (deliberately single-threaded) executor is. A
    /// concurrent, purely async ticker sharing that one worker keeps making
    /// progress the whole time `wait`'s poll loop sees a contended attach
    /// lock, which a `wait` running any of that work inline could not
    /// guarantee: a `current_thread` runtime has exactly one worker, and
    /// blocking it stalls everything else scheduled there.
    #[tokio::test(flavor = "current_thread")]
    async fn wait_does_not_block_the_executor_it_is_called_from() {
        let (_data_dir, _workspace, tasks) = tasks();
        let handle = tasks
            .spawn(
                RunSpec::new("hello")
                    .with_approve(Approve::Always)
                    // Fails fast, without a network call, the moment it is
                    // actually driven — this test's own point is entirely
                    // about the poll loop leading up to that, not the turn.
                    .with_provider("not-a-provider"),
            )
            .expect("spawns");
        let paths = attach::resolve(&tasks.data, handle.as_str()).expect("agent dir");
        // Stands in for another process already driving the task: `wait`'s
        // poll loop sees a contended attach lock every iteration for a
        // controlled stretch.
        let held = attach::try_attach(&paths).unwrap().expect("lock is free");

        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let ticker = async move {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        };
        let releaser = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(held);
        };
        let waiter = tasks.wait(&handle, None, Duration::from_secs(5), None);
        let (outcome, (), ()) = tokio::join!(waiter, ticker, releaser);
        outcome.expect("wait completes once the lock frees");
        assert!(
            ticks.load(std::sync::atomic::Ordering::SeqCst) >= 30,
            "a ticker sharing this runtime's one worker thread kept making \
             progress while wait's poll loop was contended — proof that \
             loop's lock probes and fs reads never held that thread"
        );
    }

    /// T2(a): a task minted with `continues = Some(agent_id)` holds an open
    /// claim on that conversation until it settles — a second spawn that
    /// would continue the same conversation refuses rather than mint a
    /// second claimant, the double-continuation race `continue_lock` and
    /// this check exist to close.
    #[test]
    fn a_second_continuation_of_the_same_conversation_is_refused_while_the_first_is_open() {
        let (_data_dir, _workspace, tasks) = tasks();

        // Stand in for a task that finished with a conversation to
        // continue — built directly, the way `attach`'s own tests build a
        // completed task, rather than driven through a real model.
        let done = tasks
            .spawn(RunSpec::new("hello").with_approve(Approve::Always))
            .expect("spawns");
        let paths = attach::resolve(&tasks.data, done.as_str()).expect("agent dir");
        let mut meta = state::load_meta(&paths).expect("meta.json");
        meta.agent_id = "conversation-1".to_string();
        state::save_meta(&paths, &meta).expect("save");
        state::write_terminal(
            &paths,
            &serde_json::json!({"state": "succeeded", "result": "d"}),
        )
        .expect("terminal");

        let first = tasks
            .spawn(
                RunSpec::new("step two")
                    .with_approve(Approve::Always)
                    .continuing(crate::Continuation::Latest),
            )
            .expect("the first continuation claims the conversation");

        let refused = tasks
            .spawn(
                RunSpec::new("step two, again")
                    .with_approve(Approve::Always)
                    .continuing(crate::Continuation::Latest),
            )
            .expect_err("a second, still-open claim on the same conversation is refused");
        assert!(refused.to_string().contains(first.as_str()), "{refused}");

        // Once the first claimant settles, its claim releases and a fresh
        // continuation of the same conversation is ordinary again.
        let first_paths = attach::resolve(&tasks.data, first.as_str()).expect("agent dir");
        state::write_terminal(
            &first_paths,
            &serde_json::json!({"state": "succeeded", "result": "d2"}),
        )
        .expect("terminal");
        tasks
            .spawn(
                RunSpec::new("step three")
                    .with_approve(Approve::Always)
                    .continuing(crate::Continuation::Latest),
            )
            .expect("a settled claimant no longer blocks the conversation");
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
