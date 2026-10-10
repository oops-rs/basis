//! Push notifications (ADR-0030 §4): webhooks registered on a task hear
//! every change of its status, with the caller's credentials, at least once
//! and across a restart — and only ever at addresses the guard allows.

#[path = "support/harness.rs"]
mod harness;

use std::{
    io::Write,
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use basis_a2a::{A2aService, BearerToken, Principal, a2a::error_code};
use harness::{ScriptedEndpoint, Served, base_config, read_http_request, user_message};
use serde_json::{Value, json};

/// One POST the webhook received.
#[derive(Debug, Clone)]
struct Delivery {
    headers: String,
    body: Value,
    accepted: bool,
}

/// A loopback webhook, failing its first `failures` deliveries (and every
/// one while `down`), recording each.
struct Webhook {
    url: String,
    deliveries: Arc<Mutex<Vec<Delivery>>>,
    down: Arc<AtomicBool>,
}

impl Webhook {
    fn start(failures: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let down = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicUsize::new(0));
        let (recorded, is_down) = (Arc::clone(&deliveries), Arc::clone(&down));
        thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let request = read_http_request(&mut stream);
                let (headers, body) = request.split_once("\r\n\r\n").unwrap_or((&request, ""));
                let accepted = !is_down.load(Ordering::SeqCst)
                    && failed.fetch_add(1, Ordering::SeqCst) >= failures;
                recorded.lock().unwrap().push(Delivery {
                    headers: headers.to_ascii_lowercase(),
                    body: serde_json::from_str(body).unwrap_or(Value::Null),
                    accepted,
                });
                let status = if accepted {
                    "200 OK"
                } else {
                    "503 Service Unavailable"
                };
                let _ = stream.write_all(
                    format!("HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
                        .as_bytes(),
                );
            }
        });
        Self {
            url,
            deliveries,
            down,
        }
    }

    fn deliveries(&self) -> Vec<Delivery> {
        self.deliveries.lock().unwrap().clone()
    }

    /// Waits until an accepted delivery says the task reached `state`.
    async fn heard(&self, state: &str) -> Delivery {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(delivery) = self.deliveries().into_iter().find(|delivery| {
                delivery.accepted && delivery.body["statusUpdate"]["status"]["state"] == state
            }) {
                return delivery;
            }
            assert!(
                Instant::now() < deadline,
                "never heard {state}: {:?}",
                self.deliveries()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

async fn pushing() -> Served {
    Served::start_with(ScriptedEndpoint::start(true), |config| {
        config
            .with_push_notifications()
            .with_private_webhooks()
            .with_principal(
                Principal::new("other").unwrap(),
                BearerToken::new("other-token").unwrap(),
            )
    })
    .await
}

fn webhook_config(webhook: &Webhook) -> Value {
    json!({
        "url": webhook.url,
        "token": "caller-check",
        "authentication": {"scheme": "Bearer", "credentials": "webhook-secret"},
    })
}

#[tokio::test]
async fn a_webhook_named_with_the_message_hears_each_change_with_its_credentials() {
    let served = pushing().await;
    assert_eq!(
        served.card().await["capabilities"]["pushNotifications"],
        true
    );
    let webhook = Webhook::start(0);
    let sent = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("call me back", None),
                "configuration": {
                    "returnImmediately": true,
                    "taskPushNotificationConfig": webhook_config(&webhook),
                },
            }),
        )
        .await;
    let id = sent["task"]["id"].as_str().unwrap().to_string();
    served.model.release();

    let completed = webhook.heard("TASK_STATE_COMPLETED").await;
    assert_eq!(completed.body["statusUpdate"]["taskId"], id.as_str());
    assert!(
        completed
            .headers
            .contains("authorization: bearer webhook-secret"),
        "{}",
        completed.headers
    );
    assert!(
        completed
            .headers
            .contains("x-a2a-notification-token: caller-check"),
        "{}",
        completed.headers
    );
    assert!(completed.headers.contains("content-type: application/json"));
    let states: Vec<Value> = webhook
        .deliveries()
        .iter()
        .map(|delivery| delivery.body["statusUpdate"]["status"]["state"].clone())
        .collect();
    let completed_at = states
        .iter()
        .position(|state| state == "TASK_STATE_COMPLETED")
        .unwrap();
    assert_eq!(
        completed_at,
        states.len() - 1,
        "the terminal change is the last: {states:?}"
    );
}

