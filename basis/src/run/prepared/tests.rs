//! What `prepared.rs` settles without a provider.
//!
//! Split out for the parent's size, the same remedy `basis-acp/src/server.rs`
//! took: the file was past the 800-line ceiling with these inline. What is
//! here is everything a run can be asked about before anything is sent — the
//! header it opens with, which bound ended a turn, and how an error's chain is
//! rendered. Anything needing a live session is driven end to end from
//! `basis/tests/`.

use super::*;
use crate::context::{ContextDocument, ContextScope};

#[test]
fn the_header_lists_context_files_weakest_first() {
    let context = WorkspaceContext::from_documents(vec![
        ContextDocument {
            path: PathBuf::from("/AGENTS.md"),
            scope: ContextScope::Ancestor { depth: 2 },
            content: "outer".to_string(),
        },
        ContextDocument {
            path: PathBuf::from("/repo/AGENTS.md"),
            scope: ContextScope::Workspace,
            content: "inner".to_string(),
        },
    ]);

    let files: Vec<ContextFile> = context
        .documents()
        .iter()
        .map(|document| ContextFile {
            path: document.path.clone(),
            scope: document.scope.label(),
        })
        .collect();

    assert_eq!(files[0].scope, "ancestor:2");
    assert_eq!(files[1].scope, "workspace");
}

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

    let parse_error =
        serde_json::from_str::<Value>("{").expect_err("truncated JSON does not parse");
    let error = RuntimeError::FailedToSerializeTasks(parse_error);

    assert_eq!(chain_message(&error), error.to_string());
}
