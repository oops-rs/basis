//! Turning both bindings' participants into one decision.
//!
//! [`HookRunner`] is the single [`PreExecutionHook`] lan registers. It walks the
//! in-process interceptors and then the configured subprocess hooks, threading
//! any modification through the rest, and stops at the first refusal.
//!
//! One runner rather than one registration per participant, even though
//! `RuntimeBuilder::with_pre_hook` appends: lan wants the ordering and the
//! short-circuit to be its own, so an interceptor's denial can stop a workspace
//! hook from being spawned at all. Handing mentra a list would compose the same
//! way but hand that control over with it.
//!
//! What an answer *means* is not decided here — that is [`chain`](super::chain),
//! one implementation for both bindings. This module is two adapters and a
//! thread: asking a subprocess blocks, asking an interceptor awaits, and the
//! answers meet in the same [`Chain`].
//!
//! # The order participants speak in
//!
//! In-process interceptors first, in registration order; then hooks, global
//! before workspace. One rule underneath: **the further a participant is from
//! the workspace's own data, the earlier it speaks.** An interceptor is
//! compiled into the embedding program, a global hook belongs to the person at
//! the machine, and `.lan/hooks.json` arrived with a repository that may have
//! been cloned five minutes ago. Since the first refusal short-circuits, that
//! ordering is what lets the host's own guard stop a repository-supplied
//! program from being spawned at all — the same argument that already puts
//! global hooks before workspace ones (see [`crate::hooks`]).
//!
//! It is not a claim that a later participant is powerless. A hook still sees,
//! and can still refuse, whatever an interceptor rewrote.

use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use mentra::{
    error::RuntimeError,
    runtime::{HookDecision, PreExecutionContext, PreExecutionHook},
};
use thiserror::Error;

use super::{
    HookEvent, HookSpec, Interceptor,
    chain::{Answer, Chain, Participant},
    contract::{HookCall, HookOutcome, HookRequest},
    exec::{self, Completion},
    wire::HookResponse,
};

/// Runs every registered interceptor and configured hook against a tool call.
#[derive(Clone)]
pub struct HookRunner {
    workspace: PathBuf,
    interceptors: Vec<Arc<dyn Interceptor>>,
    hooks: Vec<HookSpec>,
    report: Arc<dyn Fn(&str) + Send + Sync>,
}

impl HookRunner {
    pub fn new(workspace: impl Into<PathBuf>, hooks: Vec<HookSpec>) -> Self {
        Self {
            workspace: workspace.into(),
            interceptors: Vec::new(),
            hooks,
            report: Arc::new(|message| eprintln!("lan: {message}")),
        }
    }

    /// Adds an in-process participant, after any already registered.
    ///
    /// Appends rather than replaces, and the order of the calls is the order
    /// they are consulted in — see the module docs for where that sits relative
    /// to subprocess hooks, and why.
    ///
    /// [`RuntimeBuilder::with_interceptor`](crate::RuntimeBuilder::with_interceptor)
    /// is how a host normally reaches this; the constructor is here for a host
    /// building a runner for a runtime of its own.
    pub fn with_interceptor(self, interceptor: impl Interceptor + 'static) -> Self {
        Self {
            interceptors: {
                let mut interceptors = self.interceptors;
                interceptors.push(Arc::new(interceptor));
                interceptors
            },
            ..self
        }
    }

    /// Redirects failure reports somewhere other than stderr.
    ///
    /// A broken participant is an operator's problem, not the model's, so it is
    /// said out loud by default — including when
    /// [`OnFailure::Allow`](super::OnFailure::Allow) means the turn carries on,
    /// which is the case where nothing else would ever mention it. A host that
    /// owns its own logging replaces the destination; it cannot remove it.
    pub fn with_reporter(self, report: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self {
            report: Arc::new(report),
            ..self
        }
    }

