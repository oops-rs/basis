//! The runner's two adapters, each against the binding it serves.
//!
//! Split out of `runner.rs` only for its size — the file was past the 800-line
//! ceiling with these inline. What is checked here is the runner's own job:
//! that each binding's answers are translated faithfully, and that the chain's
//! rules (which `chain.rs` tests on their own) survive the translation.
//!
//! This file is the seam before a call; [`results`] is the one after it, split
//! off for size again and sharing the fixtures below. Both are gated to unix
//! for the same reason as `exec`'s tests: the subprocess fixtures are
//! `/bin/sh` scripts. The in-process binding needs no shell, and
//! `tests/interception.rs` covers it on every platform.

mod results;

use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::hooks::{InterceptorError, OnFailure};

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

/// Answers whatever it was built with, and records that it was asked.
struct Fixed {
    name: &'static str,
    answer: fn(&HookRequest) -> Result<HookOutcome, InterceptorError>,
}

impl Fixed {
    fn new(
        name: &'static str,
        answer: fn(&HookRequest) -> Result<HookOutcome, InterceptorError>,
    ) -> Self {
        Self { name, answer }
    }
}

#[async_trait::async_trait]
impl Interceptor for Fixed {
    fn name(&self) -> &str {
        self.name
    }

    async fn intercept(&self, call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        (self.answer)(call)
    }
}

fn allows(_call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
    Ok(HookOutcome::Allow)
}

#[test]
fn no_participants_means_no_subprocess_and_no_opinion() {
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
        vec![sh("guard", r#"echo '{"decision":"deny"}'"#).with_tools(vec!["files".to_string()])],
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
    // `;` is shell syntax; as argv it is just an argument. If basis ever
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

// -----------------------------------------------------------------------
// The in-process binding, through the same runner
// -----------------------------------------------------------------------

#[tokio::test]
async fn an_interceptor_decides_without_a_hooks_file_in_sight() {
    let runner = HookRunner::new(".", Vec::new())
        .with_interceptor(Fixed::new("guard", |_| {
            Ok(HookOutcome::Deny("nothing today".to_string()))
        }))
        .with_reporter(|_| {});

    assert!(!runner.is_empty(), "a runner with a say is not empty");
    assert_eq!(
        runner.decide_async(&call("shell")).await,
        HookOutcome::Deny("denied by interceptor 'guard': nothing today".to_string())
    );
}

#[tokio::test]
async fn an_interceptor_is_asked_before_a_hook_is_spawned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp.path().join("hook-ran");
    let runner = HookRunner::new(
        ".",
        vec![sh(
            "repo",
            &format!(
                r#"touch '{}'; echo '{{"decision":"allow"}}'"#,
                marker.display()
            ),
        )],
    )
    .with_interceptor(Fixed::new("host", |_| {
        Ok(HookOutcome::Deny("my program, my rules".to_string()))
    }))
    .with_reporter(|_| {});

    let outcome = runner.decide_async(&call("shell")).await;

    assert!(denied(outcome).contains("my program, my rules"));
    assert!(
        !marker.exists(),
        "the host's own refusal must land before a repository's hook is spawned"
    );
}

#[tokio::test]
async fn interceptors_are_asked_in_registration_order() {
    let runner = HookRunner::new(".", Vec::new())
        .with_interceptor(Fixed::new("first", |_| {
            Ok(HookOutcome::Deny("mine".to_string()))
        }))
        .with_interceptor(Fixed::new("second", |_| {
            Ok(HookOutcome::Deny("also mine".to_string()))
        }))
        .with_reporter(|_| {});

    assert_eq!(
        runner.decide_async(&call("shell")).await,
        HookOutcome::Deny("denied by interceptor 'first': mine".to_string())
    );
}

#[tokio::test]
async fn an_interceptors_rewrite_is_what_a_later_hook_is_asked_about() {
    // The hook answers differently depending on what it was shown, so its
    // output is the only proof that the rewrite reached it.
    let runner = HookRunner::new(
        ".",
        vec![sh(
            "check",
            r#"
            request=$(cat)
            case "$request" in
                *'"command":"rewritten"'*) echo '{"decision":"allow"}' ;;
                *) echo '{"decision":"deny","reason":"saw the original"}' ;;
            esac
            "#,
        )],
    )
    .with_interceptor(Fixed::new("rewriter", |_| {
        Ok(HookOutcome::Modify {
            input: json!({"command": "rewritten"}),
            reason: Some("narrowed".to_string()),
        })
    }))
    .with_reporter(|_| {});

    assert_eq!(
        runner.decide_async(&call("shell")).await,
        HookOutcome::Modify {
            input: json!({"command": "rewritten"}),
            reason: Some("interceptor 'rewriter': narrowed".to_string()),
        }
    );
}

#[tokio::test]
async fn an_interceptor_cannot_smuggle_a_call_past_a_hook() {
    // The no-smuggling property `PreExecutionHooks::run` documents. basis's
    // runner owns the ordering, so basis's runner has to keep it — and it
    // has to keep it across the two bindings, not only within one.
    let runner = HookRunner::new(
        ".",
        vec![sh(
            "guard",
            r#"echo '{"decision":"deny","reason":"no rewrite makes this fine"}'"#,
        )],
    )
    .with_interceptor(Fixed::new("rewriter", |_| {
        Ok(HookOutcome::Modify {
            input: json!({"command": "sneaky"}),
            reason: None,
        })
    }))
    .with_reporter(|_| {});

    let outcome = runner.decide_async(&call("shell")).await;

    assert!(denied(outcome).contains("no rewrite makes this fine"));
}

