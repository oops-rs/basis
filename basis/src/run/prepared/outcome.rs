//! Why a turn ended, and how its failure is worded.
//!
//! Split from [`prepared`](super) for the parent's size, and it is one
//! subject: everything a turn's *ending* is classified by. A bound the run
//! outgrew is not a failure of the work, and a failure the caller is handed
//! has to read the same on the stream as it does in the error — both of those
//! are decided here, once, so no call site can hold a second opinion.

use mentra::runtime::{EarlyEnd, RunOptions};

use super::Bound;

/// How a turn ended, in the terms the stream reports.
///
/// Borrowed rather than owned so a caller can hand the failure over for
/// classification and still return it: the error a typed turn reports to its
/// caller and the message the stream carries have to be the same error.
pub(super) enum Ended<'a> {
    /// mentra completed the turn. Carries the assistant's final prose, when the
    /// turn had any — a typed turn's answer is not prose and is not put here.
    Answered(Option<String>),
    /// mentra failed the turn.
    Failed(&'a mentra::error::RuntimeError),
    /// The turn completed, but its answer did not fit the requested type.
    Mismatched(&'a serde_json::Error),
}

/// Which bound ended the turn, if one did.
///
/// Two sources, asked in this order because only one of them is the runner's
/// own account. mentra records a graceful early end at the boundary it decides
/// on — reachable here through the caller's clone of the options — while
/// [`tripped_bound`] can only read a failure after the fact. So the record is
/// consulted first and on *both* arms: a run that ends on its token budget with
/// the assistant's answer already committed returns an ordinary `Ok` carrying
/// ordinary prose, and nothing in that result says an allowance is why there is
/// no more of it.
///
/// [`EarlyEnd::StopRequested`] deliberately maps to nothing. A stop is an
/// instruction the caller issued rather than an allowance the run outgrew, and
/// basis has no `Bound` for it — inventing one would put a caller's own stop
/// button on the same exit code as running out of budget.
pub(super) fn ended_on(
    observed: &RunOptions,
    error: Option<&mentra::error::RuntimeError>,
) -> Option<Bound> {
    match observed.ended_early() {
        Some(EarlyEnd::TokenBudget) => Some(Bound::TokenBudget),
        // `EarlyEnd` is non-exhaustive, and a variant basis has not been taught
        // is not a bound basis can name. Falling through leaves the failure to
        // speak for itself rather than guessing.
        _ => error.and_then(tripped_bound),
    }
}

/// Which of the run's own bounds ended the turn, if one did.
///
/// Classified here, from the typed error, rather than left for someone to
/// recognize in a message later — a caller matching on prose would break the
/// first time mentra reworded one.
pub(super) fn tripped_bound(error: &mentra::error::RuntimeError) -> Option<Bound> {
    match error {
        mentra::error::RuntimeError::DeadlineExceeded => Some(Bound::Deadline),
        mentra::error::RuntimeError::ToolBudgetExceeded(_) => Some(Bound::ToolBudget),
        // Everything else is a failure of the work, not of the allowance: a
        // provider error, a cancelled turn, an unreadable transcript.
        _ => None,
    }
}

/// Renders `error`'s message together with whatever its `source()` chain adds
/// that the message does not already say.
///
/// thiserror interpolates a `#[source]` straight into `Display` wherever a
/// variant's format string names it, and every
/// [`RuntimeError`](mentra::error::RuntimeError) variant does — so
/// `error.to_string()` already reads several layers deep on its own, down to
/// whatever the innermost wrapped type's `Display` shows. The gap is past
/// that point: `reqwest::Error`'s `Display` only classifies itself ("error
/// sending request for url (...)") and never describes its own `source()`, so
/// a DNS failure, a refused connection, or a TLS handshake error — the actual
/// reason a `ProviderError::Transport` or `ProviderError::Decode` failed —
/// reaches neither `to_string()` nor, since mentra's own stream event for the
/// same failure is built the same way (`Session::finish_turn`), the event
/// stream either. Walking the chain here recovers it, and is the only place
/// in basis that needs to: everywhere else a `RuntimeError`'s `Display`
/// already says everything its sources do.
///
/// Safe to run unconditionally. Nothing reachable from a `RuntimeError` today
/// forwards a request or response body through `source()` — that path is
/// `ProviderError::Http`, which interpolates its body into `Display` directly
/// rather than through a source, and this function does not change what it
/// shows. The substring check below is what keeps a level whose text a parent
/// already interpolated — exactly what happens one hop up, via thiserror's
/// own `{0}` — from being repeated.
pub(super) fn chain_message(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(source) = cause {
        let text = source.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        cause = source.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tripped_bound_is_told_apart_from_a_failed_run() {
        use mentra::error::RuntimeError;

        assert_eq!(
            tripped_bound(&RuntimeError::DeadlineExceeded),
            Some(Bound::Deadline)
        );
        assert_eq!(
            tripped_bound(&RuntimeError::ToolBudgetExceeded(40)),
            Some(Bound::ToolBudget)
        );

        // A run the provider refused is a failure, and a shell script that
        // retried it as if it had merely run out of time would retry forever.
        assert_eq!(tripped_bound(&RuntimeError::EmptyAssistantResponse), None);
        assert_eq!(tripped_bound(&RuntimeError::Cancelled), None);
    }

    /// The options a run that ended on `end` hands back to whoever kept a clone.
    fn recorded(end: EarlyEnd) -> RunOptions {
        let slot = std::sync::OnceLock::new();
        let _ = slot.set(end);
        RunOptions {
            early_end: std::sync::Arc::new(slot),
            ..RunOptions::default()
        }
    }

    #[test]
    fn a_run_that_answered_still_names_the_budget_that_ended_it() {
        // The case that makes reading mentra's record load-bearing rather than
        // decorative, and the reason both arms consult it: the turn returns an
        // ordinary `Ok` carrying ordinary prose, so nothing in the result tells
        // "the model was done" from "the allowance ran out" except what the
        // runner wrote down at the boundary it decided at.
        //
        // Checked here rather than end to end because basis cannot reach the
        // shape from outside: mentra only re-checks the budget after a
        // committed final message when a steer or a follow-up is queued behind
        // it, and basis exposes neither.
        assert_eq!(
            ended_on(&recorded(EarlyEnd::TokenBudget), None),
            Some(Bound::TokenBudget)
        );
    }

    #[test]
    fn a_budget_that_ends_a_run_owing_an_answer_is_not_read_as_a_provider_failure() {
        // The shape a driven run reaches — `tests/token_budget.rs` — where the
        // failure is real but the reason for it is the allowance. Classifying
        // by error alone would exit 1 here, sending someone after a broken
        // provider when the fix is a larger budget.
        use mentra::error::RuntimeError;

        assert_eq!(
            ended_on(
                &recorded(EarlyEnd::TokenBudget),
                Some(&RuntimeError::EmptyAssistantResponse)
            ),
            Some(Bound::TokenBudget)
        );
    }

    #[test]
    fn a_graceful_stop_is_not_reported_as_a_bound() {
        // A caller's own stop button is an instruction, not an allowance the
        // run outgrew. basis has no `Bound` for it, and borrowing one would give
        // a client's stop the exit code of a run that ran out of budget.
        use mentra::error::RuntimeError;

        assert_eq!(ended_on(&recorded(EarlyEnd::StopRequested), None), None);
        assert_eq!(
            ended_on(
                &recorded(EarlyEnd::StopRequested),
                Some(&RuntimeError::EmptyAssistantResponse)
            ),
            None
        );
    }

    #[test]
    fn a_run_that_recorded_nothing_is_classified_by_its_failure_alone() {
        use mentra::error::RuntimeError;

        assert_eq!(ended_on(&RunOptions::default(), None), None);
        assert_eq!(
            ended_on(
                &RunOptions::default(),
                Some(&RuntimeError::DeadlineExceeded)
            ),
            Some(Bound::Deadline)
        );
    }

    /// A leaf with no further source — what most of `RuntimeError`'s own
    /// variants look like.
    #[derive(Debug)]
    struct Leaf(&'static str);

    impl std::fmt::Display for Leaf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Leaf {}

    /// A wrapper whose `Display` does not repeat its source's text — the
    /// shape `reqwest::Error` takes, and the one `chain_message` exists for.
    #[derive(Debug)]
    struct Opaque {
        own_text: &'static str,
        source: Leaf,
    }

    impl std::fmt::Display for Opaque {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.own_text)
        }
    }

    impl std::error::Error for Opaque {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    /// A wrapper whose `Display` interpolates its source's text directly —
    /// the shape every `RuntimeError` variant takes via thiserror's `{0}`.
    #[derive(Debug)]
    struct Interpolated {
        source: Leaf,
    }

    impl std::fmt::Display for Interpolated {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wrapper failed: {}", self.source)
        }
    }

    impl std::error::Error for Interpolated {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn a_leaf_error_is_left_exactly_as_its_own_display_wrote_it() {
        let error = Leaf("no providers are registered");

        assert_eq!(chain_message(&error), "no providers are registered");
    }

    #[test]
    fn a_source_a_wrappers_display_never_mentions_is_appended() {
        // `reqwest::Error`'s own case: "error sending request for url (...)"
        // says nothing about *why* the request failed, because it never
        // describes its `source()`. Without walking the chain, that reason —
        // here, "connection refused" — is gone the moment `.to_string()` is
        // called, on the report and on mentra's own stream event alike.
        let error = Opaque {
            own_text: "error sending request for url (http://127.0.0.1:1/)",
            source: Leaf("connection refused (os error 61)"),
        };

        assert_eq!(
            chain_message(&error),
            "error sending request for url (http://127.0.0.1:1/): connection refused (os error 61)"
        );
    }

    #[test]
    fn a_source_a_wrappers_display_already_quotes_is_not_repeated() {
        // Exactly what every `RuntimeError` variant does one hop up, via
        // thiserror's `{0}`: the source's text is already in the parent's
        // `Display`, so walking `source()` too would say it twice.
        let error = Interpolated {
            source: Leaf("disk quota exceeded"),
        };

        assert_eq!(
            chain_message(&error),
            "wrapper failed: disk quota exceeded",
            "the source's text must appear once, not twice"
        );
    }

    #[test]
    fn a_chain_three_levels_deep_still_reaches_its_root_cause() {
        struct Middle {
            source: Opaque,
        }

        impl std::fmt::Debug for Middle {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("Middle").finish()
            }
        }

        impl std::fmt::Display for Middle {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "failed to send provider request: {}", self.source)
            }
        }

        impl std::error::Error for Middle {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.source)
            }
        }

        // `RuntimeError::FailedToSendRequest` wrapping `ProviderError::Transport`
        // wrapping `reqwest::Error`: two levels interpolate cleanly into
        // `Display`, and the third — the one that doesn't — is where the
        // actual cause was hiding.
        let error = Middle {
            source: Opaque {
                own_text: "provider transport error: error sending request for url (http://127.0.0.1:1/)",
                source: Leaf("connection refused (os error 61)"),
            },
        };

        assert_eq!(
            chain_message(&error),
            "failed to send provider request: provider transport error: error sending request for url (http://127.0.0.1:1/): connection refused (os error 61)"
        );
    }

    #[test]
    fn a_real_runtime_errors_already_complete_message_is_unchanged() {
        // serde_json's `Display` is already the full story — message, line,
        // and column — and its `source()` is written to skip back to
        // whatever `Display` already showed rather than repeat it, the same
        // shape as most of `RuntimeError`'s own variants. The chain walk must
        // add nothing here, on a real mentra error rather than a synthetic one.
        use mentra::error::RuntimeError;

        let parse_error = serde_json::from_str::<serde_json::Value>("{")
            .expect_err("truncated JSON does not parse");
        let error = RuntimeError::FailedToSerializeTasks(parse_error);

        assert_eq!(chain_message(&error), error.to_string());
    }
}
