//! The four numbers a script branches on.
//!
//! ADR-0015 makes these contract, and a contract is easier to keep when it
//! fits on one screen: `--json` can grow a field and the prose can be
//! reworded, but a `case $?` somebody wrote against lan a year ago has to keep
//! meaning what it meant. So the codes and the one function that chooses
//! between them sit together, and the whole promise is this file.
//!
//! The choosing is deliberately narrow — it reads two fields of a finished
//! [`RunReport`] and nothing else. Everything upstream that fails before a run
//! exists reports [`EXIT_FAILED`] where it failed, because at that point there
//! is no report to map.

use lan_core::RunReport;

/// The run finished.
pub(crate) const EXIT_OK: u8 = 0;
/// The run failed, or lan could not start it.
pub(crate) const EXIT_FAILED: u8 = 1;
/// The invocation was wrong. clap's own code for a usage error, named here so
/// nothing else takes it and so the table in the crate docs is complete.
pub(crate) const EXIT_USAGE: u8 = 2;
/// A bound tripped, which is not the same as failing: the run stopped because
/// it reached an allowance its caller set, and kept what it had.
pub(crate) const EXIT_BOUNDED: u8 = 3;

/// The exit code a finished run earns.
///
/// A tripped bound is read first and answers on its own. Usually the run also
/// failed — a deadline or a tool budget ends it with no final message — and
/// "you ran out of the time you set" is the more useful of the two things that
/// are true (ADR-0015). A token budget is the case where they come apart: it
/// ends the run gracefully, so a run can answer *and* be bounded, and that
/// still earns [`EXIT_BOUNDED`]. The answer reached stdout either way, and a
/// script that read `0` would take a run that stopped for want of allowance as
/// one that finished its work.
pub(crate) fn exit_code<S>(report: &RunReport<S>) -> u8 {
    match report.stopped_by {
        Some(_) => EXIT_BOUNDED,
        None if report.succeeded() => EXIT_OK,
        None => EXIT_FAILED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use lan_core::RunOutcome;

    /// A report with nothing in it but the two fields the exit code reads.
    fn report(outcome: RunOutcome, stopped_by: Option<lan_core::Bound>) -> RunReport<()> {
        RunReport {
            session_id: "s1".to_string(),
            model: "gpt-5".to_string(),
            provider: "openai".to_string(),
            final_message: None,
            outcome,
            stopped_by,
            usage: lan_core::RunUsage::default(),
            sink: (),
        }
    }

    #[test]
    fn a_finished_run_exits_zero() {
        assert_eq!(exit_code(&report(RunOutcome::Ok, None)), EXIT_OK);
    }

    #[test]
    fn a_tripped_bound_is_told_apart_from_a_failure_by_the_exit_code() {
        // The whole point of the contract: `lan run --deadline 10m …; case $? in`
        // has to be able to retry a bounded run and escalate a failed one.
        let failed = report(
            RunOutcome::Error {
                message: "provider refused the request".to_string(),
            },
            None,
        );
        let bounded = report(
            RunOutcome::Error {
                message: "deadline exceeded".to_string(),
            },
            Some(lan_core::Bound::Deadline),
        );

        assert_eq!(exit_code(&failed), EXIT_FAILED);
        assert_eq!(exit_code(&bounded), EXIT_BOUNDED);
        assert_ne!(
            exit_code(&failed),
            exit_code(&bounded),
            "a shell script must be able to tell the two apart"
        );
    }

    #[test]
    fn a_run_that_answered_on_a_spent_token_budget_still_exits_bounded() {
        // The bound that ends a run gracefully: the transcript is committed and
        // the answer is real, so `outcome` is `Ok` while `stopped_by` names the
        // allowance. `Some(_)` already covers it, but only this says which
        // answer that is — and it is the one a caller of `--token-budget` sees,
        // where every other signal says the run finished.
        let answered = report(RunOutcome::Ok, Some(lan_core::Bound::TokenBudget));
        let unanswered = report(
            RunOutcome::Error {
                message: "run completed without a final assistant message".to_string(),
            },
            Some(lan_core::Bound::TokenBudget),
        );

        assert_eq!(exit_code(&answered), EXIT_BOUNDED);
        assert_eq!(
            exit_code(&unanswered),
            EXIT_BOUNDED,
            "the same bound earns the same code whether or not prose came back"
        );
    }
}
