//! One error type for the whole crate.
//!
//! Every fallible operation in the moved implementation — file I/O, decode
//! failures, policy refusals — already carries a human-readable message; that
//! was true when it answered only to `basis-cli`'s own `ClientError`, and
//! nothing about becoming a library changes it. What changes is the type: a
//! `Result<_, String>` is an implementation detail a caller should not have
//! to `.to_string()` their way around, so the public surface returns this
//! instead. `basis-cli`'s exit-code and hint *text* — which of these are a
//! timeout, which are a usage error, what a `next:` line reads — stays
//! exactly where ADR-0015 puts it, in the CLI's own `ClientError`; this type
//! carries no opinion about either. What it does carry, for the handful of
//! errors that name one unambiguously, is [`hint`](Error::hint) — a fact a
//! host builds its own hint text from, not the text itself.

use std::fmt;

use crate::handle::TaskHandle;

/// A next step this error names unambiguously enough for a host to build its
/// own hint text from, without parsing this error's message. Not every
/// error carries one: an ordinary operational failure has no next step this
/// crate can name from here, and stays [`None`](Error::hint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    /// The caller is itself a task in a different workspace; spawning here
    /// needs the detached escape — `basis spawn --detached <PROMPT>`, in
    /// `basis-cli`'s words.
    SpawnDetached,
    /// No task in the target workspace has a conversation to continue yet —
    /// `basis spawn <PROMPT>`, in `basis-cli`'s words.
    SpawnFresh,
    /// The conversation named exists but cannot be continued right now —
    /// still running (one executor per conversation), or never attached —
    /// so the next step is the task itself: `basis wait <task>`.
    Wait(TaskHandle),
}

/// An error from a `basis-tasks` operation: what went wrong, in one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
    invalid_reference: bool,
    hint: Option<Hint>,
}

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            invalid_reference: false,
            hint: None,
        }
    }

    /// An error over a handle or reference the caller gave that could never
    /// have resolved — not a task in a state that might still change (a
    /// running task, an expired wait), but one that was never going to name
    /// anything valid, the way a malformed handle or a handle from a
    /// different workspace does not.
    pub(crate) fn invalid_reference(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            invalid_reference: true,
            hint: None,
        }
    }

    /// Attaches the next step this error names, for a host that wants to
    /// build a hint from it.
    #[must_use]
    pub(crate) fn with_hint(self, hint: Hint) -> Self {
        Self {
            hint: Some(hint),
            ..self
        }
    }

    /// Whether this is exactly that: a bad argument no amount of waiting
    /// fixes, as opposed to an ordinary operational failure. A host mapping
    /// this crate's errors onto its own vocabulary — `basis-cli`'s "usage"
    /// exit code (ADR-0015), for one — reads this rather than the message
    /// text.
    pub fn is_invalid_reference(&self) -> bool {
        self.invalid_reference
    }

    /// The next step this error names, when it names one unambiguously —
    /// see [`Hint`].
    pub fn hint(&self) -> Option<&Hint> {
        self.hint.as_ref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}
