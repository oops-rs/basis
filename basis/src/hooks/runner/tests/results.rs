//! The same runner, asked about a call that has already run.
//!
//! Its sibling covers the seam before the call. The two share one `consult`,
//! so what is worth checking here is what only this event has: that the result
//! reaches a hook at all, that `allow` keeps it and `deny` replaces it with the
//! reason, and that a hook declared for the other event is never asked.
//!
//! The fixtures — `sh`, `call`, `Fixed` — come from the parent module, because
//! a second copy would let the two halves drift apart on what a hook looks
//! like.

use serde_json::json;

use super::{Fixed, call, sh};
use crate::hooks::{
    HookEvent, HookOutcome, HookRequest, HookRunner, HookSpec, Interceptor, InterceptorError,
    OnFailure,
};
use mentra::{
    runtime::{PostExecutionContext, PostExecutionHook, ResultDecision},
    tool::ToolResultContent,
};

/// Puts a finished call to the runner the way mentra does.
async fn result_decision(hooks: Vec<HookSpec>, output: &str, is_error: bool) -> ResultDecision {
    HookRunner::new(".", hooks)
        .with_reporter(|_| {})
        .post_tool_execution(&PostExecutionContext {
            agent_id: "agent-1".to_string(),
            tool_name: "shell".to_string(),
            tool_call_id: "call-1".to_string(),
            input_json: r#"{"command":"cat .env"}"#.to_string(),
            working_directory: std::path::PathBuf::from("."),
            content: ToolResultContent::text(output),
            is_error,
        })
        .await
        .expect("a runner never errors")
}

fn post(name: &str, script: &str) -> HookSpec {
    sh(name, script).with_event(HookEvent::PostToolUse)
}

/// Answers whatever it was built with when asked about a result, and nothing
/// about the call itself.
struct Reviews {
    name: &'static str,
    answer: fn(&HookRequest) -> Result<HookOutcome, InterceptorError>,
}

#[async_trait::async_trait]
impl Interceptor for Reviews {
    fn name(&self) -> &str {
        self.name
    }

    async fn intercept(&self, _call: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        Ok(HookOutcome::Allow)
    }

    async fn review(&self, result: &HookRequest) -> Result<HookOutcome, InterceptorError> {
        (self.answer)(result)
    }
}

#[tokio::test]
async fn a_result_reaches_the_host_before_the_workspace() {
    // The same ordering the call gets, for the same reason: the further a
    // participant is from the workspace's own data, the earlier it speaks.
    // The hook answers from what it was shown, so its output is only
    // reachable if the interceptor went first.
    let runner = HookRunner::new(
        ".",
        vec![post(
            "annotate",
            r#"
            request=$(cat)
            case "$request" in
                *'"output":"[redacted]"'*)
                    echo '{"decision":"replace","output":"[redacted] (checked)"}' ;;
                *) echo '{"decision":"deny","reason":"saw the original"}' ;;
            esac
            "#,
        )],
    )
    .with_interceptor(Reviews {
        name: "redact",
        answer: |_| {
            Ok(HookOutcome::Replace {
                output: json!("[redacted]"),
                is_error: false,
                reason: Some("a credential".to_string()),
            })
        },
    })
    .with_reporter(|_| {});

    let HookOutcome::Replace { output, reason, .. } = runner
        .review_async(&call("shell"), json!("TOKEN=hunter2"), false)
        .await
    else {
        panic!("expected a replacement");
    };
    assert_eq!(output, json!("[redacted] (checked)"));
    assert_eq!(
        reason,
        Some("interceptor 'redact': a credential; hook 'annotate'".to_string())
    );
}

#[tokio::test]
async fn a_hook_is_told_what_the_tool_produced() {
    // Echoing the request back proves the result reached stdin, and that the
    // input beside it is the one the tool ran with.
    let decision = result_decision(
        vec![post(
            "echoer",
            r#"printf '{"decision":"replace","output":%s}' "\"$(cat | tr -d '\n' | tr -d '"' | cut -c1-300)\"" "#,
        )],
        "TOKEN=hunter2",
        false,
    )
    .await;

    let ResultDecision::Replace { content, .. } = decision else {
        panic!("expected a replacement, got {decision:?}");
    };
    let echoed = content.to_display_string();
    assert!(echoed.contains("post_tool_use"), "got {echoed}");
    assert!(echoed.contains("TOKEN=hunter2"), "got {echoed}");
    assert!(echoed.contains("cat .env"), "got {echoed}");
    assert!(echoed.contains("is_error:false"), "got {echoed}");
}

