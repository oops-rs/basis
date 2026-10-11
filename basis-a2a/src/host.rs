//! A host's own tasks, served (ADR-0033).
//!
//! By default every task this crate serves is a `basis-tasks` spawn in a
//! workspace. A host that already runs tasks of its own — its own roster,
//! its own persisted runs, its own answer shape — serves them instead by
//! implementing [`TaskHost`] and opening the service with
//! [`ServeConfig::hosted`](crate::ServeConfig::hosted). Everything above
//! this seam is the same for both: the two bindings, the card, principals
//! and their scoping, the live-task quota, stream caps, hops and deadlines,
//! push notifications and the extended card.
//!
//! The seam is the handful of points at which the server touches the task
//! layer: it starts a task, asks where one stands, cancels one, waits for
//! one to settle, and follows its progress. Ids cross it in one direction
//! only: the host names its task with an id of its own, the server keeps
//! that id beside the A2A id it mints, and a caller never sees the host's.

use a2a::Part;
use futures::{future::BoxFuture, stream::BoxStream};

/// What a message asks a host to start: everything the server decided
/// about it before the host is called.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostedStart {
    /// The name of the principal whose message this is (ADR-0030 §8).
    pub principal: String,
    /// The A2A `contextId` the task belongs to: the one the message named,
    /// or one the server minted for a new conversation.
    pub context_id: String,
    /// The host's id for the latest task in this context, when the message
    /// continues a conversation; `None` for a new one.
    pub continues: Option<String>,
    /// The message's parts rendered as one prompt: text as written, data as
    /// labelled JSON, inline textual files fenced under their names — what a
    /// `basis-tasks` task would be spawned with.
    pub prompt: String,
    /// The message's parts, exactly as sent.
    pub parts: Vec<Part>,
    /// The hop the message arrived at (`docs/a2a/hops-v1.md`); 0 from a
    /// top-level caller.
    pub hop: u32,
    /// When the task must have settled by, in milliseconds since the Unix
    /// epoch: the server's default deadline, already clamped to the
    /// caller's. The host must not run the task past it.
    pub deadline_ms: u64,
}

/// Where a host's task stands.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum HostedStanding {
    /// Accepted, and not yet running.
    Submitted,
    Working,
    /// Settled with an answer: these parts become the task's one `result`
    /// artifact, exactly as given. An answer of no parts has no artifact.
    Completed {
        parts: Vec<Part>,
    },
    /// Settled without an answer; `error` is the status message a caller
    /// reads.
    Failed {
        error: String,
    },
    /// Settled because it was canceled.
    Canceled,
}

impl HostedStanding {
    /// Whether the task has settled: completed, failed or canceled.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Canceled
        )
    }
}

/// One note on a host's task's progress. Each becomes a `WORKING` status
/// update whose message is the note, as an assistant message or a tool
/// starting does for a `basis-tasks` task.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostedProgress {
    pub note: String,
}

impl HostedProgress {
    pub fn new(note: impl Into<String>) -> Self {
        Self { note: note.into() }
    }
}

/// Why a host refused or failed a request — and so whose fault it is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HostError {
    /// The context is still working on an earlier message. The caller is
    /// told to wait for it to settle and send again, exactly as a
    /// `basis-tasks` conversation with a task still running tells it.
    #[error("busy: {0}")]
    Busy(String),
    /// The request is the caller's to fix; the message is told to it as
    /// invalid params.
    #[error("refused: {0}")]
    Refused(String),
    /// The host's own failure. Logged where the operator reads it; the
    /// caller is told only that the server failed.
    #[error("{0}")]
    Internal(String),
}

/// A host that runs the tasks a server serves (ADR-0033 §1).
///
/// Object safe, and used as `Arc<dyn TaskHost>`. Every `id` passed back in
/// is one [`start`](Self::start) returned, in this process or an earlier
/// one: the server's index keeps it across restarts, and on
/// [`A2aService::open`](crate::A2aService::open) awaits
/// [`settled`](Self::settled) again for each task that had not settled. It
/// never calls `start` again for a message it already started; a task that
/// did not survive the host's own restart is the host's to report as
/// [`HostedStanding::Failed`], with the reason.
pub trait TaskHost: Send + Sync + 'static {
    /// Begins the work `start` asks for and returns the host's own id for
    /// it. The id is never shown to a caller.
    fn start(&self, start: HostedStart) -> BoxFuture<'_, Result<String, HostError>>;

    /// Where the task `id` stands now.
    fn standing(&self, id: &str) -> BoxFuture<'_, Result<HostedStanding, HostError>>;

    /// Asks the task `id` to stop. `CancelTask` then answers with its
    /// standing once it settles, or after a grace of a few seconds.
    fn cancel(&self, id: &str) -> BoxFuture<'_, Result<(), HostError>>;

    /// Resolves once the task `id` has settled — at once, when it already
    /// has. The server's driver for the task awaits it.
    fn settled(&self, id: &str) -> BoxFuture<'_, Result<(), HostError>>;

    /// The task's progress notes from now on, ending when it settles. A
    /// host with nothing to say returns an empty stream; a stream that ends
    /// early only means fewer notes, never a settled task.
    fn progress(&self, id: &str) -> BoxStream<'static, HostedProgress>;
}
