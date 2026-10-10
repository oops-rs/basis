//! The HTTP+JSON binding, called the way a REST client calls it: the
//! interface URL read off the card, resources under it, errors as AIP-193
//! `Status` objects (spec §11).

#[path = "support/harness.rs"]
mod harness;

use harness::{ScriptedEndpoint, Served, artifact_text, sse_events, user_message};
use reqwest::Method;
use serde_json::{Value, json};

/// The HTTP+JSON interface the card advertises.
async fn interface(served: &Served) -> String {
    let card = served.card().await;
    let interfaces = card["supportedInterfaces"].as_array().unwrap();
    assert_eq!(interfaces[0]["protocolBinding"], "JSONRPC", "{card}");
    assert_eq!(interfaces[1]["protocolBinding"], "HTTP+JSON", "{card}");
    assert_eq!(interfaces[1]["protocolVersion"], "1.0");
    interfaces[1]["url"].as_str().unwrap().to_string()
}

async fn rest(served: &Served, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let url = format!("{}{path}", interface(served).await);
    let mut request = served.caller().request(method, &url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn reason(error: &Value) -> &str {
    error["error"]["details"][0]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("no ErrorInfo: {error}"))
}

#[tokio::test]
async fn a_message_sent_over_rest_is_the_same_task_json_rpc_sees() {
    let served = Served::start().await;
    let (status, sent) = rest(
        &served,
        Method::POST,
        "/message:send",
        Some(json!({"message": user_message("hello", None)})),
    )
    .await;
    assert_eq!(status, 200, "{sent}");
    let task = &sent["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
    let id = task["id"].as_str().unwrap();
    let context = task["contextId"].as_str().unwrap();

    let (status, fetched) = rest(&served, Method::GET, &format!("/tasks/{id}"), None).await;
    assert_eq!(status, 200);
    assert_eq!(artifact_text(&fetched), "reply-1");
    let over_rpc = served.result("GetTask", json!({"id": id})).await;
    assert_eq!(over_rpc["id"], fetched["id"]);

    let (_, listed) = rest(
        &served,
        Method::GET,
        &format!("/tasks?contextId={context}&status=completed&includeArtifacts=true"),
        None,
    )
    .await;
    assert_eq!(listed["totalSize"], 1, "{listed}");
    assert_eq!(artifact_text(&listed["tasks"][0]), "reply-1");

    let (status, error) = rest(&served, Method::POST, &format!("/tasks/{id}:cancel"), None).await;
    assert_eq!(status, 409, "a settled task cannot be canceled: {error}");
    assert_eq!(reason(&error), "TASK_NOT_CANCELABLE");
}

#[tokio::test]
async fn errors_carry_the_specs_status_and_reason() {
    let served = Served::start().await;
    let (status, error) = rest(&served, Method::GET, "/tasks/no-such-task", None).await;
    assert_eq!(status, 404);
    assert_eq!(error["error"]["status"], "NOT_FOUND");
    assert_eq!(reason(&error), "TASK_NOT_FOUND");
    assert_eq!(error["error"]["details"][0]["domain"], "a2a-protocol.org");

    let (status, error) = rest(
        &served,
        Method::POST,
        "/message:send",
        Some(json!({"message": {
            "messageId": "m", "role": "ROLE_USER",
            "parts": [{"url": "https://example.com/a.png", "mediaType": "image/png"}],
        }})),
    )
    .await;
    assert_eq!(status, 415, "{error}");
    assert_eq!(reason(&error), "CONTENT_TYPE_NOT_SUPPORTED");

    let (status, error) = rest(&served, Method::GET, "/acme/tasks/x", None).await;
    assert_eq!(status, 400, "{error}");
    assert!(
        error["error"]["message"].as_str().unwrap().contains("acme"),
        "a tenant is refused by name: {error}"
    );

    let (status, _) = rest(&served, Method::DELETE, "/message:send", None).await;
    assert_eq!(status, 405);
}

#[tokio::test]
async fn rest_has_the_same_authentication_and_version_check() {
    let served = Served::start().await;
    let url = format!("{}/tasks", interface(&served).await);
    let anonymous = served
        .http
        .get(&url)
        .header("A2A-Version", "1.0")
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    let unversioned = served
        .http
        .get(&url)
        .bearer_auth(harness::TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(unversioned.status(), 400);
    let error: Value = unversioned.json().await.unwrap();
    assert_eq!(reason(&error), "VERSION_NOT_SUPPORTED");
}

#[tokio::test]
async fn streams_are_bare_stream_responses_over_sse() {
    let served = Served::start_with(ScriptedEndpoint::start(true), |config| config).await;
    let url = interface(&served).await;
    let (_, sent) = rest(
        &served,
        Method::POST,
        "/message:send",
        Some(json!({
            "message": user_message("slow", None),
            "configuration": {"returnImmediately": true},
        })),
    )
    .await;
    let id = sent["task"]["id"].as_str().unwrap().to_string();

    // The proto's verb; the prose table's `POST` is served too.
    let subscription = served
        .caller()
        .request(Method::GET, &format!("{url}/tasks/{id}:subscribe"))
        .send()
        .await
        .unwrap();
    assert_eq!(subscription.status(), 200);
    assert!(
        subscription.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    served.model.release();
    let body = subscription.text().await.unwrap();
    let events = sse_events(&body);
    assert_eq!(events[0]["task"]["id"], id.as_str(), "{body}");
    assert!(events.iter().all(|event| event.get("jsonrpc").is_none()));
    let last = &events.last().unwrap()["statusUpdate"];
    assert_eq!(last["status"]["state"], "TASK_STATE_COMPLETED", "{body}");

    let settled = served
        .caller()
        .request(Method::POST, &format!("{url}/tasks/{id}:subscribe"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        settled.status(),
        400,
        "a settled task has nothing to stream"
    );
    let error: Value = settled.json().await.unwrap();
    assert_eq!(reason(&error), "UNSUPPORTED_OPERATION");
}

#[tokio::test]
async fn a_principal_holds_at_most_its_stream_cap() {
    let served = Served::start_with(ScriptedEndpoint::start(true), |config| {
        config.with_stream_cap(1)
    })
    .await;
    let url = interface(&served).await;
    let (_, sent) = rest(
        &served,
        Method::POST,
        "/message:send",
        Some(json!({
            "message": user_message("slow", None),
            "configuration": {"returnImmediately": true},
        })),
    )
    .await;
    let id = sent["task"]["id"].as_str().unwrap().to_string();
    let subscribe = || {
        served
            .caller()
            .request(Method::GET, &format!("{url}/tasks/{id}:subscribe"))
            .send()
    };

    let open = subscribe().await.unwrap();
    assert_eq!(open.status(), 200);
    let second = subscribe().await.unwrap();
    assert_eq!(second.status(), 400, "one stream is the cap");
    let error: Value = second.json().await.unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("streams open"),
        "{error}"
    );
    let streamed = served
        .call(
            "SendStreamingMessage",
            json!({"message": user_message("another", None)}),
        )
        .await;
    assert_eq!(
        streamed["error"]["code"],
        basis_a2a::a2a::error_code::UNSUPPORTED_OPERATION,
        "the cap counts both bindings and both methods"
    );

    // A stream that ends gives its place back.
    served.model.release();
    open.text().await.unwrap();
    let (_, again) = rest(
        &served,
        Method::POST,
        "/message:send",
        Some(json!({
            "message": user_message("next", None),
            "configuration": {"returnImmediately": true},
        })),
    )
    .await;
    let next = again["task"]["id"].as_str().unwrap();
    let reopened = served
        .caller()
        .request(Method::GET, &format!("{url}/tasks/{next}:subscribe"))
        .send()
        .await
        .unwrap();
    assert_ne!(reopened.status(), 400, "the place was given back");
}
