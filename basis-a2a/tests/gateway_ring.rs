//! Gateway observations belong to the process driving a durable task. The
//! A2A adapter passes that observer all the way to its private task runtime.

#[path = "support/harness.rs"]
mod harness;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use basis::{ConfigLookup, runtime::GatewayRingEvent};
use basis_a2a::RunDefaults;
use harness::{ScriptedEndpoint, Served, artifact_text, user_message};
use serde_json::json;

#[tokio::test]
async fn a_served_task_reports_rotation_and_completes_on_the_fallback() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let preferred = format!("http://{}/", listener.local_addr().unwrap());
    let refused = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().fallback(|| async {
                (axum::http::StatusCode::UNAUTHORIZED, "gateway unavailable")
            }),
        )
        .await
        .unwrap();
    });
    let fallback = ScriptedEndpoint::start(false);
    let files = tempfile::tempdir().unwrap();
    let preferred_key = files.path().join("preferred-key");
    let fallback_key = files.path().join("fallback-key");
    std::fs::write(&preferred_key, "preferred-test-key").unwrap();
    std::fs::write(&fallback_key, "fallback-test-key").unwrap();
    let config_file = files.path().join("model.json");
    std::fs::write(
        &config_file,
        json!({
            "schema": 1,
            "gateways": [
                {"base_url": preferred, "api_key_file": preferred_key},
                {"base_url": fallback.base_url, "api_key_file": fallback_key}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let served = Served::start_with(fallback, |config| {
        config
            .with_run_defaults(
                RunDefaults::default()
                    .with_config(ConfigLookup::File(config_file))
                    .with_model("test-model")
                    .with_deadline(Duration::from_secs(30)),
            )
            .with_gateway_ring_observer(move |event| seen.lock().unwrap().push(event))
    })
    .await;

    let result = served
        .result(
            "SendMessage",
            json!({"message": user_message("hello", None)}),
        )
        .await;
    let task = &result["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
    assert_eq!(artifact_text(task), "reply-1");
    assert!(
        events.lock().unwrap().iter().any(|event| matches!(
            event,
            GatewayRingEvent::Rotated { from, to, .. } if from.index == 0 && to.index == 1
        )),
        "the process hosting the task must hear its gateway rotation"
    );
    let requests = served.model.requests();
    assert!(
        requests
            .iter()
            .all(|request| request.contains("Bearer fallback-test-key"))
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.contains("preferred-test-key"))
    );
    refused.abort();
}
