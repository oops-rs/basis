//! The rules a chain of participants obeys, in one place.
//!
//! Both bindings of ADR-0012's interception contract end up here: an
//! [`Interceptor`](super::Interceptor)'s answer and a subprocess hook's answer
//! are folded into the same [`Chain`] by the same [`advance`](Chain::advance).
//! Two loops ask (they have to — one awaits a future, the other waits on a
//! process), but only one piece of code decides what an answer *means*, so the
//! properties below cannot hold for one binding and not the other:
//!
//! - **The first refusal wins**, and nothing after it is asked. Ordering
//!   therefore decides only who gets to speak, which is why basis owns it (see
//!   [`crate::hooks`]) rather than handing mentra a list.
//! - **Rewrites compose.** Each participant is asked about the call as its
//!   predecessors left it, never the original — the input before the call, the
//!   result after it.
//! - **A later participant can still deny.** `Modify` and `Replace` are not a
//!   way to route a call past someone who runs after you — the property
//!   mentra's `PreExecutionHooks::run` documents, preserved here because
//!   basis's runner, not mentra's, is where basis's ordering happens.
//! - **A rewrite basis cannot use blocks the call**, rather than falling back
//!   to the original: running the original would silently ignore a participant
//!   that believed it had intervened. An answer belonging to the other event
//!   is one of those — a `modify` after the call, a `replace` before it.
//! - **Every hand that touched the call is named** in the surviving reason.
//!
//! One [`Chain`] serves both events, because only two of those properties even
//! mention what is being rewritten. Which seam a chain is on is a fact about
//! the request it carries, not a second type.

use serde_json::Value;

use super::{
    OnFailure,
    contract::{HookEvent, HookOutcome, HookRequest},
};

/// Who is speaking, for the words that name them afterwards.
///
/// `kind` is the binding — `"hook"` or `"interceptor"` — and it is carried
/// rather than inferred so a denial says which kind of thing refused. An
/// operator reading "denied by hook 'guard'" knows to look in
/// `.basis/hooks.json`; reading "denied by interceptor 'guard'" they know to look
/// in the program.
pub(super) struct Participant<'a> {
    kind: &'static str,
    name: &'a str,
    on_failure: OnFailure,
}

impl<'a> Participant<'a> {
    /// A subprocess hook, which says for itself what a failure means.
    pub(super) fn hook(name: &'a str, on_failure: OnFailure) -> Self {
        Self {
            kind: "hook",
            name,
            on_failure,
        }
    }

    /// An in-process interceptor, which always fails closed.
    ///
    /// No per-interceptor choice, because there is nothing for one to buy: a
    /// host that would rather be ignored on failure returns
    /// [`HookOutcome::Allow`] in the code it already owns, where a hook —
    /// which cannot answer once it has crashed — needs basis to answer for it.
    pub(super) fn interceptor(name: &'a str) -> Self {
        Self {
            kind: "interceptor",
            name,
            on_failure: OnFailure::Deny,
        }
    }
}

impl std::fmt::Display for Participant<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} '{}'", self.kind, self.name)
    }
}

/// What one participant said, in the vocabulary both bindings speak.
///
/// The adapters' whole job: a `HookResponse` parsed off a subprocess's stdout
/// and a `Result<HookOutcome, _>` returned by an interceptor both become one of
/// these, and everything after that is shared.
pub(super) enum Answer {
    Allow,
    Deny(Option<String>),
    Modify {
        input: Value,
        reason: Option<String>,
    },
    /// A rewritten result, with `is_error` as the participant left it —
    /// `None` where it said nothing and the tool's own verdict stands.
    Replace {
        output: Value,
        is_error: Option<bool>,
        reason: Option<String>,
    },
    /// Could not decide, with what went wrong phrased to read after the
    /// participant's name — "hook 'guard' timed out …".
    Broken(String),
}

/// The call as the chain has left it, and who changed it.
#[derive(Debug)]
pub(super) struct Chain {
    request: HookRequest,
    /// One entry per participant that rewrote the input, in the order they did.
    modifiers: Vec<String>,
}

impl Chain {
    pub(super) fn new(request: HookRequest) -> Self {
        Self {
            request,
            modifiers: Vec::new(),
        }
    }

    /// The call as it now stands, which is what the next participant is asked
    /// about.
    pub(super) fn request(&self) -> &HookRequest {
        &self.request
    }

