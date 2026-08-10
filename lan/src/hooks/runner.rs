//! Turning configured commands into one decision.
//!
//! [`HookRunner`] is the single [`PreExecutionHook`] lan registers. It walks
//! the configured hooks in order, spawns the ones that apply, threads any
//! modification through the rest, and stops at the first refusal.
//!
//! One runner rather than one registration per hook, even though
//! `RuntimeBuilder::with_pre_hook` appends: lan wants the ordering and the
//! short-circuit to be its own, so a global hook's denial can stop a workspace
//! hook from being spawned at all. Handing mentra a list would compose the same
//! way but hand that control over with it.

use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use mentra::{
    error::RuntimeError,
    runtime::{HookDecision, PreExecutionContext, PreExecutionHook},
};
use serde_json::Value;
use thiserror::Error;
use tokio::runtime::{Handle, RuntimeFlavor};

use super::{
    HookEvent, HookSpec, OnFailure,
    exec::{self, Completion},
    wire::{HookCall, HookRequest, HookResponse},
};

/// What the configured hooks decided about a call.
///
/// lan's own type, shaped like mentra's `HookDecision` so the adapter bridging
/// them is a `match` and nothing more — but carrying the replacement input as
/// parsed JSON rather than a string, because an in-process host reading this
/// should not have to parse a document back out of it.
#[derive(Debug, Clone, PartialEq)]
pub enum HookOutcome {
    /// No hook objected. Not the same as "no hook ran".
    Allow,
    /// Blocked, with a reason meant to be read — by the model, which sees it as
    /// the tool's error, and by whoever has to work out what happened.
    Deny(String),
    /// Run the tool with this input instead.
    ///
    /// `input` is what the chain left behind after every modification, and
    /// `reason` names each hook that changed something — "the input is not what
    /// the model wrote" is exactly what an audit trail is for.
    Modify {
        input: Value,
        reason: Option<String>,
    },
}

/// Runs every configured hook against a tool call.
#[derive(Clone)]
pub struct HookRunner {
    workspace: PathBuf,
    hooks: Vec<HookSpec>,
    report: Arc<dyn Fn(&str) + Send + Sync>,
}

impl HookRunner {
    pub fn new(workspace: impl Into<PathBuf>, hooks: Vec<HookSpec>) -> Self {
        Self {
            workspace: workspace.into(),
            hooks,
            report: Arc::new(|message| eprintln!("lan: {message}")),
        }
    }