    /// Whether this runner would consult anybody at all.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty() && self.interceptors.is_empty()
    }

    /// Consults every applicable **subprocess hook**, in order, until one
    /// refuses.
    ///
    /// Never fails: every way a hook can go wrong ends as a [`HookOutcome`]
    /// carrying words, because an error here would reach the model as a bare
    /// blocked call with the reason thrown away.
    ///
    /// Blocking: it spawns subprocesses and waits for them. Callers on an async
    /// runtime should reach it through [`decide_async`](Self::decide_async)
    /// rather than calling it directly.
    ///
    /// A runner with interceptors registered **denies here rather than
    /// deciding**. An [`Interceptor`] is async by contract and there is nowhere
    /// in a synchronous call to await one; skipping them would silently drop a
    /// control the host believes is in place, which is the one failure this
    /// whole module is arranged to avoid.
    pub fn decide(&self, call: &HookCall) -> HookOutcome {
        if !self.interceptors.is_empty() {
            return HookOutcome::Deny(
                "in-process interceptors are registered and cannot be consulted synchronously; \
                 this call belongs on HookRunner::decide_async"
                    .to_string(),
            );
        }

        if self.hooks.is_empty() {
            return HookOutcome::Allow;
        }

        match self.consult_hooks(Chain::new(self.request(call))) {
            Ok(chain) => chain.outcome(),
            Err(outcome) => outcome,
        }
    }

    /// Consults everybody: interceptors first, then hooks.
    ///
    /// Hooks are subprocesses, so asking them blocks for as long as they take.
    /// `spawn_blocking` puts that on a thread meant for it, which works on
    /// every runtime flavor — the previous `block_in_place` dance existed only
    /// because mentra's hook trait was synchronous and there was nowhere else
    /// to put the wait (oops-rs/mentra#16, fixed in 0.16). Interceptors are
    /// awaited instead, on the caller's own runtime, because they are async by
    /// contract and a blocking thread is exactly where a future cannot go.
    pub async fn decide_async(&self, call: &HookCall) -> HookOutcome {
        if self.is_empty() {
            return HookOutcome::Allow;
        }

        let chain = match self
            .consult_interceptors(Chain::new(self.request(call)))
            .await
        {
            Ok(chain) => chain,
            Err(outcome) => return outcome,
        };

        if self.hooks.is_empty() {
            return chain.outcome();
        }

        let runner = self.clone();

        match tokio::task::spawn_blocking(move || runner.consult_hooks(chain)).await {
            Ok(Ok(chain)) => chain.outcome(),
            Ok(Err(outcome)) => outcome,
            // The blocking task panicked, which a hook cannot cause — every
            // failure inside `consult_hooks` is already an outcome. Denying
            // keeps "a broken guard never silently allows" true even here.
            Err(error) => HookOutcome::Deny(format!("hook runner failed: {error}")),
        }
    }

    fn request(&self, call: &HookCall) -> HookRequest {
        HookRequest::from_call(HookEvent::PreToolUse, &self.workspace, call)
    }

    /// The in-process binding's adapter: ask, translate, fold.
    async fn consult_interceptors(&self, chain: Chain) -> Result<Chain, HookOutcome> {
        let mut chain = chain;

        for interceptor in &self.interceptors {
            let answer = match self.ask_interceptor(interceptor, chain.request()).await {
                Ok(HookOutcome::Allow) => Answer::Allow,
                Ok(HookOutcome::Deny(reason)) => Answer::Deny(Some(reason)),
                Ok(HookOutcome::Modify { input, reason }) => Answer::Modify { input, reason },
                Err(failure) => Answer::Broken(failure),
            };

            chain = chain.advance(
                Participant::interceptor(interceptor.name()),
                answer,
                &*self.report,
            )?;
        }

        Ok(chain)
    }

    /// The subprocess binding's adapter: spawn, parse, fold.
    fn consult_hooks(&self, chain: Chain) -> Result<Chain, HookOutcome> {
        let mut chain = chain;

        for spec in &self.hooks {
            // Which hooks apply is a function of the tool, and no participant
            // can change that — only the input moves.
            if !spec.applies_to(chain.request().event, &chain.request().tool_name) {
                continue;
            }

            let answer = match self.ask(spec, chain.request()) {
                Ok(HookResponse::Allow { .. }) => Answer::Allow,
                Ok(HookResponse::Deny { reason }) => Answer::Deny(reason),
                Ok(HookResponse::Modify { input, reason }) => Answer::Modify { input, reason },
                Err(failure) => Answer::Broken(failure.to_string()),
            };

            chain = chain.advance(
                Participant::hook(&spec.name, spec.on_failure),
                answer,
                &*self.report,
            )?;
        }

        Ok(chain)
    }

    /// Puts the call to one interceptor, on a task of its own.
    ///
    /// The task is what turns a panic into a denial rather than into a lost
    /// turn — the same trick [`decide_async`](Self::decide_async) already
    /// relies on for the blocking half. It costs the interceptor the ability to
    /// be cancelled with the turn, which a check answering in milliseconds does
    /// not need.
    async fn ask_interceptor(
        &self,
        interceptor: &Arc<dyn Interceptor>,
        request: &HookRequest,
    ) -> Result<HookOutcome, String> {
        let interceptor = Arc::clone(interceptor);
        let request = request.clone();

        match tokio::spawn(async move { interceptor.intercept(&request).await }).await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(error)) => Err(format!("answered with an error: {error}")),
            Err(error) if error.is_panic() => {
                Err(format!("panicked: {}", panic_message(error.into_panic())))
            }
            Err(error) => Err(format!("could not be asked: {error}")),
        }
    }

    fn ask(&self, spec: &HookSpec, request: &HookRequest) -> Result<HookResponse, HookFailure> {
        let payload = serde_json::to_string(request).map_err(HookFailure::Payload)?;

        let completion = exec::execute(&spec.command, &self.workspace, &payload, spec.timeout())
            .map_err(HookFailure::Spawn)?;

        let (code, stdout, stderr) = match completion {
            Completion::TimedOut => {
                return Err(HookFailure::TimedOut {
                    timeout: spec.timeout(),
                });
            }
            Completion::Exited {
                code,
                stdout,
                stderr,
            } => (code, stdout, stderr),
        };

        // The exit code is checked before the output is read: a hook that
        // crashed after printing has not decided anything.
        if code != Some(0) {
            return Err(HookFailure::Exited {
                code: code.map_or_else(|| "a signal".to_string(), |code| format!("code {code}")),
                stderr,
            });
        }

        if stdout.trim().is_empty() {
            return Err(HookFailure::NoAnswer);
        }

        serde_json::from_str(&stdout).map_err(|source| HookFailure::Malformed {
            output: exec::truncated_output(&stdout),
            source,
        })
    }
}