    /// Folds one participant's answer in.
    ///
    /// `Err` is a short-circuit: the chain is over and that outcome is the
    /// answer. `report` is called for every participant that could not decide,
    /// including one that carries on afterwards — a broken observer that costs
    /// nothing still has to be visible, or nothing would ever mention it.
    pub(super) fn advance(
        self,
        who: Participant<'_>,
        answer: Answer,
        report: &dyn Fn(&str),
    ) -> Result<Self, HookOutcome> {
        match answer {
            Answer::Allow => Ok(self),
            Answer::Deny(reason) => Err(HookOutcome::Deny(match reason {
                Some(reason) => format!("denied by {who}: {reason}"),
                None => format!("denied by {who}"),
            })),
            Answer::Modify { input, reason } => {
                // An answer for the other event is not an answer. Applying it
                // anyway would rewrite an input the tool has already been run
                // with, which changes nothing except what the trail says
                // happened.
                if self.request.event != HookEvent::PreToolUse {
                    return self.broken(
                        who,
                        "asked to rewrite the input of a call that has already run".to_string(),
                        report,
                    );
                }

                // A tool's input is an object. Anything else fails inside the
                // tool with a worse message, and running the original instead
                // would ignore a participant that believed it had intervened —
                // so it takes the failure path, like any other answer basis
                // cannot use.
                if !input.is_object() {
                    return self.broken(
                        who,
                        "asked to replace the tool input with something that is not a JSON object"
                            .to_string(),
                        report,
                    );
                }

                let modifiers = self.named(who, reason);
                Ok(Self {
                    request: self.request.with_input(input),
                    modifiers,
                })
            }
            Answer::Replace {
                output,
                is_error,
                reason,
            } => {
                if self.request.event != HookEvent::PostToolUse {
                    return self.broken(
                        who,
                        "asked to replace the result of a call that has not run yet".to_string(),
                        report,
                    );
                }

                // Any JSON is a result — a string is text, anything else is
                // structured — so there is nothing here to refuse, unlike an
                // input, which has to be an object to reach a tool at all.
                let is_error = is_error.or(self.request.is_error).unwrap_or(false);
                let modifiers = self.named(who, reason);
                Ok(Self {
                    request: self.request.with_output(output, is_error),
                    modifiers,
                })
            }
            Answer::Broken(failure) => self.broken(who, failure, report),
        }
    }

    /// This chain's modifiers with one more hand on the end.
    fn named(&self, who: Participant<'_>, reason: Option<String>) -> Vec<String> {
        let mut modifiers = self.modifiers.clone();
        modifiers.push(match reason {
            Some(reason) => format!("{who}: {reason}"),
            None => who.to_string(),
        });
        modifiers
    }

    /// What the chain decided, once everyone has spoken.
    pub(super) fn outcome(self) -> HookOutcome {
        if self.modifiers.is_empty() {
            return HookOutcome::Allow;
        }

        // Every participant that changed something, not just the last one:
        // the question an audit trail asks is who touched this call, and a
        // single name would answer it wrongly.
        let reason = Some(self.modifiers.join("; "));

        // The request says which seam this chain is on, so the two cannot
        // disagree — and `advance` has already refused every answer that
        // belonged to the other one.
        match self.request.event {
            HookEvent::PreToolUse => HookOutcome::Modify {
                input: self.request.input,
                reason,
            },
            HookEvent::PostToolUse => HookOutcome::Replace {
                // Unreachable: a post request is built from a result, and only
                // a replacement can have touched it. Saying so beats
                // unwrapping, which is what keeps "a chain never panics" true
                // by construction rather than by argument.
                output: self.request.output.unwrap_or(Value::Null),
                is_error: self.request.is_error.unwrap_or(false),
                reason,
            },
        }
    }

