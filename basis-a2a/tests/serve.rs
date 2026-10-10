//! A served agent, called over the wire the way any A2A client calls it:
//! the card fetched from its well-known path, JSON-RPC posted to the
//! endpoint the card names, and a scripted model behind it.
//!
//! The harness — scripted model, served agent, wire client — is shared with
//! the other test files in `support/harness.rs`.

#[path = "support/harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use basis_a2a::{A2aServer, A2aService, AgentIdentity, Principal, ServeConfig, a2a::error_code};
use harness::{
    ScriptedEndpoint, Served, artifact_text, base_config, isolate_environment, sse_events,
    user_message,
};
use serde_json::{Value, json};

#[tokio::test]
async fn the_card_is_public_and_names_the_endpoint_it_is_served_on() {
    let served = Served::start().await;
    let card: Value = reqwest::get(&served.card_url)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(card["name"], "helper");
    assert_eq!(card["supportedInterfaces"][0]["url"], served.endpoint);
    assert_eq!(card["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
    assert_eq!(card["capabilities"]["streaming"], true);
    assert!(card["securitySchemes"]["bearer"].is_object(), "{card}");
}

#[tokio::test]
async fn a_call_without_the_token_or_the_version_is_refused() {
    let served = Served::start().await;
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {}});

    let anonymous = served
        .http
        .post(&served.endpoint)
        .header("A2A-Version", "1.0")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    let unversioned: Value = served
        .http
        .post(&served.endpoint)
        .bearer_auth(harness::TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        unversioned["error"]["code"],
        error_code::VERSION_NOT_SUPPORTED
    );
}

#[tokio::test]
async fn a_blocking_send_answers_with_the_completed_task_and_its_result() {
    let served = Served::start().await;
    let result = served
        .result(
            "SendMessage",
            json!({"message": user_message("hello", None)}),
        )
        .await;
    let task = &result["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
    assert_eq!(artifact_text(task), "reply-1");
    assert_eq!(task["history"][0]["messageId"], "m-hello");

    let id = task["id"].as_str().unwrap();
    let fetched = served.result("GetTask", json!({"id": id})).await;
    assert_eq!(artifact_text(&fetched), "reply-1");

    let listed = served.result("ListTasks", json!({})).await;
    assert_eq!(listed["totalSize"], 1);
    assert_eq!(listed["nextPageToken"], "");
    assert!(
        listed["tasks"][0].get("artifacts").is_none(),
        "artifacts are omitted unless asked for: {listed}"
    );
}

#[tokio::test]
async fn a_follow_up_in_the_same_context_continues_the_conversation() {
    let served = Served::start().await;
    let first = served
        .result(
            "SendMessage",
            json!({"message": user_message("first", None)}),
        )
        .await;
    let context_id = first["task"]["contextId"].as_str().unwrap().to_string();

    let second = served
        .result(
            "SendMessage",
            json!({"message": user_message("second", Some(&context_id))}),
        )
        .await;
    assert_eq!(second["task"]["contextId"], context_id.as_str());
    assert_ne!(second["task"]["id"], first["task"]["id"]);
    assert_eq!(second["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    let sent = served.model.requests();
    let last = sent.last().expect("the model was called");
    assert!(
        last.contains("first") && last.contains("reply-1") && last.contains("second"),
        "the follow-up's turn carries the earlier exchange: {last}"
    );

    let in_context = served
        .result("ListTasks", json!({"contextId": context_id}))
        .await;
    assert_eq!(in_context["totalSize"], 2);
}

#[tokio::test]
async fn an_unknown_context_or_a_reply_to_a_settled_task_is_refused() {
    let served = Served::start().await;
    assert_eq!(
        served
            .error_code(
                "SendMessage",
                json!({"message": user_message("hi", Some("not-a-context"))})
            )
            .await,
        error_code::INVALID_PARAMS
    );

    let done = served
        .result("SendMessage", json!({"message": user_message("hi", None)}))
        .await;
    let mut reply = user_message("again", None);
    reply["taskId"] = done["task"]["id"].clone();
    assert_eq!(
        served
            .error_code("SendMessage", json!({"message": reply}))
            .await,
        error_code::UNSUPPORTED_OPERATION
    );
    assert_eq!(
        served.error_code("GetTask", json!({"id": "nope"})).await,
        error_code::TASK_NOT_FOUND
    );
}

#[tokio::test]
async fn returning_immediately_hands_back_a_task_that_settles_later() {
    let served = Served::start_with(ScriptedEndpoint::start(true), |config| config).await;
    let result = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("slow", None),
                "configuration": {"returnImmediately": true},
            }),
        )
        .await;
    let task = &result["task"];
    let state = task["status"]["state"].as_str().unwrap();
    assert!(
        state == "TASK_STATE_SUBMITTED" || state == "TASK_STATE_WORKING",
        "{task}"
    );
    let id = task["id"].as_str().unwrap();

    // One conversation, one executor: a second message while the first is
    // still running is the caller's to wait out.
    let context_id = task["contextId"].as_str().unwrap();
    assert_eq!(
        served
            .error_code(
                "SendMessage",
                json!({"message": user_message("meanwhile", Some(context_id))})
            )
            .await,
        error_code::UNSUPPORTED_OPERATION
    );

    served.model.release();
    let settled = served.wait_for(id, "TASK_STATE_COMPLETED").await;
    assert_eq!(artifact_text(&settled), "reply-1");
}