    /// Redirects failure reports somewhere other than stderr.
    ///
    /// A broken hook is an operator's problem, not the model's, so it is said
    /// out loud by default — including when [`OnFailure::Allow`] means the turn
    /// carries on, which is the case where nothing else would ever mention it.
    /// A host that owns its own logging replaces the destination; it cannot
    /// remove it.
    pub fn with_reporter(self, report: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self {
            report: Arc::new(report),
            ..self
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Consults every applicable hook, in order, until one refuses.
    ///
    /// Never fails: every way a hook can go wrong ends as a [`HookOutcome`]
    /// carrying words, because an error here would reach the model as a bare
    /// blocked call with the reason thrown away.
    ///
    /// Blocking, because the runtime's interception point is synchronous. The
    /// call is wrapped so a subprocess does not stall the async runtime that
    /// reached it.
    pub fn decide(&self, call: &HookCall) -> HookOutcome {
        if self.hooks.is_empty() {
            return HookOutcome::Allow;
        }

        let request = HookRequest::from_call(HookEvent::PreToolUse, &self.workspace, call);

        without_starving_the_runtime(|| self.consult(request))
    }

    fn consult(&self, request: HookRequest) -> HookOutcome {
        // Rebound whenever a hook rewrites the input, so the next hook is asked
        // about the call as its predecessors left it. That is what makes
        // modifications compose, and it is why a modification cannot route a
        // call past a hook that runs after it.
        let mut request = request;
        let mut modifiers: Vec<String> = Vec::new();

        for spec in &self.hooks {
            // Which hooks apply is a function of the tool, and no hook can
            // change that — only the input moves.
            if !spec.applies_to(request.event, &request.tool_name) {
                continue;
            }

            match self.ask(spec, &request) {
                Ok(HookResponse::Allow { .. }) => continue,
                Ok(HookResponse::Deny { reason }) => {
                    return HookOutcome::Deny(denial(&spec.name, reason));
                }
                Ok(HookResponse::Modify { input, reason }) => {
                    // A tool's input is an object. Anything else fails inside
                    // the tool with a worse message, and running the original
                    // instead would ignore a hook that believed it had
                    // intervened — so it takes the failure path, like any other
                    // answer lan cannot use.
                    if !input.is_object() {
                        if let Some(outcome) = self.failed(spec, HookFailure::ModifiedInputShape) {
                            return outcome;
                        }
                        continue;
                    }

                    modifiers.push(attribution(&spec.name, reason));
                    request = request.with_input(input);
                }
                Err(failure) => {
                    if let Some(outcome) = self.failed(spec, failure) {
                        return outcome;
                    }
                }
            }
        }

        if modifiers.is_empty() {
            return HookOutcome::Allow;
        }

        HookOutcome::Modify {
            input: request.input,
            // Every hook that changed something, not just the last one: the
            // question an audit trail asks is who touched this call, and a
            // single name would answer it wrongly.
            reason: Some(modifiers.join("; ")),
        }
    }

    /// Reports a broken hook, then applies its configured failure mode.
    ///
    /// `Some` denies the call; `None` means carry on to the next hook.
    fn failed(&self, spec: &HookSpec, failure: HookFailure) -> Option<HookOutcome> {
        (self.report)(&format!("hook '{}' {failure}", spec.name));

        match spec.on_failure {
            OnFailure::Deny => Some(HookOutcome::Deny(format!(
                "hook '{}' could not answer and denies on failure: {failure}",
                spec.name
            ))),
            OnFailure::Allow => None,
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

impl PreExecutionHook for HookRunner {
    /// Never returns `Err`.
    ///
    /// mentra turns a hook error into a bare blocked-tool result, which throws
    /// the reason away; every outcome here is a [`HookDecision`] instead, so
    /// whatever happened reaches both the model and the audit trail as words.
    fn pre_tool_execution(
        &self,
        context: &PreExecutionContext,
    ) -> Result<HookDecision, RuntimeError> {
        let call = HookCall::new(
            context.agent_id.clone(),
            context.tool_name.clone(),
            context.tool_call_id.clone(),
            context.input_json.clone(),
        );

        Ok(match self.decide(&call) {
            HookOutcome::Allow => HookDecision::Allow,
            HookOutcome::Deny(reason) => HookDecision::Deny(reason),
            HookOutcome::Modify { input, reason } => match serde_json::to_string(&input) {
                Ok(input_json) => HookDecision::Modify { input_json, reason },
                // Unreachable in practice — `input` was parsed out of a hook's
                // stdout, so it re-encodes. Denying rather than unwrapping is
                // what keeps "a runner never panics" true by construction.
                Err(error) => HookDecision::Deny(format!(
                    "a hook's replacement input could not be re-encoded: {error}"
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
                "hooks",
                &self.hooks.iter().map(|spec| &spec.name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// Runs blocking work without stalling the async runtime that called it.
///
/// mentra's hook trait is synchronous but is invoked from inside a turn, so
/// this call sits on a tokio worker while a subprocess runs. `block_in_place`
/// hands the worker's remaining tasks to another thread first — but it panics
/// on a current-thread runtime, so the flavor is checked rather than assumed.
/// On a current-thread runtime there is nothing to hand off to, and the hook's
/// timeout is what bounds the stall.
fn without_starving_the_runtime<R>(work: impl FnOnce() -> R) -> R {
    match Handle::try_current() {
        Ok(handle) if matches!(handle.runtime_flavor(), RuntimeFlavor::MultiThread) => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

fn denial(name: &str, reason: Option<String>) -> String {
    match reason {
        Some(reason) => format!("denied by hook '{name}': {reason}"),
        None => format!("denied by hook '{name}'"),
    }
}

fn attribution(name: &str, reason: Option<String>) -> String {
    match reason {
        Some(reason) => format!("hook '{name}': {reason}"),
        None => format!("hook '{name}'"),
    }
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

    #[error("asked to replace the tool input with something that is not a JSON object")]
    ModifiedInputShape,

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn call(tool_name: &str) -> HookCall {
        HookCall::new("agent-1", tool_name, "call-1", r#"{"command":"ls"}"#)
    }

    fn sh(name: &str, script: &str) -> HookSpec {
        HookSpec::new(
            name,
            vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
        )
    }

    /// A reporter that remembers, so a test can prove a failure was announced.
    #[derive(Default, Clone)]
    struct Reports(Arc<Mutex<Vec<String>>>);

    impl Reports {
        fn install(&self, runner: HookRunner) -> HookRunner {
            let sink = self.0.clone();
            runner.with_reporter(move |message| {
                sink.lock().expect("not poisoned").push(message.to_string())
            })
        }

        fn all(&self) -> Vec<String> {
            self.0.lock().expect("not poisoned").clone()
        }
    }

    fn decide(hooks: Vec<HookSpec>) -> (HookOutcome, Vec<String>) {
        let reports = Reports::default();
        let runner = reports.install(HookRunner::new(".", hooks));

        let outcome = runner.decide(&call("shell"));

        (outcome, reports.all())
    }

    fn denied(outcome: HookOutcome) -> String {
        match outcome {
            HookOutcome::Deny(reason) => reason,
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn no_hooks_means_no_subprocess_and_no_opinion() {
        let (outcome, reports) = decide(Vec::new());

        assert_eq!(outcome, HookOutcome::Allow);
        assert!(reports.is_empty());
    }

    #[test]
    fn a_hook_that_allows_lets_the_call_through() {
        let (outcome, reports) = decide(vec![sh("ok", r#"echo '{"decision":"allow"}'"#)]);

        assert_eq!(outcome, HookOutcome::Allow);
        assert!(reports.is_empty(), "a working hook is not news");
    }

    #[test]
    fn a_denial_carries_the_hook_name_and_its_reason() {
        let (outcome, _) = decide(vec![sh(
            "guard",
            r#"echo '{"decision":"deny","reason":"not here"}'"#,
        )]);

        assert_eq!(
            outcome,
            HookOutcome::Deny("denied by hook 'guard': not here".to_string())
        );
    }

    #[test]
    fn the_hook_is_told_what_the_tool_wants() {
        // Echoing the request back proves the wire contract reached stdin.
        let (outcome, _) = decide(vec![sh(
            "echoer",
            r#"printf '{"decision":"deny","reason":%s}' "\"$(cat | tr -d '\n' | cut -c1-200)\"" "#,
        )]);

        let reason = denied(outcome);
        assert!(reason.contains("hook_schema"));
        assert!(reason.contains("pre_tool_use"));
        assert!(reason.contains("call-1"));
    }

    #[test]
    fn the_first_refusal_stops_the_chain() {
        let (outcome, _) = decide(vec![
            sh("first", r#"echo '{"decision":"deny","reason":"mine"}'"#),
            sh(
                "second",
                r#"echo '{"decision":"deny","reason":"also mine"}'"#,
            ),
        ]);

        assert_eq!(
            outcome,
            HookOutcome::Deny("denied by hook 'first': mine".to_string()),
            "a hook that already denied makes the next spawn pointless"
        );
    }

    #[test]
    fn a_hook_only_hears_about_the_tools_it_asked_for() {
        let runner = HookRunner::new(
            ".",
            vec![
                sh("guard", r#"echo '{"decision":"deny"}'"#).with_tools(vec!["files".to_string()]),
            ],
        )
        .with_reporter(|_| {});

        assert_eq!(runner.decide(&call("shell")), HookOutcome::Allow);
        assert_eq!(
            runner.decide(&call("files")),
            HookOutcome::Deny("denied by hook 'guard'".to_string())
        );
    }

    #[test]
    fn a_modify_replaces_the_input() {
        let (outcome, reports) = decide(vec![sh(
            "redact",
            r#"echo '{"decision":"modify","input":{"command":"ls"},"reason":"stripped the token"}'"#,
        )]);

        assert_eq!(
            outcome,
            HookOutcome::Modify {
                input: json!({"command": "ls"}),
                reason: Some("hook 'redact': stripped the token".to_string()),
            }
        );
        assert!(reports.is_empty(), "modifying is not a failure");
    }

    #[test]
    fn modifications_compose_in_order() {
        // The second hook answers differently depending on what it was shown,
        // so its output proves it was asked about the rewritten call.
        let (outcome, _) = decide(vec![
            sh(
                "first",
                r#"echo '{"decision":"modify","input":{"command":"once"}}'"#,
            ),
            sh(
                "second",
                r#"
                request=$(cat)
                case "$request" in
                    *'"command":"once"'*)
                        echo '{"decision":"modify","input":{"command":"twice"}}' ;;
                    *) echo '{"decision":"deny","reason":"saw the original"}' ;;
                esac
                "#,
            ),
        ]);

        let HookOutcome::Modify { input, reason } = outcome else {
            panic!("expected a modification");
        };
        assert_eq!(input, json!({"command": "twice"}));
        assert_eq!(
            reason,
            Some("hook 'first'; hook 'second'".to_string()),
            "every hand that touched the call belongs in the trail"
        );
    }

    #[test]
    fn a_later_hook_can_still_deny_a_modified_call() {
        let (outcome, _) = decide(vec![
            sh(
                "rewriter",
                r#"echo '{"decision":"modify","input":{"command":"sneaky"}}'"#,
            ),
            sh("guard", r#"echo '{"decision":"deny","reason":"still no"}'"#),
        ]);

        assert_eq!(
            outcome,
            HookOutcome::Deny("denied by hook 'guard': still no".to_string()),
            "modify must not be a way past a hook that runs later"
        );
    }

    #[test]
    fn a_replacement_that_is_not_an_object_is_refused() {
        let (outcome, reports) = decide(vec![sh(
            "confused",
            r#"echo '{"decision":"modify","input":"ls -l"}'"#,
        )]);

        assert!(
            denied(outcome).contains("not a JSON object"),
            "running the original would ignore a hook that believed it intervened"
        );
        assert_eq!(reports.len(), 1);
    }

    #[test]
    fn a_broken_hook_denies_by_default_and_says_so() {
        let cases = [
            ("gone", sh("gone", "exit 7"), "exited with code 7"),
            ("silent", sh("silent", "true"), "printed nothing"),
            (
                "babbling",
                sh("babbling", "echo not json"),
                "not a decision",
            ),
            (
                "half-modifying",
                sh("half-modifying", r#"echo '{"decision":"modify"}'"#),
                "not a decision",
            ),
        ];

        for (name, spec, expected) in cases {
            let (outcome, reports) = decide(vec![spec]);

            let reason = denied(outcome);
            assert!(
                reason.contains(expected),
                "{name}: denial must say what broke, got {reason}"
            );
            assert_eq!(reports.len(), 1, "{name}: a broken hook must be announced");
            assert!(reports[0].contains(expected));
        }
    }

    #[test]
    fn a_hooks_stderr_reaches_the_reason() {
        let (outcome, _) = decide(vec![sh("noisy", "echo 'no python' >&2; exit 1")]);

        assert!(denied(outcome).contains("no python"));
    }

    #[test]
    fn a_hanging_hook_denies_rather_than_hanging_the_turn() {
        let (outcome, reports) = decide(vec![
            sh("stuck", "sleep 30").with_timeout(Duration::from_millis(150)),
        ]);

        assert!(denied(outcome).contains("150ms"));
        assert_eq!(reports.len(), 1);
    }

    #[test]
    fn an_observer_can_choose_to_fail_open() {
        let (outcome, reports) = decide(vec![
            sh("logger", "exit 1").with_on_failure(OnFailure::Allow),
            sh("guard", r#"echo '{"decision":"allow"}'"#),
        ]);

        assert_eq!(
            outcome,
            HookOutcome::Allow,
            "fail-open is a per-hook choice, written down rather than inferred"
        );
        assert_eq!(
            reports.len(),
            1,
            "carrying on is not the same as staying quiet"
        );
    }

    #[test]
    fn a_command_is_never_handed_to_a_shell() {
        // `;` is shell syntax; as argv it is just an argument. If lan ever
        // started interpreting the command, `touch` would run.
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("ran");
        let spec = HookSpec::new(
            "argv",
            vec![
                "/bin/echo".to_string(),
                format!("{{\"decision\":\"allow\"}}; touch {}", marker.display()),
            ],
        );

        let (outcome, _) = decide(vec![spec]);

        // The whole line is echoed, so it is not a decision — which is itself
        // the proof that nothing split it on the semicolon.
        assert!(matches!(outcome, HookOutcome::Deny(_)));
        assert!(!marker.exists(), "a hook command must not reach a shell");
    }

    /// Puts a call to the runner the way mentra does.
    fn hook_decision(hooks: Vec<HookSpec>) -> HookDecision {
        HookRunner::new(".", hooks)
            .with_reporter(|_| {})
            .pre_tool_execution(&PreExecutionContext {
                agent_id: "agent-1".to_string(),
                tool_name: "shell".to_string(),
                tool_call_id: "call-1".to_string(),
                input_json: r#"{"command":"ls"}"#.to_string(),
            })
            .expect("a runner never errors")
    }

    #[test]
    fn the_runtime_seam_carries_every_outcome() {
        assert_eq!(
            hook_decision(vec![sh("ok", r#"echo '{"decision":"allow"}'"#)]),
            HookDecision::Allow
        );

        assert_eq!(
            hook_decision(vec![sh("no", r#"echo '{"decision":"deny","reason":"x"}'"#)]),
            HookDecision::Deny("denied by hook 'no': x".to_string())
        );

        // The replacement crosses back as JSON text, which is mentra's shape.
        assert_eq!(
            hook_decision(vec![sh(
                "rewrite",
                r#"echo '{"decision":"modify","input":{"command":"safe"}}'"#,
            )]),
            HookDecision::Modify {
                input_json: r#"{"command":"safe"}"#.to_string(),
                reason: Some("hook 'rewrite'".to_string()),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hook_runs_from_inside_a_multi_thread_runtime() {
        // block_in_place panics on the wrong flavor, so both flavors are
        // exercised: this one, and the current-thread one below.
        let (outcome, _) = decide(vec![sh("ok", r#"echo '{"decision":"allow"}'"#)]);

        assert_eq!(outcome, HookOutcome::Allow);
    }

    #[tokio::test]
    async fn a_hook_runs_from_inside_a_current_thread_runtime() {
        let (outcome, _) = decide(vec![sh("ok", r#"echo '{"decision":"allow"}'"#)]);

        assert_eq!(outcome, HookOutcome::Allow);
    }

    #[test]
    fn the_debug_view_names_the_hooks_without_leaking_the_reporter() {
        let runner = HookRunner::new("/repo", vec![sh("guard", "true")]);

        let shown = format!("{runner:?}");

        assert!(shown.contains("guard"));
        assert!(shown.contains("/repo"));
    }
}
