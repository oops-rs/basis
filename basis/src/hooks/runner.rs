//! Turning both bindings' participants into one decision.
//!
//! [`HookRunner`] is the single [`ExecutionHookParticipant`] a workspace
//! registers. It walks the in-process interceptors and then the configured
//! subprocess hooks, threading any modification through the rest, and stops at
//! the first refusal.
//!
//! One runner rather than one registration per participant, even though
//! mentra's registries append: basis wants the ordering and the short-circuit
//! to be its own, so an interceptor's denial can stop a workspace hook from
//! being spawned at all. Handing mentra a list would compose the same way but
//! hand that control over with it. One *participant* rather than two
//! registrations, because mentra 0.26's mixed chain takes both seams in one
//! guard and one snapshot — see the trait impl for what that buys.
//!
//! What an answer *means* is not decided here — that is [`chain`](super::chain),
//! one implementation for both bindings. This module is two adapters: asking a
//! subprocess awaits mentra's bounded command, asking an interceptor awaits
//! the host's code, and the answers meet in the same [`Chain`].
//!
//! # The order participants speak in
//!
//! In-process interceptors first, in registration order; then host-supplied
//! subprocess hooks; then discovered hooks, global before workspace. One rule
//! underneath: **the further a participant is from the workspace's own data,
//! the earlier it speaks.** An interceptor is compiled into the embedding
//! program, a supplied hook is explicitly installed by it, a global hook
//! belongs to the person at the machine, and `.basis/hooks.json` arrived with a
//! repository that may have been cloned five minutes ago. Since the first
//! refusal short-circuits, that ordering is what lets the host's own guard stop
//! a repository-supplied program from being spawned at all — the same argument
//! that already puts global hooks before workspace ones (see [`crate::hooks`]).
//!
//! It is not a claim that a later participant is powerless. A hook still sees,
//! and can still refuse, whatever an interceptor rewrote.

use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use mentra::{
    error::RuntimeError,
    runtime::{
        AfterDecision, BeforeDecision, ExecutionHookParticipant, HookDecision,
        PostExecutionContext, PostExecutionHook, PreExecutionContext, PreExecutionHook,
        ResultDecision,
    },
    tool::ToolResultContent,
};
use serde_json::Value;
use thiserror::Error;

use crate::subprocess::{self, Completion};

use super::{
    HookEvent, HookSpec, Interceptor,
    chain::{Answer, Chain, Participant},
    contract::{HookCall, HookOutcome, HookRequest},
    wire::HookResponse,
};

/// Runs every registered interceptor and configured hook against a tool call.
#[derive(Clone)]
pub struct HookRunner {
    workspace: PathBuf,
    /// What this chain answers to when mentra names it — in a refusal the
    /// model reads, and in the attribution a rewrite carries into a later
    /// one. The workspace is in it because a shared runtime carries several of
    /// these at once and "which chain spoke" is otherwise unanswerable.
    name: String,
    interceptors: Vec<Arc<dyn Interceptor>>,
    hooks: Vec<HookSpec>,
    report: Arc<dyn Fn(&str) + Send + Sync>,
}