#[tokio::test]
async fn configs_are_created_read_listed_and_deleted_on_both_bindings_by_their_owner() {
    let served = pushing().await;
    let webhook = Webhook::start(0);
    let sent = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("watched", None),
                "configuration": {"returnImmediately": true},
            }),
        )
        .await;
    let id = sent["task"]["id"].as_str().unwrap().to_string();

    let mut config = webhook_config(&webhook);
    config["taskId"] = json!(id);
    config["id"] = json!("mine");
    let created = served
        .result("CreateTaskPushNotificationConfig", config)
        .await;
    assert_eq!(created["id"], "mine");
    assert!(
        created["authentication"].get("credentials").is_none(),
        "a held credential is never answered: {created}"
    );

    let fetched = served
        .result(
            "GetTaskPushNotificationConfig",
            json!({"taskId": id, "id": "mine"}),
        )
        .await;
    assert_eq!(fetched["url"], webhook.url.as_str());

    let rest = format!("{}/json/tasks/{id}/pushNotificationConfigs", served.base);
    let listed: Value = served
        .caller()
        .request(reqwest::Method::GET, &rest)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["configs"].as_array().unwrap().len(), 1, "{listed}");

    let other = served.as_token("other-token");
    assert_eq!(
        other
            .error_code("ListTaskPushNotificationConfigs", json!({"taskId": id}))
            .await,
        error_code::TASK_NOT_FOUND,
        "another principal's task has no configs to show it"
    );

    let deleted = served
        .caller()
        .request(reqwest::Method::DELETE, &format!("{rest}/mine"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);
    let after = served
        .result("ListTaskPushNotificationConfigs", json!({"taskId": id}))
        .await;
    assert!(after.get("configs").is_none(), "{after}");
    served.model.release();
}

#[tokio::test]
async fn a_failing_webhook_is_retried_until_it_answers() {
    let served = pushing().await;
    let webhook = Webhook::start(2);
    served.model.release();
    let sent = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("retry me", None),
                "configuration": {
                    "returnImmediately": true,
                    "taskPushNotificationConfig": {"url": webhook.url},
                },
            }),
        )
        .await;
    assert!(sent["task"]["id"].is_string());
    webhook.heard("TASK_STATE_COMPLETED").await;
    assert!(
        webhook
            .deliveries()
            .iter()
            .filter(|delivery| !delivery.accepted)
            .count()
            >= 2,
        "the first two were refused and sent again"
    );
}

#[tokio::test]
async fn a_private_webhook_is_refused_unless_the_operator_allows_it() {
    let served = Served::start_with(ScriptedEndpoint::start(false), |config| {
        config.with_push_notifications()
    })
    .await;
    let webhook = Webhook::start(0);
    let refused = served
        .call(
            "SendMessage",
            json!({
                "message": user_message("hi", None),
                "configuration": {"taskPushNotificationConfig": {"url": webhook.url}},
            }),
        )
        .await;
    assert_eq!(refused["error"]["code"], error_code::INVALID_PARAMS);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("loopback"),
        "{refused}"
    );
    assert_eq!(
        served.result("ListTasks", json!({})).await["totalSize"],
        0,
        "a refused webhook leaves no task behind"
    );
}

#[test]
fn an_undelivered_change_is_delivered_by_the_next_life() {
    harness::isolate_environment();
    let model = ScriptedEndpoint::start(false);
    let webhook = Webhook::start(0);
    webhook.down.store(true, Ordering::SeqCst);
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = || {
        base_config(workspace.path(), data.path(), &model)
            .with_push_notifications()
            .with_private_webhooks()
    };

    let first_life = tokio::runtime::Runtime::new().unwrap();
    first_life.block_on(async {
        let service = A2aService::open(config()).unwrap();
        let request = serde_json::from_value(json!({
            "message": user_message("outlive me", None),
            "configuration": {"taskPushNotificationConfig": {"url": webhook.url}},
        }))
        .unwrap();
        service
            .send_message(&Principal::anonymous(), request)
            .await
            .unwrap();
        // The task has settled (the send blocked until it did), and the
        // webhook was tried and refused: its first change is being retried,
        // and the completion is queued behind it.
        let deadline = Instant::now() + Duration::from_secs(30);
        while webhook.deliveries().is_empty() {
            assert!(Instant::now() < deadline, "never tried");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    first_life.shutdown_timeout(Duration::from_millis(200));

    webhook.down.store(false, Ordering::SeqCst);
    let second_life = tokio::runtime::Runtime::new().unwrap();
    second_life.block_on(async {
        let _service = A2aService::open(config()).unwrap();
        webhook.heard("TASK_STATE_COMPLETED").await;
    });
}
