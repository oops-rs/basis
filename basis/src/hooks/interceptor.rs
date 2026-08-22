//! The in-process binding of the interception contract.
//!
//! ADR-0012 gives the interception seam one contract and two bindings: an
//! embedder implements the trait, or a workspace declares a subprocess in
//! `.basis/hooks.json`. This module is the first of those. Its sibling is the
//! whole rest of [`crate::hooks`] — [`wire`](super::wire) for what a subprocess
//! is told, [`HookSpec`](super::HookSpec) for how one is declared — and both
//! arrive at [`HookRunner`](super::HookRunner), which owns the ordering.
//!
//! It exists because a binding basis did not have was a power basis did not offer.
//! Redacting a credential out of a tool's input needs the host's own code —
//! the vault handle, the token it just minted, the regex it keeps in a config
//! struct — and until now the only way to get code onto this seam was to spawn
//! a process and hand it the tool call on stdin. That is the right answer for a
//! guard a repository ships and the wrong one for a guard the embedding program
//! *is*.
//!
//! ```no_run
//! use basis::{
//!     HookOutcome, HookRequest, Interceptor, InterceptorError, Runtime, Workspace, async_trait,
//! };
//!
//! struct Redact;
//!
//! #[async_trait]
//! impl Interceptor for Redact {
//!     fn name(&self) -> &str {
//!         "redact"
//!     }
//!
//!     async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
//!         let Some(command) = call.input.get("command").and_then(|value| value.as_str()) else {
//!             return Ok(HookOutcome::Allow);
//!         };
//!         if !command.contains("--token") {
//!             return Ok(HookOutcome::Allow);
//!         }
//!
//!         Ok(HookOutcome::Modify {
//!             input: serde_json::json!({"command": "deploy --token REDACTED"}),
//!             reason: Some("stripped a credential".to_string()),
//!         })
//!     }
//! }
//!
//! # async fn example() -> Result<(), basis::RunError> {
//! // Host scope is runtime scope (ADR-0018): the guard registers on the
//! // runtime the workspaces share — or, as here, on the private one this
//! // workspace's open builds from the recipe.
//! let workspace = Workspace::builder("/repo")
//!     .with_runtime_builder(Runtime::builder().with_interceptor(Redact))
//!     .open()
//!     .await?;
//! # let _ = workspace;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use super::contract::{HookOutcome, HookRequest};

/// Why an [`Interceptor`] could not decide.
///
/// Boxed rather than an enum of basis's own, because basis has nothing to say about
/// it: whatever went wrong happened inside the host's code, against the host's
/// dependencies. A box lets `?` carry any of them out, and the only thing basis
/// does with one is print it in the denial.
pub type InterceptorError = Box<dyn std::error::Error + Send + Sync>;

/// Gets a say over each tool call, in the embedding program's own process.
///
/// The in-process binding of ADR-0012's interception contract, and the sibling
/// of the subprocess hooks in [`crate::hooks`]: same vocabulary
/// ([`HookOutcome`]), same request ([`HookRequest`]), same chain. What differs
/// is only who is speaking — the host's compiled code rather than a program
/// named in a file.
///
/// Two questions, one trait: [`intercept`](Self::intercept) before the call and
/// [`review`](Self::review) after it. The second is defaulted to no objection,
/// so an interceptor written when there was only the first still compiles and
/// still means what it said.
///
/// The other seam is [`Approver`](crate::approval::Approver), and the two are
/// deliberately not merged (ADR-0012, and mentra keeps them apart for the same
/// reason). An approver answers *may this happen* and its answer feeds the
/// permission machinery a person drives; an interceptor answers *may this
/// happen, in this form* and composes with every other interceptor and hook. A
/// host that wants to ask a person wants an approver; a host that wants to
/// rewrite an argument wants this.
///
/// Async because mentra's own hook trait is, and for the reason it gives: a
/// participant that reads a file, asks a service, or takes a lock would
/// otherwise block a runtime worker for its whole duration. The attribute to
/// spell that with is re-exported at the crate root —
/// [`async_trait`](crate::async_trait) — so writing an impl costs no manifest
/// line of the host's own.
///
/// # Fail closed
///
/// **An interceptor that cannot answer denies.** An `Err` denies, and so does a
/// panic — the call is put to the interceptor on its own task so that a panic
/// is caught rather than taking the turn with it. Either way the reason names
/// this interceptor and says what happened, and the failure is reported through
/// [`HookRunner::with_reporter`](super::HookRunner::with_reporter).
///
/// This is the same rule [`OnFailure`](super::OnFailure) states for hooks, and
/// the same asymmetry justifies it: failing open on a broken guard silently
/// removes a control someone believes is in place, while failing closed on a
/// broken observer is loud and gets fixed. There is no `OnFailure::Allow`
/// equivalent here, and none is needed — an interceptor that would rather be
/// ignored is one `Ok(HookOutcome::Allow)` away from saying so, in code it
/// already owns.
///
/// After the call the same rule holds, and denying means what it can still
/// mean there: the model is shown the reason instead of the output. A guard
/// that broke while checking a result for credentials has not established that
/// there were none in it.
#[async_trait::async_trait]
pub trait Interceptor: Send + Sync {
    /// Names this interceptor in denials and in the audit trail.
    ///
    /// Required rather than defaulted, and for the reason a
    /// [`HookSpec`](super::HookSpec) must carry a name: a chain's whole output
    /// is *who* said what, and "an interceptor denied this" is not an answer
    /// anybody can act on.
    fn name(&self) -> &str;