#[tokio::test]
async fn a_hook_and_an_interceptor_both_appear_in_the_trail() {
    let runner = HookRunner::new(
        ".",
        vec![sh(
            "pin",
            r#"echo '{"decision":"modify","input":{"command":"pinned"},"reason":"pinned the ref"}'"#,
        )],
    )
    .with_interceptor(Fixed::new("redact", |_| {
        Ok(HookOutcome::Modify {
            input: json!({"command": "redacted"}),
            reason: None,
        })
    }))
    .with_reporter(|_| {});

    let HookOutcome::Modify { reason, .. } = runner.decide_async(&call("shell")).await else {
        panic!("expected a modification");
    };
    assert_eq!(
        reason,
        Some("interceptor 'redact'; hook 'pin': pinned the ref".to_string())
    );
}

#[tokio::test]
async fn an_interceptor_that_errors_denies_and_says_so() {
    let reports = Reports::default();
    let runner = reports.install(
        HookRunner::new(".", Vec::new()).with_interceptor(Fixed::new("vault", |_| {
            Err(std::io::Error::other("the vault is unreachable").into())
        })),
    );

    let reason = denied(runner.decide_async(&call("shell")).await);

    assert!(reason.contains("vault"), "got {reason}");
    assert!(reason.contains("the vault is unreachable"), "got {reason}");
    assert_eq!(reports.all().len(), 1, "a broken guard must be announced");
}

#[tokio::test]
async fn an_interceptor_that_panics_denies_rather_than_taking_the_turn() {
    let reports = Reports::default();
    let runner = reports.install(
        HookRunner::new(".", Vec::new())
            .with_interceptor(Fixed::new("confused", |_| panic!("unwrapped a None"))),
    );

    let reason = denied(runner.decide_async(&call("shell")).await);

    assert!(reason.contains("panicked"), "got {reason}");
    assert!(reason.contains("unwrapped a None"), "got {reason}");
    assert_eq!(reports.all().len(), 1);
}

#[tokio::test]
async fn a_synchronous_decision_refuses_rather_than_skipping_an_interceptor() {
    // Silently deciding without a registered guard is the one failure this
    // module is arranged to avoid, so the synchronous path says no.
    let runner = HookRunner::new(".", Vec::new())
        .with_interceptor(Fixed::new("guard", allows))
        .with_reporter(|_| {});

    assert!(denied(runner.decide(&call("shell"))).contains("decide_async"));
}

#[tokio::test]
async fn an_interceptor_may_await_on_a_current_thread_runtime() {
    struct Awaits;

    #[async_trait::async_trait]
    impl Interceptor for Awaits {
        fn name(&self) -> &str {
            "awaits"
        }

        async fn intercept(&self, _call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok(HookOutcome::Deny("after awaiting".to_string()))
        }
    }

    let runner = HookRunner::new(".", Vec::new()).with_interceptor(Awaits);

    assert!(
        denied(runner.decide_async(&call("shell")).await).contains("after awaiting"),
        "an interceptor doing real work must not need a multi-thread runtime"
    );
}

/// Puts a call to the runner the way mentra does.
async fn hook_decision(hooks: Vec<HookSpec>) -> HookDecision {
    HookRunner::new(".", hooks)
        .with_reporter(|_| {})
        .pre_tool_execution(&PreExecutionContext {
            agent_id: "agent-1".to_string(),
            tool_name: "shell".to_string(),
            tool_call_id: "call-1".to_string(),
            input_json: r#"{"command":"ls"}"#.to_string(),
            working_directory: std::path::PathBuf::from("."),
        })
        .await
        .expect("a runner never errors")
}

#[tokio::test]
async fn the_runtime_seam_carries_every_outcome() {
    assert_eq!(
        hook_decision(vec![sh("ok", r#"echo '{"decision":"allow"}'"#)]).await,
        HookDecision::Allow
    );

    assert_eq!(
        hook_decision(vec![sh("no", r#"echo '{"decision":"deny","reason":"x"}'"#)]).await,
        HookDecision::Deny("denied by hook 'no': x".to_string())
    );

    // The replacement crosses back as JSON text, which is mentra's shape.
    assert_eq!(
        hook_decision(vec![sh(
            "rewrite",
            r#"echo '{"decision":"modify","input":{"command":"safe"}}'"#,
        )])
        .await,
        HookDecision::Modify {
            input_json: r#"{"command":"safe"}"#.to_string(),
            reason: Some("hook 'rewrite'".to_string()),
        }
    );
}

/// Both flavors, because a hook waits on a subprocess and where that wait
/// happens has to be right on either. `spawn_blocking` works on both;
/// `block_in_place`, which this used to need, panics on current_thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_runs_from_inside_a_multi_thread_runtime() {
    let runner = HookRunner::new(".", vec![sh("ok", r#"echo '{"decision":"allow"}'"#)]);

    assert_eq!(
        runner.decide_async(&call("shell")).await,
        HookOutcome::Allow
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_hook_runs_from_inside_a_current_thread_runtime() {
    let runner = HookRunner::new(".", vec![sh("ok", r#"echo '{"decision":"allow"}'"#)]);

    assert_eq!(
        runner.decide_async(&call("shell")).await,
        HookOutcome::Allow
    );
}

#[test]
fn the_debug_view_names_both_bindings_without_leaking_the_reporter() {
    let runner = HookRunner::new("/repo", vec![sh("guard", "true")])
        .with_interceptor(Fixed::new("redact", allows));

    let shown = format!("{runner:?}");

    assert!(shown.contains("guard"));
    assert!(shown.contains("redact"));
    assert!(shown.contains("/repo"));
}
