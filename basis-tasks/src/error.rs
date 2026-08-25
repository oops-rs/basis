//! One error type for the whole crate.
//!
//! Every fallible operation in the moved implementation — file I/O, decode
//! failures, policy refusals — already carries a human-readable message; that
//! was true when it answered only to `basis-cli`'s own `ClientError`, and
//! nothing about becoming a library changes it. What changes is the type: a
//! `Result<_, String>` is an implementation detail a caller should not have
//! to `.to_string()` their way around, so the public surface returns this
//! instead. `basis-cli`'s exit-code and hint mapping — which of these are a
//! timeout, which are a usage error — stays exactly where ADR-0015 puts it,
//! in the CLI's own `ClientError`; this type carries no opinion about either.

use std::fmt;

/// An error from a `basis-tasks` operation: what went wrong, in one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
    invalid_reference: bool,
}

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            invalid_reference: false,
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