    /// Decides about one tool call.
    ///
    /// `call` is the call as the chain has left it — every earlier
    /// participant's modification is already applied, so an interceptor never
    /// judges an input that has since been rewritten.
    async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError>;

    /// Decides about one tool call's result, once it has run.
    ///
    /// `result` is the same [`HookRequest`], with `output` and `is_error` on
    /// it and `input` holding what the tool actually ran with. As with
    /// [`intercept`](Self::intercept), it is the result as the chain has left
    /// it, so no interceptor judges an output a later one has already replaced.
    ///
    /// The answers are [`HookOutcome::Allow`] to let the result stand,
    /// [`HookOutcome::Replace`] to show the model something else, and
    /// [`HookOutcome::Deny`] to show it the reason instead, marked as an
    /// error. Nothing here can un-run the tool: the side effects have happened
    /// and the event stream already carries what it returned. What is being
    /// decided is only what the model reads.
    ///
    /// Defaulted to `Allow`, so an interceptor that only guards calls says
    /// nothing about results and is not made to. A guard that wants both
    /// writes both — the two questions have different answers often enough
    /// that folding them into one method would mean asking every host to
    /// branch on `result.output.is_some()`.
    async fn review(&self, result: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        let _ = result;
        Ok(HookOutcome::Allow)
    }
}

/// Forwards to the interceptor inside.
///
/// Lets a host hold an interceptor it chose at runtime — one of several, or one
/// a feature flag picked — and still hand it to anything taking
/// `impl Interceptor`. The same courtesy [`Approver`](crate::approval::Approver)
/// gets, and mentra's own hook trait.
#[async_trait::async_trait]
impl<T: Interceptor + ?Sized> Interceptor for Box<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        (**self).intercept(call).await
    }

    // Forwarded rather than inherited: taking the default here would answer
    // "no objection" on behalf of an interceptor that had one, and do it
    // silently, only for hosts that hold theirs behind a box.
    async fn review(&self, result: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        (**self).review(result).await
    }
}

