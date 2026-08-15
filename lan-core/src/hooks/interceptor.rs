//! The in-process binding of the interception contract.
//!
//! ADR-0012 gives the interception seam one contract and two bindings: an
//! embedder implements the trait, or a workspace declares a subprocess in
//! `.lan/hooks.json`. This module is the first of those. Its sibling is the
//! whole rest of [`crate::hooks`] — [`wire`](super::wire) for what a subprocess
//! is told, [`HookSpec`](super::HookSpec) for how one is declared — and both
//! arrive at [`HookRunner`](super::HookRunner), which owns the ordering.
//!
//! It exists because a binding lan did not have was a power lan did not offer.
//! Redacting a credential out of a tool's input needs the host's own code —
//! the vault handle, the token it just minted, the regex it keeps in a config
//! struct — and until now the only way to get code onto this seam was to spawn
//! a process and hand it the tool call on stdin. That is the right answer for a
//! guard a repository ships and the wrong one for a guard the embedding program
//! *is*.
//!
//! ```no_run
//! use lan_core::{
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
//! # async fn example() -> Result<(), lan_core::RunError> {
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
/// Boxed rather than an enum of lan's own, because lan has nothing to say about
/// it: whatever went wrong happened inside the host's code, against the host's
/// dependencies. A box lets `?` carry any of them out, and the only thing lan
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
}

#[async_trait::async_trait]
impl<T: Interceptor + ?Sized> Interceptor for Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        (**self).intercept(call).await
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
                // leaves through `?` with no lan-shaped conversion.
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