#[async_trait::async_trait]
impl PreExecutionHook for HookRunner {
    /// Never returns `Err`.
    ///
    /// mentra turns a hook error into a bare blocked-tool result, which throws
    /// the reason away; every outcome here is a [`HookDecision`] instead, so
    /// whatever happened reaches both the model and the audit trail as words.
    async fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        let call = HookCall::new(
            context.agent_id.clone(),
            context.tool_name.clone(),
            context.tool_call_id.clone(),
            context.input_json.clone(),
        );

        Ok(match self.decide_async(&call).await {
            HookOutcome::Allow => HookDecision::Allow,
            HookOutcome::Deny(reason) => HookDecision::Deny(reason),
            HookOutcome::Modify { input, reason } => match serde_json::to_string(&input) {
                Ok(input_json) => HookDecision::Modify { input_json, reason },
                // Unreachable in practice — `input` is a `Value`, and every
                // `Value` re-encodes. Denying rather than unwrapping is what
                // keeps "a runner never panics" true by construction.
                Err(error) => HookDecision::Deny(format!(
                    "a replacement input could not be re-encoded: {error}"
                )),
            },
        })
    }
}

impl fmt::Debug for HookRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookRunner")
            .field("workspace", &self.workspace)
            .field(
                "interceptors",
                &self
                    .interceptors
                    .iter()
                    .map(|interceptor| interceptor.name())
                    .collect::<Vec<_>>(),
            )
            .field(
                "hooks",
                &self.hooks.iter().map(|spec| &spec.name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// Whatever a panicking interceptor was panicking about.
///
/// A panic payload is `Any`, and the two shapes `panic!` produces are the two
/// handled here. Anything else is a payload nobody can read, so it is named
/// rather than guessed at.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }

    "with a payload that is not a message".to_string()
}

/// Why a hook did not produce a decision.
///
/// Phrased to read after the hook's name — "hook 'guard' timed out …" — because
/// that is where these end up, in a denial the model reads and in a report the
/// operator does.
#[derive(Debug, Error)]
enum HookFailure {
    #[error("could not be started: {0}")]
    Spawn(#[source] std::io::Error),

    #[error("did not answer within {}ms and was killed", .timeout.as_millis())]
    TimedOut { timeout: Duration },

    #[error("exited with {code}{}", stderr_tail(.stderr))]
    Exited { code: String, stderr: String },

    #[error("printed nothing; a hook answers with a JSON decision on stdout")]
    NoAnswer,

    #[error("printed something that is not a decision ({source}): {output}")]
    Malformed {
        output: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("could not be asked, because the request would not serialize: {0}")]
    Payload(#[source] serde_json::Error),
}

fn stderr_tail(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!(" and said: {stderr}")
    }
}

#[cfg(all(test, unix))]
mod tests;
