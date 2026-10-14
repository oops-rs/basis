//! The contract a remote agent is spoken to through (ADR-0038).
//!
//! `spawn` needs little of an agent somewhere else: hand it a message,
//! continuing a conversation or answering a task it asked about; wait again
//! on a task it has not finished; stop waiting when the run stops; and know
//! how much of what it says to show. Everything around that — which
//! conversation a task belongs to, which task is outstanding, and what the
//! model reads — is basis's (ADR-0029 §5) and lives in `tools::spawn`.
//!
//! So this is the whole of what crosses to a peer: a [`Request`] in, an
//! [`Outcome`] or a [`PeerError`] out, with the ids basis records and the
//! text already framed as the remote agent's own words. The `a2a` feature
//! implements it for A2A; a host implements it for
//! anything else that can take a task and answer.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use mentra::runtime::RunOptions;

/// Something a task can be handed to, and that answers.
#[async_trait]
pub trait Peer: Send + Sync {
    /// Hands `request` over and reports what came back, waiting within
    /// `bounds` for a task to settle.
    ///
    /// A peer that starts a task says so through
    /// [`ExchangeBounds::started`] *before* it waits, so that a process which dies
    /// mid-wait comes back knowing the task exists and waits on it rather
    /// than handing the work off again. A peer that answers at once need
    /// not.
    async fn exchange(
        &self,
        request: Request,
        bounds: &ExchangeBounds<'_>,
    ) -> Result<Outcome, PeerError>;

    /// How many bytes of what the agent says reach the model.
    fn max_output_bytes(&self) -> usize;
}

/// What `spawn` hands a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// A message: a new task, the next in a conversation, or the answer to
    /// a task waiting for one.
    Send(Message),
    /// Nothing new: wait again on a task the agent has not finished.
    Wait { task_id: String },
}

impl Request {
    pub fn wait(task_id: impl Into<String>) -> Self {
        Self::Wait {
            task_id: task_id.into(),
        }
    }
}

/// One message to a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Message {
    /// What the model wrote after `@<name>`.
    pub text: String,
    /// The conversation this continues, when one is open.
    pub context_id: Option<String>,
    /// The task this answers, when the agent asked something.
    pub answering: Option<String>,
    /// Which hop this message is in a chain of agents: the sender's own plus
    /// one, so a top-level caller sends 1. A peer carries it however its wire
    /// can, or not at all.
    pub hop: u32,
    /// When this side stops waiting, when it knows.
    pub deadline: Option<SystemTime>,
}

impl Message {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            context_id: None,
            answering: None,
            hop: 1,
            deadline: None,
        }
    }

    #[must_use]
    pub fn in_context(self, context_id: Option<String>) -> Self {
        Self { context_id, ..self }
    }

    #[must_use]
    pub fn answering(self, task_id: Option<String>) -> Self {
        Self {
            answering: task_id,
            ..self
        }
    }

    #[must_use]
    pub fn at_hop(self, hop: u32, deadline: Option<SystemTime>) -> Self {
        Self {
            hop,
            deadline,
            ..self
        }
    }
}

/// The run's bounds on an exchange, and where to report while it waits.
pub struct ExchangeBounds<'a> {
    /// The run's own cancellation, stop and deadline, which the wait obeys.
    pub options: &'a RunOptions,
    /// Progress to show while the task runs, as the remote agent's own words.
    pub progress: &'a (dyn Fn(String) + Send + Sync),
    /// Told the task a request started, before the peer waits on it.
    pub started: &'a (dyn Fn(Started) + Send + Sync),
}

/// A task a request started, as the peer knows it before waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Started {
    pub task_id: String,
    pub context_id: Option<String>,
}

/// What came back.
///
/// `text` is what the agent said, already framed as its own words and cut
/// to the peer's output cap; empty when it said nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Answered with a message and started no task.
    Replied {
        context_id: Option<String>,
        text: String,
    },
    /// The task finished.
    Finished {
        task_id: String,
        context_id: Option<String>,
        text: String,
    },
    /// The task is still running when the wait ended; waiting again later
    /// picks it up.
    Working {
        task_id: String,
        context_id: Option<String>,
        text: String,
        why: StillWorking,
    },
    /// The agent asked something; the next message answers it.
    NeedsInput {
        task_id: String,
        context_id: Option<String>,
        text: String,
    },
    /// The agent needs an authorization this conversation cannot give.
    NeedsAuthorization {
        task_id: String,
        context_id: Option<String>,
        text: String,
    },
    /// The task ended without doing the work.
    Failed {
        task_id: String,
        context_id: Option<String>,
        text: String,
        how: Failure,
    },
}

/// Why a wait ended with the task still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StillWorking {
    /// The peer's wait bound passed; carried for the telling.
    WaitBound(Duration),
    /// The run asked to stop at its next boundary.
    RunStopping,
}

/// How a task ended without doing the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Failure {
    Failed,
    Rejected,
    Canceled,
}

/// Why an exchange has no outcome.
///
/// Text in these is safe to show the model: a peer puts no server body and
/// no credential in it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerError {
    /// The request itself failed; nothing is known to be running.
    Send(String),
    /// Checking on a task failed; it may still be running.
    Check { task_id: String, error: String },
    /// The agent does not know the task asked about.
    UnknownTask,
    /// The run was cancelled while the task ran, and the task was asked to
    /// cancel — with how that went.
    Abandoned {
        task_id: String,
        canceled: Result<(), String>,
    },
    /// The agent answered in a way the peer does not understand.
    Unrecognized,
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Send(error) => write!(f, "the request failed: {error}"),
            Self::Check { task_id, error } => {
                write!(f, "checking on task {task_id} failed: {error}")
            }
            Self::UnknownTask => write!(f, "the agent does not know the task"),
            Self::Abandoned { task_id, .. } => write!(f, "task {task_id} was abandoned"),
            Self::Unrecognized => {
                write!(f, "the agent answered in a way spawn does not understand")
            }
        }
    }
}

impl std::error::Error for PeerError {}
