//! Hops and deadlines across the wire (ADR-0030 §9, `docs/a2a/hops-v1.md`):
//! a message past the hop cap is refused, a task never gets longer than its
//! caller will wait, and a message without the extension is hop 0.

#[path = "support/harness.rs"]
mod harness;

use basis_a2a::a2a::error_code;
use chrono::{DateTime, Duration as Span, Utc};
use harness::{ScriptedEndpoint, Served, user_message};
use serde_json::{Value, json};

const EXTENSION: &str = "https://github.com/oops-rs/basis/blob/main/docs/a2a/hops-v1.md";

fn at_hop(text: &str, hop: u32, deadline: Option<DateTime<Utc>>) -> Value {
    let mut message = user_message(text, None);
    let mut hops = json!({"hop": hop});
    if let Some(deadline) = deadline {
        hops["deadline"] = json!(deadline.to_rfc3339());
    }
    message["metadata"] = json!({EXTENSION: hops});
    message
}

fn recorded(task: &Value) -> (u64, DateTime<Utc>) {
    let hops = &task["metadata"][EXTENSION];
    (
        hops["hop"].as_u64().unwrap_or_else(|| panic!("{task}")),
        hops["deadline"].as_str().unwrap().parse().unwrap(),
    )
}

#[tokio::test]
async fn the_card_declares_the_extension() {
    let served = Served::start().await;
    let card = served.card().await;
    let extensions = card["capabilities"]["extensions"].as_array().unwrap();
    assert!(
        extensions
            .iter()
            .any(|extension| extension["uri"] == EXTENSION),
        "{card}"
    );
}

#[tokio::test]
async fn a_message_past_the_cap_is_refused() {
    let served = Served::start_with(ScriptedEndpoint::start(false), |config| {
        config.with_hop_cap(2)
    })
    .await;
    let refused = served
        .call("SendMessage", json!({"message": at_hop("deep", 3, None)}))
        .await;
    assert_eq!(refused["error"]["code"], error_code::INVALID_PARAMS);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cap of 2"),
        "{refused}"
    );
    let accepted = served
        .result(
            "SendMessage",
            json!({"message": at_hop("deep enough", 2, None)}),
        )
        .await;
    assert_eq!(recorded(&accepted["task"]).0, 2);
}

#[tokio::test]
async fn a_task_never_outlives_its_callers_deadline() {
    let served = Served::start().await;
    let caller_deadline = Utc::now() + Span::seconds(20);
    let sent = served
        .result(
            "SendMessage",
            json!({"message": at_hop("hurry", 1, Some(caller_deadline))}),
        )
        .await;
    let (hop, deadline) = recorded(&sent["task"]);
    assert_eq!(hop, 1);
    assert!(
        deadline <= caller_deadline + Span::milliseconds(1),
        "{deadline} is past the caller's {caller_deadline}"
    );

    // The served agent's own deadline (a minute, in these tests) stands when
    // the caller's is later: a deadline only ever shrinks.
    let generous = Utc::now() + Span::hours(1);
    let sent = served
        .result(
            "SendMessage",
            json!({"message": at_hop("relax", 1, Some(generous))}),
        )
        .await;
    let (_, deadline) = recorded(&sent["task"]);
    assert!(deadline < Utc::now() + Span::seconds(61), "{deadline}");

    let past = Utc::now() - Span::seconds(1);
    assert_eq!(
        served
            .error_code(
                "SendMessage",
                json!({"message": at_hop("too late", 1, Some(past))})
            )
            .await,
        error_code::INVALID_PARAMS
    );
}

#[tokio::test]
async fn a_message_without_the_extension_is_hop_zero() {
    let served = Served::start().await;
    let sent = served
        .result("SendMessage", json!({"message": user_message("hi", None)}))
        .await;
    assert_eq!(recorded(&sent["task"]).0, 0);
}

/// The other half of the extension: a served task's own `@agent` delegation is
/// the next hop, and it tells the agent it reaches when it stops waiting.
#[tokio::test]
async fn a_served_task_tells_the_agent_it_delegates_to_the_next_hop() {
    let downstream = Served::start_anonymous(ScriptedEndpoint::start(false), |config| config).await;
    let model = ScriptedEndpoint::scripted(
        false,
        vec![
            harness::Reply::ToolCall {
                name: "spawn".to_string(),
                arguments: json!({"input": "@downstream relay this"}),
            },
            harness::Reply::Text("relayed".to_string()),
        ],
    );
    let upstream = Served::start_with(model, |config| {
        config.with_approval(basis_a2a::InboundApproval::Always)
    })
    .await;
    std::fs::create_dir_all(upstream.workspace.path().join(".basis")).unwrap();
    std::fs::write(
        upstream.workspace.path().join(".basis/a2a.json"),
        json!({"agents": {"downstream": {"url": downstream.base}}}).to_string(),
    )
    .unwrap();

    let sent = upstream
        .result(
            "SendMessage",
            json!({"message": at_hop("relay", 2, None), "configuration": {"returnImmediately": true}}),
        )
        .await;
    let id = sent["task"]["id"].as_str().unwrap().to_string();
    let settled = upstream.wait_for(&id, "TASK_STATE_COMPLETED").await;
    let (_, upstream_deadline) = recorded(&settled);

    let listed = downstream.result("ListTasks", json!({})).await;
    let reached = &listed["tasks"][0];
    let (hop, deadline) = recorded(reached);
    assert_eq!(hop, 3, "hop 2 delegating is hop 3: {reached}");
    assert!(
        deadline <= upstream_deadline,
        "a deadline only shrinks across a hop: {deadline} after {upstream_deadline}"
    );
}
