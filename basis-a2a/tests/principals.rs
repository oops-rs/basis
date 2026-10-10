//! One served agent shared by several callers who cannot see each other's
//! work (ADR-0030 §8): every task-scoped method answers another principal's
//! task exactly as it answers a task that does not exist.

#[path = "support/harness.rs"]
mod harness;

use basis_a2a::{A2aService, BearerToken, OpenError, Principal, a2a::error_code};
use harness::{ScriptedEndpoint, Served, base_config, user_message};
use serde_json::json;

const ALICE: &str = "alice-token";
const BOB: &str = "bob-token";

async fn shared() -> Served {
    Served::start_with(ScriptedEndpoint::start(false), |config| {
        config
            .with_principal(
                Principal::new("alice").unwrap(),
                BearerToken::new(ALICE).unwrap(),
            )
            .with_principal(
                Principal::new("bob").unwrap(),
                BearerToken::new(BOB).unwrap(),
            )
    })
    .await
}

#[tokio::test]
async fn another_principals_task_is_indistinguishable_from_a_missing_one() {
    let served = shared().await;
    let alice = served.as_token(ALICE);
    let bob = served.as_token(BOB);

    let created = alice
        .result(
            "SendMessage",
            json!({"message": user_message("mine", None)}),
        )
        .await;
    let id = created["task"]["id"].as_str().unwrap();
    let context_id = created["task"]["contextId"].as_str().unwrap();
    assert_eq!(
        alice.result("GetTask", json!({"id": id})).await["id"],
        id,
        "the owner sees its task"
    );

    let missing = bob.call("GetTask", json!({"id": "no-such-task"})).await;
    let foreign = bob.call("GetTask", json!({"id": id})).await;
    assert_eq!(foreign["error"]["code"], error_code::TASK_NOT_FOUND);
    assert_eq!(
        foreign["error"]["message"]
            .as_str()
            .unwrap()
            .replace(id, "<id>"),
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .replace("no-such-task", "<id>"),
        "the refusal says nothing a missing task's would not"
    );
    for method in ["CancelTask", "SubscribeToTask"] {
        assert_eq!(
            bob.error_code(method, json!({"id": id})).await,
            error_code::TASK_NOT_FOUND,
            "{method}"
        );
    }

    let mut reply = user_message("me too", None);
    reply["taskId"] = json!(id);
    assert_eq!(
        bob.error_code("SendMessage", json!({"message": reply}))
            .await,
        error_code::TASK_NOT_FOUND
    );
    let foreign_context = bob
        .call(
            "SendMessage",
            json!({"message": user_message("continue", Some(context_id))}),
        )
        .await;
    let unknown_context = bob
        .call(
            "SendMessage",
            json!({"message": user_message("continue", Some("not-a-context"))}),
        )
        .await;
    assert_eq!(foreign_context["error"]["code"], error_code::INVALID_PARAMS);
    assert_eq!(
        foreign_context["error"]["message"]
            .as_str()
            .unwrap()
            .replace(context_id, "<ctx>"),
        unknown_context["error"]["message"]
            .as_str()
            .unwrap()
            .replace("not-a-context", "<ctx>"),
    );

    assert_eq!(bob.result("ListTasks", json!({})).await["totalSize"], 0);
    assert_eq!(alice.result("ListTasks", json!({})).await["totalSize"], 1);
    assert_eq!(
        served.result("ListTasks", json!({})).await["totalSize"],
        0,
        "the default principal is a principal like any other"
    );
}

#[tokio::test]
async fn a_token_no_principal_holds_is_refused() {
    let served = shared().await;
    let stranger = served.as_token("guess");
    let response = stranger
        .post(json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {}}))
        .await;
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_loopback_server_with_no_token_is_one_anonymous_principal() {
    let served = Served::start_anonymous(ScriptedEndpoint::start(false), |config| config).await;
    let created = served
        .result("SendMessage", json!({"message": user_message("hi", None)}))
        .await;
    let id = created["task"]["id"].as_str().unwrap();
    assert_eq!(served.result("GetTask", json!({"id": id})).await["id"], id);
    let card = served.card().await;
    assert!(card.get("securitySchemes").is_none(), "{card}");
}

#[tokio::test]
async fn a_name_given_twice_fails_the_open() {
    harness::isolate_environment();
    let model = ScriptedEndpoint::start(false);
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = base_config(workspace.path(), data.path(), &model)
        .with_principal(
            Principal::new("alice").unwrap(),
            BearerToken::new(ALICE).unwrap(),
        )
        .with_principal(
            Principal::new("alice").unwrap(),
            BearerToken::new(BOB).unwrap(),
        );
    assert!(matches!(
        A2aService::open(config),
        Err(OpenError::Principals(_))
    ));
}

#[tokio::test]
async fn one_principal_cannot_hold_more_live_tasks_than_its_quota() {
    let served = Served::start_with(ScriptedEndpoint::start(true), |config| {
        config.with_live_task_quota(1).with_principal(
            Principal::new("bob").unwrap(),
            BearerToken::new(BOB).unwrap(),
        )
    })
    .await;
    let later = |text: &str| {
        json!({
            "message": user_message(text, None),
            "configuration": {"returnImmediately": true},
        })
    };
    let first = served.result("SendMessage", later("one")).await;
    let refused = served.call("SendMessage", later("two")).await;
    assert_eq!(refused["error"]["code"], error_code::UNSUPPORTED_OPERATION);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("still running"),
        "{refused}"
    );
    served
        .as_token(BOB)
        .result("SendMessage", later("bob's own"))
        .await;

    served.model.release();
    served
        .wait_for(
            first["task"]["id"].as_str().unwrap(),
            "TASK_STATE_COMPLETED",
        )
        .await;
    served.result("SendMessage", later("three")).await;
}