#[async_trait::async_trait]
impl<T: Interceptor + ?Sized> Interceptor for Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        (**self).intercept(call).await
    }

    async fn review(&self, result: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        (**self).review(result).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::hooks::{HookCall, HookEvent};

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl Interceptor for Named {
        fn name(&self) -> &str {
            self.0
        }

        async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
            Ok(HookOutcome::Deny(call.tool_name.clone()))
        }
    }

    fn request() -> HookRequest {
        HookRequest::from_call(
            HookEvent::PreToolUse,
            Path::new("/repo"),
            &HookCall::new("agent-1", "shell", "call-1", r#"{"command":"ls"}"#),
        )
    }

    fn result() -> HookRequest {
        HookRequest::from_result(
            Path::new("/repo"),
            &HookCall::new("agent-1", "shell", "call-1", r#"{"command":"cat .env"}"#),
            serde_json::json!("TOKEN=hunter2"),
            false,
        )
    }

    #[tokio::test]
    async fn an_indirected_interceptor_answers_exactly_as_the_one_inside() {
        // What a host relies on to choose between several without writing the
        // registration out once per arm.
        let boxed: Box<dyn Interceptor> = Box::new(Named("boxed"));
        let shared: Arc<dyn Interceptor> = Arc::new(Named("shared"));

        assert_eq!(boxed.name(), "boxed");
        assert_eq!(shared.name(), "shared");
        assert_eq!(
            boxed.intercept(&request()).await.expect("answers"),
            HookOutcome::Deny("shell".to_string())
        );
        assert_eq!(
            shared.intercept(&request()).await.expect("answers"),
            HookOutcome::Deny("shell".to_string())
        );
    }

    #[tokio::test]
    async fn an_interceptor_that_only_guards_calls_keeps_every_result() {
        // What every impl written before there was a second seam does, and
        // the reason the method is defaulted: a host that never asked to see
        // results is not made to answer about them.
        assert_eq!(
            Named("guard").review(&result()).await.expect("answers"),
            HookOutcome::Allow
        );
    }

    #[tokio::test]
    async fn an_indirection_forwards_a_review_too() {
        // A `Box` that inherited the default would swallow the inner
        // interceptor's opinion and report no objection — the one failure
        // this whole module is arranged to avoid.
        struct Redacts;

        #[async_trait::async_trait]
        impl Interceptor for Redacts {
            fn name(&self) -> &str {
                "redacts"
            }

            async fn intercept(
                &self,
                _call: &HookRequest,
            ) -> Result<HookOutcome, InterceptorError> {
                Ok(HookOutcome::Allow)
            }

            async fn review(&self, result: &HookRequest) -> Result<HookOutcome, InterceptorError> {
                Ok(HookOutcome::Replace {
                    output: serde_json::json!("[redacted]"),
                    is_error: result.is_error.unwrap_or(false),
                    reason: Some("a credential".to_string()),
                })
            }
        }

        let boxed: Box<dyn Interceptor> = Box::new(Redacts);
        let shared: Arc<dyn Interceptor> = Arc::new(Redacts);

        for indirected in [&boxed as &dyn Interceptor, &shared as &dyn Interceptor] {
            assert_eq!(
                indirected.review(&result()).await.expect("answers"),
                HookOutcome::Replace {
                    output: serde_json::json!("[redacted]"),
                    is_error: false,
                    reason: Some("a credential".to_string()),
                }
            );
        }
    }

    #[tokio::test]
    async fn a_review_is_shown_the_output_as_well_as_the_input() {
        struct SeesOutput;

        #[async_trait::async_trait]
        impl Interceptor for SeesOutput {
            fn name(&self) -> &str {
                "sees-output"
            }

            async fn intercept(
                &self,
                _call: &HookRequest,
            ) -> Result<HookOutcome, InterceptorError> {
                Ok(HookOutcome::Allow)
            }

            async fn review(&self, result: &HookRequest) -> Result<HookOutcome, InterceptorError> {
                // The input is what the tool *ran* with, which is half of
                // what makes an output judgeable.
                Ok(HookOutcome::Deny(format!(
                    "{} -> {}",
                    result.input["command"],
                    result.output.clone().unwrap_or_default()
                )))
            }
        }

        assert_eq!(
            SeesOutput.review(&result()).await.expect("answers"),
            HookOutcome::Deny("\"cat .env\" -> \"TOKEN=hunter2\"".to_string())
        );
    }

    #[tokio::test]
    async fn an_interceptor_is_asked_about_the_call_as_it_now_stands() {
        struct SeesInput;

        #[async_trait::async_trait]
        impl Interceptor for SeesInput {
            fn name(&self) -> &str {
                "sees-input"
            }

            async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
                // Parsed, not a string to decode: the whole reason an
                // in-process binding is worth having.
                Ok(HookOutcome::Deny(call.input["command"].to_string()))
            }
        }

        assert_eq!(
            SeesInput.intercept(&request()).await.expect("answers"),
            HookOutcome::Deny("\"ls\"".to_string())
        );
    }

    #[tokio::test]
    async fn any_error_a_host_has_can_be_carried_out() {
        struct Fails;

        #[async_trait::async_trait]
        impl Interceptor for Fails {
            fn name(&self) -> &str {
                "fails"
            }

            async fn intercept(
                &self,
                _call: &HookRequest,
            ) -> Result<HookOutcome, InterceptorError> {
                // The point of the boxed error: a host's own failure type
                // leaves through `?` with no basis-shaped conversion.
                Err(std::io::Error::other("the vault is unreachable"))?
            }
        }

        assert_eq!(
            Fails
                .intercept(&request())
                .await
                .expect_err("fails")
                .to_string(),
            "the vault is unreachable"
        );
    }
}
