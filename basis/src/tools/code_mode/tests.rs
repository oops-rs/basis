use std::{convert::Infallible, ops::ControlFlow, time::Duration};

use mentra::tool::ToolResultContent;
use serde_json::{Value, json};

use super::{Result, evaluate_with_timeout};

async fn evaluate(
    source: String,
    names: Vec<String>,
    mut dispatch: impl AsyncFnMut(String, Value) -> Result<ToolResultContent>,
) -> Result<Value> {
    match super::evaluate::<Infallible>(source, names, async |name, args| {
        ControlFlow::Continue(dispatch(name, args).await)
    })
    .await
    {
        ControlFlow::Continue(result) => result,
        ControlFlow::Break(never) => match never {},
    }
}

#[tokio::test]
async fn chains_tools_with_computed_input_and_returns_only_the_selection() {
    let mut calls = Vec::new();
    let output = evaluate(
        r#"
        const records = JSON.parse(await tools["mcp__ci__list-runs"]({branch: "main"}));
        const summaries = [];
        for (const run of records.filter(r => r.failed)) {
            summaries.push(await tools.summary({id: run.id}));
        }
        return summaries;
        "#
        .into(),
        vec!["mcp__ci__list-runs".into(), "summary".into()],
        async |name, args| {
            calls.push((name.clone(), args.clone()));
            match name.as_str() {
                "mcp__ci__list-runs" => Ok(ToolResultContent::text(
                    r#"[{"id":"01","failed":false},{"id":"02","failed":true}]"#,
                )),
                "summary" => Ok(ToolResultContent::Structured(json!({"failed": args["id"]}))),
                _ => unreachable!(),
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(
        calls,
        [
            ("mcp__ci__list-runs".into(), json!({"branch":"main"})),
            ("summary".into(), json!({"id":"02"}))
        ]
    );
    assert_eq!(output, json!([{"failed":"02"}]));
}

#[tokio::test]
async fn each_invocation_offers_only_bound_names_and_keeps_text_as_text() {
    let source = "return [typeof globalThis.marker, typeof tools.code, typeof tools.toString, \
        typeof tools.hidden, typeof process, typeof fetch, await tools.id({})]";
    let first = evaluate(
        "globalThis.marker = 1".into(),
        vec![],
        async |_, _| unreachable!(),
    )
    .await;
    assert_eq!(first.unwrap(), Value::Null);
    let second = evaluate(
        source.into(),
        vec!["code".into(), "id".into()],
        async |name, _| {
            assert_eq!(name, "id");
            Ok(ToolResultContent::text("4821"))
        },
    )
    .await
    .unwrap();
    assert_eq!(
        second,
        json!([
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "undefined",
            "4821"
        ])
    );
}

#[tokio::test]
async fn a_tool_error_rejects_the_promise_and_skips_the_dependent_call() {
    let mut calls = Vec::new();
    let result = evaluate(
        "await tools.denied({}); return await tools.later({});".into(),
        vec!["denied".into(), "later".into()],
        async |name, _| {
            calls.push(name);
            Err("approval denied".into())
        },
    )
    .await;
    assert!(result.unwrap_err().contains("approval denied"));
    assert_eq!(calls, ["denied"]);
}

#[tokio::test]
async fn a_promise_uses_the_arguments_at_the_call_site() {
    let result = evaluate(
        "const arg = {n: 1}; const pending = tools.echo(arg); arg.n = 2; return await pending;"
            .into(),
        vec!["echo".into()],
        async |_, args| Ok(ToolResultContent::Structured(args)),
    )
    .await
    .unwrap();
    assert_eq!(result, json!({"n":1}));
}

#[tokio::test]
async fn returning_with_an_unfinished_call_cannot_report_success() {
    let result = evaluate(
        "tools.wait({}); return 'done'".into(),
        vec!["wait".into()],
        async |_, _| std::future::pending().await,
    )
    .await;
    assert!(result.unwrap_err().contains("await pending tool calls"));
}

#[tokio::test]
async fn a_turn_stop_never_becomes_a_catchable_js_error() {
    let mut calls = Vec::new();
    let outcome = super::evaluate(
        "try { await tools.first({}) } catch (_) {} return await tools.later({})".into(),
        vec!["first".into(), "later".into()],
        async |name, _| {
            calls.push(name);
            ControlFlow::Break("terminal")
        },
    )
    .await;
    assert_eq!(outcome, ControlFlow::Break("terminal"));
    assert_eq!(calls, ["first"]);
}

#[tokio::test]
async fn timeouts_stop_busy_idle_and_waiting_scripts() {
    for source in [
        "while (true) {}",
        "await new Promise(() => {})",
        "await tools.wait({})",
    ] {
        let result = evaluate_with_timeout(
            source.into(),
            vec!["wait".into()],
            async |_, _| std::future::pending::<ControlFlow<(), Result<ToolResultContent>>>().await,
            Duration::from_millis(40),
        )
        .await;
        assert!(matches!(result, ControlFlow::Continue(Err(_))), "{source}");
    }
}

#[tokio::test]
async fn a_script_must_return_bounded_json() {
    for source in ["return 1n", "return 'x'.repeat(1024 * 1024)"] {
        let result =
            super::evaluate::<()>(source.into(), vec![], async |_, _| unreachable!()).await;
        assert!(matches!(result, ControlFlow::Continue(Err(_))), "{source}");
    }
}

#[tokio::test]
async fn dropping_the_evaluation_drops_a_pending_tool_and_skips_its_continuation() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let (started, waiting) = oneshot::channel();
    let (held_by_call, dropped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let mut started = Some(started);
        let mut held_by_call = Some(held_by_call);
        super::evaluate::<()>(
            "await tools.wait({}); await tools.later({})".into(),
            vec!["wait".into(), "later".into()],
            async move |_, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                let _held = held_by_call.take().unwrap();
                started.take().unwrap().send(()).unwrap();
                std::future::pending().await
            },
        )
        .await
    });
    waiting.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // A pending call owns this sender; cancellation must drop it.
    assert!(dropped.await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