impl HookRunner {
    pub fn new(workspace: impl Into<PathBuf>, hooks: Vec<HookSpec>) -> Self {
        let workspace = workspace.into();

        Self {
            name: format!("basis hooks ({})", workspace.display()),
            workspace,
            interceptors: Vec::new(),
            hooks,
            report: Arc::new(|message| eprintln!("basis: {message}")),
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

    /// The subprocess hooks this runner would consult, in order.
    ///
    /// What a same-root open is compared against before it joins a live
    /// registration rather than making a second one
    /// ([`Runtime::register_hook_chain`](crate::runtime::Runtime::register_hook_chain)).
    /// The specs, not a digest of them: a [`HookSpec`] is small, `Eq`, and
    /// already the complete statement of what a participant will do.
    pub(crate) fn hooks(&self) -> &[HookSpec] {
        &self.hooks
    }

    /// Consults every applicable **subprocess hook**, in order, until one
    /// refuses.
    ///
    /// Never fails: every way a hook can go wrong ends as a [`HookOutcome`]
    /// carrying words, because an error here would reach the model as a bare
    /// blocked call with the reason thrown away.
    ///
    /// Consults everybody: in-process interceptors first, then subprocess
    /// hooks.
    ///
    /// Both are awaited on the caller's own runtime. A hook is a subprocess,
    /// and since mentra 0.24 spawning and waiting for one is an async
    /// primitive rather than blocking work, so the `spawn_blocking` this used
    /// to route hooks through has gone with the thread it needed — and with it
    /// the one thing that thread could not do: a turn cancelled mid-hook now
    /// drops the future and kills the program, where a blocking wait abandoned
    /// the thread to it. That holds on every runtime flavor, `current_thread`
    /// included.
    pub async fn decide_async(&self, call: &HookCall) -> HookOutcome {
        if self.is_empty() {
            return HookOutcome::Allow;
        }

        self.consult(HookRequest::from_call(
            HookEvent::PreToolUse,
            &self.workspace,
            call,
        ))
        .await
    }

    /// Consults everybody about a call that has already run.
    ///
    /// The after-the-call twin of [`decide_async`](Self::decide_async), down to
    /// the order and the threading: only the request differs, and the answers
    /// that request admits. `output` is the result as JSON — a text result is
    /// a JSON string, a structured one is itself — and `is_error` is the
    /// tool's own verdict, which a replacement may leave alone or overturn.
    ///
    /// Answers [`HookOutcome::Allow`] when nobody objected, which for a result
    /// means keep.
    pub async fn review_async(
        &self,
        call: &HookCall,
        output: Value,
        is_error: bool,
    ) -> HookOutcome {
        if self.is_empty() {
            return HookOutcome::Allow;
        }

        self.consult(HookRequest::from_result(
            &self.workspace,
            call,
            output,
            is_error,
        ))
        .await
    }

    /// Interceptors first, then hooks, both on the caller's runtime.
    ///
    /// One body for both events, because which of them this is is a fact about
    /// `request` — the participants, their order, and what a refusal does to
    /// the chain are the same question either side of the call.
    async fn consult(&self, request: HookRequest) -> HookOutcome {
        let chain = match self.consult_interceptors(Chain::new(request)).await {
            Ok(chain) => chain,
            Err(outcome) => return outcome,
        };

        if self.hooks.is_empty() {
            return chain.outcome();
        }

        match self.consult_hooks(chain).await {
            Ok(chain) => chain.outcome(),
            Err(outcome) => outcome,
        }
    }

    /// The in-process binding's adapter: ask, translate, fold.
    async fn consult_interceptors(&self, chain: Chain) -> Result<Chain, HookOutcome> {
        let mut chain = chain;

        for interceptor in &self.interceptors {
            let answer = match self.ask_interceptor(interceptor, chain.request()).await {
                Ok(HookOutcome::Allow) => Answer::Allow,
                Ok(HookOutcome::Deny(reason)) => Answer::Deny(Some(reason)),
                Ok(HookOutcome::Modify { input, reason }) => Answer::Modify { input, reason },
                Ok(HookOutcome::Replace {
                    output,
                    is_error,
                    reason,
                }) => Answer::Replace {
                    output,
                    is_error: Some(is_error),
                    reason,
                },
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
    async fn consult_hooks(&self, chain: Chain) -> Result<Chain, HookOutcome> {
        let mut chain = chain;

        for spec in &self.hooks {
            // Which hooks apply is a function of the event and the tool, and
            // no participant can change either — only the input, or the
            // result, moves.
            if !spec.applies_to(chain.request().event, &chain.request().tool_name) {
                continue;
            }

            let answer = match self.ask(spec, chain.request()).await {
                Ok(HookResponse::Allow { .. }) => Answer::Allow,
                Ok(HookResponse::Deny { reason }) => Answer::Deny(reason),
                Ok(HookResponse::Modify { input, reason }) => Answer::Modify { input, reason },
                Ok(HookResponse::Replace {
                    output,
                    is_error,
                    reason,
                }) => Answer::Replace {
                    output,
                    is_error,
                    reason,
                },
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
    ///
    /// Which of the trait's two questions is asked is the request's to say:
    /// the subprocess binding puts `event` on the wire and this one calls the
    /// method that goes with it.
    async fn ask_interceptor(
        &self,
        interceptor: &Arc<dyn Interceptor>,
        request: &HookRequest,
    ) -> Result<HookOutcome, String> {
        let interceptor = Arc::clone(interceptor);
        let request = request.clone();

        match tokio::spawn(async move {
            match request.event {
                HookEvent::PreToolUse => interceptor.intercept(&request).await,
                HookEvent::PostToolUse => interceptor.review(&request).await,
            }
        })
        .await
        {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(error)) => Err(format!("answered with an error: {error}")),
            Err(error) if error.is_panic() => {
                Err(format!("panicked: {}", panic_message(error.into_panic())))
            }
            Err(error) => Err(format!("could not be asked: {error}")),
        }
    }

    async fn ask(
        &self,
        spec: &HookSpec,
        request: &HookRequest,
    ) -> Result<HookResponse, HookFailure> {
        let payload = serde_json::to_string(request).map_err(HookFailure::Payload)?;

        // The baseline and nothing of basis's own: a hook is asked a question,
        // not handed a credential to act on. The tool binding is where `env`
        // belongs.
        let completion =
            subprocess::execute(&spec.command, &self.workspace, [], &payload, spec.timeout())
                .await
                .map_err(HookFailure::Spawn)?;

        // A timed-out hook carries whatever it printed before the kill, and
        // basis does not read it: a half-written answer is not an answer.
        let (code, stdout, stderr) = match completion {
            Completion::TimedOut { .. } => {
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
                stderr: subprocess::stderr_text(&stderr),
            });
        }

        // An answer the cap cut is not the answer the hook gave, and parsing
        // the kept ends with an elision marker between them would only report
        // a syntax error at the wrong place.
        if stdout.truncated() {
            return Err(HookFailure::Oversized {
                limit: subprocess::OUTPUT_CAPTURE_LIMIT,
            });
        }

        let stdout = subprocess::stdout_text(&stdout);
        if stdout.trim().is_empty() {
            return Err(HookFailure::NoAnswer);
        }

        serde_json::from_str(&stdout).map_err(|source| HookFailure::Malformed {
            output: subprocess::truncated_output(&stdout),
            source,
        })
    }
}

/// One participant, both seams — which is how basis registers a workspace's
/// chain and the shape upstream added in 0.26 for exactly this.
///
/// The two legacy impls below say the same things to mentra's two older
/// registries, and they are kept because they are the only door a
/// `mentra::test::MockRuntime` has (its builder takes a `PreExecutionHook` and
/// a `PostExecutionHook` and nothing mixed — upstream gap), and because
/// `HookRunner` is public and a host on those seams may still be using them.
/// All four methods route through [`decide_async`](HookRunner::decide_async)
/// and [`review_async`](HookRunner::review_async), so there is one chain and
/// one set of semantics however a runner is installed.
///
/// **What the mixed registration buys**, and why basis's own open uses it: one
/// [`ExecutionHookRegistration`](mentra::runtime::ExecutionHookRegistration)
/// rather than two independent guards, whose snapshot is taken once and
/// retained across both sides of a call — so a workspace cannot be consulted
/// before a tool and absent after it, which two separately dropped guards
/// permit. And a rewrite's *attribution* survives: mentra threads the reason a
/// `Modify` carried into the refusal that a rejected rewrite earns (invalid
/// JSON, a schema violation, a parallel-lane category flip), where the legacy
/// seam drops it. Threading it into a *policy* or authorizer denial is a
/// separate upstream gap (`mentra#57`), which is why a refusal of a rewritten
/// write still speaks in the policy's words and does not name the hook.
#[async_trait::async_trait]
impl ExecutionHookParticipant for HookRunner {
    fn name(&self) -> &str {
        &self.name
    }

    /// Never returns `Err`, for
    /// [`pre_tool_execution`](HookRunner::pre_tool_execution)'s reason: an
    /// error would reach the model as a blocked call with the reason thrown
    /// away.
    async fn before(&self, context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        let call = HookCall::new(
            context.agent_id.clone(),
            context.tool_name.clone(),
            context.tool_call_id.clone(),
            context.input_json.clone(),
        );

        Ok(match self.decide_async(&call).await {
            HookOutcome::Allow => BeforeDecision::Continue,
            HookOutcome::Deny(reason) => BeforeDecision::Deny(reason),
            HookOutcome::Modify { input, reason } => match serde_json::to_string(&input) {
                Ok(input_json) => BeforeDecision::Modify {
                    input_json,
                    // Every hand that touched the call, as `hooks::chain`
                    // composed it — mentra carries this into whatever refuses
                    // the rewrite later.
                    attribution: reason,
                },
                // Unreachable in practice — `input` is a `Value`, and every
                // `Value` re-encodes. Denying rather than unwrapping is what
                // keeps "a runner never panics" true by construction.
                Err(error) => BeforeDecision::Deny(format!(
                    "a replacement input could not be re-encoded: {error}"
                )),
            },
            // Unreachable: the chain refuses a replacement before the call has
            // run, so one cannot survive to here.
            HookOutcome::Replace { .. } => BeforeDecision::Deny(
                "a participant replaced the result of a call that has not run yet".to_string(),
            ),
        })
    }

    /// A refusal arrives as a `Replace` rather than an
    /// [`AfterDecision::Deny`], and deliberately: mentra prefixes a `Deny` with
    /// its own "denied by execution hook" wording, and after a tool has run the
    /// strongest thing left is *what the model reads* — which basis's chain has
    /// already worded, naming the participant that objected. Overwriting the
    /// result with that text and `is_error: true` is the same answer
    /// [`post_tool_execution`](HookRunner::post_tool_execution) gives, in the
    /// same words.
    async fn after(&self, context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        let call = HookCall::new(
            context.agent_id.clone(),
            context.tool_name.clone(),
            context.tool_call_id.clone(),
            context.input_json.clone(),
        );

        Ok(
            match self
                .review_async(&call, as_json(&context.content), context.is_error)
                .await
            {
                HookOutcome::Allow => AfterDecision::Continue,
                HookOutcome::Replace {
                    output,
                    is_error,
                    reason,
                } => AfterDecision::Replace {
                    content: as_content(output),
                    is_error: Some(is_error),
                    attribution: reason,
                },
                HookOutcome::Deny(reason) => AfterDecision::Replace {
                    content: ToolResultContent::text(reason),
                    is_error: Some(true),
                    attribution: None,
                },
                // Unreachable: the chain refuses a rewritten input once the
                // call has run.
                HookOutcome::Modify { .. } => AfterDecision::Replace {
                    content: ToolResultContent::text(
                        "a participant rewrote the input of a call that had already run",
                    ),
                    is_error: Some(true),
                    attribution: None,
                },
            },
        )
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
            // Unreachable: the chain refuses a replacement before the call has
            // run, so one cannot survive to here. Denying says which
            // impossible thing happened instead of panicking about it.
            HookOutcome::Replace { .. } => HookDecision::Deny(
                "a participant replaced the result of a call that has not run yet".to_string(),
            ),
        })
    }
}

#[async_trait::async_trait]
impl PostExecutionHook for HookRunner {
    /// Never returns `Err`, for the reason
    /// [`pre_tool_execution`](Self::pre_tool_execution) does not: an error here
    /// fails the turn, and a guard's opinion is worth more to whoever reads the
    /// transcript than a stack of runtime errors is.
    ///
    /// A refusal — a participant's `deny`, or a broken one that denies on
    /// failure — arrives as the reason in place of the output, `is_error: true`.
    /// That is the strongest thing left after a tool has run: the side effects
    /// are done, and `AgentEvent::ToolExecutionFinished` has already carried
    /// the real result to every subscriber, so what a refusal can still govern
    /// is what the model reads. A guard that broke while checking an output
    /// for credentials has not established that there were none in it.
    async fn post_tool_execution(
        &self,
        context: &PostExecutionContext,
    ) -> Result<ResultDecision, RuntimeError> {
        let call = HookCall::new(
            context.agent_id.clone(),
            context.tool_name.clone(),
            context.tool_call_id.clone(),
            // The input the tool ran with, which is half of what makes an
            // output judgeable: mentra hands over the post-`Modify` input, and
            // basis passes on what it was given.
            context.input_json.clone(),
        );

        Ok(
            match self
                .review_async(&call, as_json(&context.content), context.is_error)
                .await
            {
                HookOutcome::Allow => ResultDecision::Keep,
                HookOutcome::Replace {
                    output, is_error, ..
                } => ResultDecision::Replace {
                    content: as_content(output),
                    is_error,
                },
                HookOutcome::Deny(reason) => ResultDecision::Replace {
                    content: ToolResultContent::text(reason),
                    is_error: true,
                },
                // Unreachable: the chain refuses a rewritten input once the
                // call has run. Saying so beats a panic, and beats silently
                // keeping a result somebody meant to intervene in.
                HookOutcome::Modify { .. } => ResultDecision::Replace {
                    content: ToolResultContent::text(
                        "a participant rewrote the input of a call that had already run",
                    ),
                    is_error: true,
                },
            },
        )
    }
}

/// A tool result as the contract carries it.
///
/// Text becomes a JSON string and structured content stays itself. Nothing
/// re-parses text that happens to look like JSON: the runtime already said
/// which of the two it produced, and guessing would turn a tool that printed a
/// number into one that returned one.
fn as_json(content: &ToolResultContent) -> Value {
    match content {
        ToolResultContent::Text(text) => Value::String(text.clone()),
        ToolResultContent::Structured(value) => value.clone(),
    }
}

/// The same in reverse, so a replacement round-trips what it did not touch.
fn as_content(output: Value) -> ToolResultContent {
    match output {
        Value::String(text) => ToolResultContent::Text(text),
        other => ToolResultContent::Structured(other),
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

    #[error("printed more than the {limit} bytes basis keeps; a decision is not that long")]
    Oversized { limit: usize },

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