    /// Reports a participant that could not decide, then applies its failure
    /// mode.
    fn broken(
        self,
        who: Participant<'_>,
        failure: String,
        report: &dyn Fn(&str),
    ) -> Result<Self, HookOutcome> {
        report(&format!("{who} {failure}"));

        match who.on_failure {
            OnFailure::Deny => Err(HookOutcome::Deny(format!(
                "{who} could not answer and denies on failure: {failure}"
            ))),
            OnFailure::Allow => Ok(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Mutex};

    use serde_json::json;

    use super::*;
    use crate::hooks::{HookCall, HookEvent};

    fn chain() -> Chain {
        Chain::new(HookRequest::from_call(
            HookEvent::PreToolUse,
            Path::new("/repo"),
            &HookCall::new("agent-1", "shell", "call-1", r#"{"command":"ls"}"#),
        ))
    }

    /// The same call, after it ran and printed something it should not have.
    fn finished() -> Chain {
        Chain::new(HookRequest::from_result(
            Path::new("/repo"),
            &HookCall::new("agent-1", "shell", "call-1", r#"{"command":"cat .env"}"#),
            json!("TOKEN=hunter2"),
            false,
        ))
    }

    /// A reporter that remembers, so a test can prove a failure was announced.
    #[derive(Default)]
    struct Reports(Mutex<Vec<String>>);

    impl Reports {
        fn sink(&self) -> impl Fn(&str) + '_ {
            |message: &str| {
                self.0
                    .lock()
                    .expect("not poisoned")
                    .push(message.to_string())
            }
        }

        fn all(&self) -> Vec<String> {
            self.0.lock().expect("not poisoned").clone()
        }
    }

    fn nowhere(_message: &str) {}

    #[test]
    fn a_chain_nobody_touched_allows() {
        assert_eq!(chain().outcome(), HookOutcome::Allow);
    }

    #[test]
    fn a_refusal_names_the_binding_that_refused() {
        // Which file or which program to go and look in is the first thing
        // whoever reads this needs.
        let hook = chain()
            .advance(
                Participant::hook("guard", OnFailure::Deny),
                Answer::Deny(Some("not here".to_string())),
                &nowhere,
            )
            .expect_err("denied");
        let interceptor = chain()
            .advance(
                Participant::interceptor("guard"),
                Answer::Deny(None),
                &nowhere,
            )
            .expect_err("denied");

        assert_eq!(
            hook,
            HookOutcome::Deny("denied by hook 'guard': not here".to_string())
        );
        assert_eq!(
            interceptor,
            HookOutcome::Deny("denied by interceptor 'guard'".to_string())
        );
    }

    #[test]
    fn a_modification_is_what_the_next_participant_sees() {
        let chain = chain()
            .advance(
                Participant::interceptor("redact"),
                Answer::Modify {
                    input: json!({"command": "deploy --token REDACTED"}),
                    reason: Some("stripped a credential".to_string()),
                },
                &nowhere,
            )
            .expect("allowed on");

        assert_eq!(chain.request().input["command"], "deploy --token REDACTED");
        assert_eq!(
            chain.outcome(),
            HookOutcome::Modify {
                input: json!({"command": "deploy --token REDACTED"}),
                reason: Some("interceptor 'redact': stripped a credential".to_string()),
            }
        );
    }

    #[test]
    fn every_hand_that_touched_the_call_is_named_in_order() {
        let outcome = chain()
            .advance(
                Participant::interceptor("first"),
                Answer::Modify {
                    input: json!({"command": "once"}),
                    reason: None,
                },
                &nowhere,
            )
            .expect("allowed on")
            .advance(
                Participant::hook("second", OnFailure::Deny),
                Answer::Modify {
                    input: json!({"command": "twice"}),
                    reason: Some("narrowed".to_string()),
                },
                &nowhere,
            )
            .expect("allowed on")
            .outcome();

        assert_eq!(
            outcome,
            HookOutcome::Modify {
                input: json!({"command": "twice"}),
                reason: Some("interceptor 'first'; hook 'second': narrowed".to_string()),
            }
        );
    }

    #[test]
    fn a_modification_cannot_smuggle_a_call_past_a_later_guard() {
        let outcome = chain()
            .advance(
                Participant::interceptor("rewriter"),
                Answer::Modify {
                    input: json!({"command": "sneaky"}),
                    reason: None,
                },
                &nowhere,
            )
            .expect("allowed on")
            .advance(
                Participant::hook("guard", OnFailure::Deny),
                Answer::Deny(Some("still no".to_string())),
                &nowhere,
            )
            .expect_err("denied");

        assert_eq!(
            outcome,
            HookOutcome::Deny("denied by hook 'guard': still no".to_string())
        );
    }

    #[test]
    fn a_replacement_that_is_not_an_object_is_refused_whichever_binding_sent_it() {
        for who in [
            Participant::hook("confused", OnFailure::Deny),
            Participant::interceptor("confused"),
        ] {
            let reports = Reports::default();

            let outcome = chain()
                .advance(
                    who,
                    Answer::Modify {
                        input: json!("ls -l"),
                        reason: None,
                    },
                    &reports.sink(),
                )
                .expect_err("denied");

            let HookOutcome::Deny(reason) = outcome else {
                panic!("expected a denial");
            };
            assert!(reason.contains("not a JSON object"), "got {reason}");
            assert_eq!(reports.all().len(), 1);
        }
    }

    #[test]
    fn a_chain_nobody_touched_keeps_the_result() {
        assert_eq!(finished().outcome(), HookOutcome::Allow);
    }

    #[test]
    fn replacements_compose_and_the_last_one_is_what_the_model_sees() {
        let outcome = finished()
            .advance(
                Participant::interceptor("redact"),
                Answer::Replace {
                    output: json!("TOKEN=[redacted]"),
                    is_error: None,
                    reason: Some("a credential".to_string()),
                },
                &nowhere,
            )
            .expect("carries on")
            .advance(
                Participant::hook("annotate", OnFailure::Deny),
                Answer::Replace {
                    output: json!("TOKEN=[redacted] (this file is off limits)"),
                    is_error: Some(true),
                    reason: None,
                },
                &nowhere,
            )
            .expect("carries on")
            .outcome();

        assert_eq!(
            outcome,
            HookOutcome::Replace {
                output: json!("TOKEN=[redacted] (this file is off limits)"),
                is_error: true,
                reason: Some("interceptor 'redact': a credential; hook 'annotate'".to_string()),
            }
        );
    }

    #[test]
    fn a_replacement_is_what_the_next_participant_is_shown() {
        let chain = finished()
            .advance(
                Participant::interceptor("redact"),
                Answer::Replace {
                    output: json!("[redacted]"),
                    is_error: None,
                    reason: None,
                },
                &nowhere,
            )
            .expect("carries on");

        assert_eq!(chain.request().output, Some(json!("[redacted]")));
        assert_eq!(
            chain.request().is_error,
            Some(false),
            "saying nothing about failure must leave the tool's own verdict standing"
        );
    }

    #[test]
    fn a_decision_that_does_not_fit_the_event_is_not_an_answer() {
        // Reinterpreting either one would hand a participant a power it did
        // not ask for: rewriting an input that has already been used, or a
        // result that does not exist yet.
        for (chain, answer, expected) in [
            (
                finished(),
                Answer::Modify {
                    input: json!({"command": "ls"}),
                    reason: None,
                },
                "already run",
            ),
            (
                chain(),
                Answer::Replace {
                    output: json!("made up"),
                    is_error: None,
                    reason: None,
                },
                "has not run",
            ),
        ] {
            let reports = Reports::default();

            let outcome = chain
                .advance(
                    Participant::hook("confused", OnFailure::Deny),
                    answer,
                    &reports.sink(),
                )
                .expect_err("denied");

            let HookOutcome::Deny(reason) = outcome else {
                panic!("expected a denial");
            };
            assert!(reason.contains(expected), "got {reason}");
            assert_eq!(reports.all().len(), 1);
        }
    }

    #[test]
    fn a_participant_can_still_refuse_a_result_after_one_rewrote_it() {
        let outcome = finished()
            .advance(
                Participant::interceptor("redact"),
                Answer::Replace {
                    output: json!("[redacted]"),
                    is_error: None,
                    reason: None,
                },
                &nowhere,
            )
            .expect("carries on")
            .advance(
                Participant::hook("guard", OnFailure::Deny),
                Answer::Deny(Some("that file is off limits".to_string())),
                &nowhere,
            )
            .expect_err("denied");

        assert_eq!(
            outcome,
            HookOutcome::Deny("denied by hook 'guard': that file is off limits".to_string())
        );
    }

    #[test]
    fn an_interceptor_that_breaks_denies_and_says_so() {
        let reports = Reports::default();

        let outcome = chain()
            .advance(
                Participant::interceptor("vault"),
                Answer::Broken("answered with an error: unreachable".to_string()),
                &reports.sink(),
            )
            .expect_err("denied");

        assert_eq!(
            outcome,
            HookOutcome::Deny(
                "interceptor 'vault' could not answer and denies on failure: answered with an \
                 error: unreachable"
                    .to_string()
            )
        );
        assert_eq!(
            reports.all(),
            vec!["interceptor 'vault' answered with an error: unreachable"],
            "a broken guard must be announced, not only reported to the model"
        );
    }

    #[test]
    fn a_participant_that_chose_to_fail_open_is_still_announced() {
        let reports = Reports::default();

        let chain = chain()
            .advance(
                Participant::hook("logger", OnFailure::Allow),
                Answer::Broken("exited with code 1".to_string()),
                &reports.sink(),
            )
            .expect("carries on");

        assert_eq!(chain.outcome(), HookOutcome::Allow);
        assert_eq!(
            reports.all().len(),
            1,
            "carrying on is not the same as staying quiet"
        );
    }
}