#[tokio::test]
async fn the_result_seam_carries_every_outcome() {
    assert_eq!(
        result_decision(
            vec![post("ok", r#"echo '{"decision":"allow"}'"#)],
            "fine",
            false
        )
        .await,
        ResultDecision::Keep,
        "allow after the call is keep"
    );

    assert_eq!(
        result_decision(
            vec![post(
                "redact",
                r#"echo '{"decision":"replace","output":"[redacted]"}'"#,
            )],
            "TOKEN=hunter2",
            false,
        )
        .await,
        ResultDecision::Replace {
            content: ToolResultContent::text("[redacted]"),
            is_error: false,
        },
        "a replacement that says nothing about failure leaves it alone"
    );

    assert_eq!(
        result_decision(
            vec![post(
                "structured",
                r#"echo '{"decision":"replace","output":{"lines":[]},"is_error":true}'"#,
            )],
            "whatever",
            false,
        )
        .await,
        ResultDecision::Replace {
            content: ToolResultContent::Structured(json!({"lines": []})),
            is_error: true,
        },
        "a JSON object is a structured result, not a string of one"
    );
}

#[tokio::test]
async fn a_refusal_after_the_call_is_what_the_model_reads_instead() {
    // Nothing can be blocked here — the tool has run. The reason takes the
    // output's place, marked as a failure so the model does not read it as
    // the command's own words.
    let decision = result_decision(
        vec![post(
            "no-secrets",
            r#"echo '{"decision":"deny","reason":"that file is off limits"}'"#,
        )],
        "TOKEN=hunter2",
        false,
    )
    .await;

    let ResultDecision::Replace { content, is_error } = decision else {
        panic!("expected a replacement, got {decision:?}");
    };
    let shown = content.to_display_string();
    assert!(shown.contains("that file is off limits"), "got {shown}");
    assert!(shown.contains("no-secrets"), "got {shown}");
    assert!(!shown.contains("hunter2"), "the output must not survive");
    assert!(is_error);
}

#[tokio::test]
async fn a_broken_reviewer_denies_and_the_output_never_reaches_the_model() {
    let decision = result_decision(vec![post("crashes", "exit 3")], "TOKEN=hunter2", false).await;

    let ResultDecision::Replace { content, is_error } = decision else {
        panic!("expected a replacement, got {decision:?}");
    };
    assert!(is_error);
    assert!(
        !content.to_display_string().contains("hunter2"),
        "a guard that broke while checking an output has not cleared it"
    );
}

#[tokio::test]
async fn a_reviewer_that_would_rather_be_ignored_lets_the_result_stand() {
    let hooks = vec![post("logger", "exit 1").with_on_failure(OnFailure::Allow)];

    assert_eq!(
        result_decision(hooks, "fine", false).await,
        ResultDecision::Keep
    );
}

#[tokio::test]
async fn a_hook_answers_only_at_the_event_it_declared() {
    // The same file holds both, and each is asked once. A pre hook that
    // could also be consulted afterwards would be a control the operator
    // never asked for.
    let both = vec![
        sh(
            "before",
            r#"echo '{"decision":"deny","reason":"pre only"}'"#,
        ),
        post(
            "after",
            r#"echo '{"decision":"replace","output":"post only"}'"#,
        ),
    ];

    assert_eq!(
        HookRunner::new(".", both.clone())
            .with_reporter(|_| {})
            .decide_async(&call("shell"))
            .await,
        HookOutcome::Deny("denied by hook 'before': pre only".to_string())
    );
    assert_eq!(
        result_decision(both, "fine", false).await,
        ResultDecision::Replace {
            content: ToolResultContent::text("post only"),
            is_error: false,
        }
    );
}

#[tokio::test]
async fn an_interceptor_that_only_guards_calls_is_no_opinion_about_a_result() {
    let runner = HookRunner::new(".", Vec::new()).with_interceptor(Fixed::new("guard", |_| {
        Ok(HookOutcome::Deny("no".to_string()))
    }));

    assert_eq!(
        runner
            .review_async(&call("shell"), json!("fine"), false)
            .await,
        HookOutcome::Allow,
        "`intercept` answers about a call, and `review` is what answers about a result"
    );
}