#[tokio::test]
async fn a_streamed_send_opens_on_the_task_and_closes_on_its_terminal_state() {
    let served = Served::start().await;
    let response = served
        .post(json!({
            "jsonrpc": "2.0", "id": 7, "method": "SendStreamingMessage",
            "params": {"message": user_message("stream me", None)},
        }))
        .await;
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let body = response.text().await.unwrap();
    let events = sse_events(&body);

    assert!(events.iter().all(|event| event["id"] == 7));
    assert!(events[0]["result"]["task"].is_object(), "{body}");
    let artifact = events
        .iter()
        .find_map(|event| event["result"].get("artifactUpdate"))
        .unwrap_or_else(|| panic!("the result is streamed: {body}"));
    assert_eq!(artifact["artifact"]["parts"][0]["text"], "reply-1");
    let last = &events.last().unwrap()["result"]["statusUpdate"];
    assert_eq!(last["status"]["state"], "TASK_STATE_COMPLETED", "{body}");
}

#[tokio::test]
async fn cancelling_a_running_task_settles_it_canceled() {
    let served = Served::start_with(ScriptedEndpoint::start(true), |config| config).await;
    let result = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("never mind", None),
                "configuration": {"returnImmediately": true},
            }),
        )
        .await;
    let id = result["task"]["id"].as_str().unwrap().to_string();

    // Cancellation lands at a turn boundary (ADR-0019): the held turn has to
    // end before the task can.
    let canceller = {
        let endpoint = served.endpoint.clone();
        let id = id.clone();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(&endpoint)
                .bearer_auth(harness::TOKEN)
                .header("A2A-Version", "1.0")
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "CancelTask", "params": {"id": id}}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    served.model.release();
    let cancelled = canceller.await.unwrap();
    assert!(cancelled.get("error").is_none(), "{cancelled}");

    served.wait_for(&id, "TASK_STATE_CANCELED").await;
    assert_eq!(
        served.error_code("CancelTask", json!({"id": id})).await,
        error_code::TASK_NOT_CANCELABLE
    );
}

#[tokio::test]
async fn unsupported_features_are_refused_with_the_specs_own_errors() {
    let served = Served::start().await;
    assert_eq!(
        served
            .error_code(
                "CreateTaskPushNotificationConfig",
                json!({"taskId": "x", "url": "https://example.com"})
            )
            .await,
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
    );
    assert_eq!(
        served.error_code("GetExtendedAgentCard", json!({})).await,
        error_code::UNSUPPORTED_OPERATION
    );
    assert_eq!(
        served.error_code("NoSuchMethod", json!({})).await,
        error_code::METHOD_NOT_FOUND
    );
    let file = json!({
        "messageId": "m-file", "role": "ROLE_USER",
        "parts": [{"url": "https://example.com/a.png", "mediaType": "image/png"}],
    });
    assert_eq!(
        served
            .error_code("SendMessage", json!({"message": file}))
            .await,
        error_code::CONTENT_TYPE_NOT_SUPPORTED
    );
}

#[tokio::test]
async fn a_non_loopback_address_without_a_token_is_refused_before_listening() {
    isolate_environment();
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = ServeConfig::new(workspace.path(), AgentIdentity::new("open", "No token."))
        .with_data_dir(data.path());
    let service = A2aService::open(config).unwrap();
    let refused = A2aServer::bind("0.0.0.0:0".parse().unwrap(), service).await;
    assert!(matches!(
        refused,
        Err(basis_a2a::BindError::Unauthenticated(_))
    ));
}

#[test]
fn a_restarted_server_resumes_driving_the_tasks_it_accepted() {
    isolate_environment();
    let model = ScriptedEndpoint::start(true);
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = || base_config(workspace.path(), data.path(), &model);
    let request = |text: &str| -> basis_a2a::a2a::SendMessageRequest {
        serde_json::from_value(json!({
            "message": user_message(text, None),
            "configuration": {"returnImmediately": true},
        }))
        .unwrap()
    };

    // First life: the task is accepted, and the server shuts down while its
    // turn is in flight.
    let first_life = tokio::runtime::Runtime::new().unwrap();
    let id = first_life.block_on(async {
        let service = A2aService::open(config()).unwrap();
        let basis_a2a::a2a::SendMessageResponse::Task(task) = service
            .send_message(&Principal::anonymous(), request("survive"))
            .await
            .unwrap()
        else {
            panic!("a send answers with a task");
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while model.requests().is_empty() {
            assert!(
                Instant::now() < deadline,
                "the turn never reached the model"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        service.shutdown().await;
        task.id
    });
    // The first life's request is answered now, with its runtime still up:
    // had its driver outlived the shutdown, this is the answer it would go
    // on to record, and there is time here for it to.
    model.release();
    std::thread::sleep(Duration::from_millis(500));
    let tasks = basis_tasks::Tasks::open_at(data.path(), workspace.path()).unwrap();
    let summaries = tasks.list().unwrap();
    assert_eq!(summaries.len(), 1);
    assert!(
        tasks
            .terminal(&basis_tasks::TaskHandle::parse(&summaries[0].task).unwrap())
            .unwrap()
            .is_none(),
        "the shutdown recorded nothing about the turn it cut off"
    );
    drop(first_life);

    // Second life: opening the service is all it takes to pick the task back
    // up — nobody sends anything.
    let second_life = tokio::runtime::Runtime::new().unwrap();
    second_life.block_on(async {
        let service = A2aService::open(config()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let task = service
                .get_task(
                    &Principal::anonymous(),
                    serde_json::from_value(json!({"id": id})).unwrap(),
                )
                .unwrap();
            if task.status.state == basis_a2a::a2a::TaskState::Completed {
                let text = task.artifacts.unwrap()[0].parts[0]
                    .as_text()
                    .unwrap()
                    .to_string();
                assert_eq!(
                    text, "reply-2",
                    "the second life's own turn answered, not the first life's"
                );
                assert_eq!(model.requests().len(), 2, "the turn was driven again");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the restarted server never finished the task: {:?}",
                task.status.state
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
}
