//! A host's own tasks, served (ADR-0033): everything above the `TaskHost`
//! seam — the bindings, the card, principals, streams, the quota, hops,
//! restarts — works for a hosted task as it does for a `basis-tasks` one.

#[path = "support/fake_host.rs"]
mod fake_host;
#[path = "support/harness.rs"]
mod harness;

use std::{sync::Arc, time::Duration};

use basis_a2a::{
    A2aService, AgentIdentity, BearerToken, HostedStanding, InboundApproval, OpenError, Principal,
    RunDefaults, ServeConfig,
    a2a::{Part, error_code},
};
use chrono::{Duration as Span, Utc};
use fake_host::{FakeHost, REFUSED};
use harness::{Served, TOKEN, sse_events, user_message};
use serde_json::{Value, json};

const HOPS: &str = "https://github.com/oops-rs/basis/blob/main/docs/a2a/hops-v1.md";

fn identity() -> AgentIdentity {
    AgentIdentity::new("host", "Runs its own tasks in tests.")
}

fn hosted(state: &tempfile::TempDir, host: &Arc<FakeHost>) -> ServeConfig {
    ServeConfig::hosted(identity(), state.path(), Arc::clone(host) as _)
}

/// A hosted agent behind the default token, as `configure` changes it.
async fn serve(host: &Arc<FakeHost>, configure: impl FnOnce(ServeConfig) -> ServeConfig) -> Served {
    let state = tempfile::tempdir().unwrap();
    let config = configure(hosted(&state, host).with_token(BearerToken::new(TOKEN).unwrap()));
    Served::start_config(config, state, Some(TOKEN.to_string())).await
}

fn answer() -> Vec<Part> {
    vec![
        Part::text("the answer"),
        Part::data(json!({"answer": 42, "sources": ["a.md"]})),
    ]
}

fn send(text: &str, context_id: Option<&str>, immediately: bool) -> Value {
    json!({
        "message": user_message(text, context_id),
        "configuration": {"returnImmediately": immediately},
    })
}

#[tokio::test]
async fn the_card_is_served_as_for_any_agent() {
    let served = serve(&FakeHost::held(), |config| config).await;
    let card = served.card().await;
    assert_eq!(card["name"], "host");
    assert_eq!(card["capabilities"]["streaming"], true);
    let interfaces = card["supportedInterfaces"].as_array().unwrap();
    assert_eq!(interfaces[0]["url"], served.endpoint.as_str(), "{card}");
    assert_eq!(interfaces.len(), 2, "{card}");
}

#[tokio::test]
async fn a_blocking_send_answers_with_the_hosts_parts_as_the_result() {
    let host = FakeHost::answering(answer());
    let served = serve(&host, |config| config).await;
    let task = served
        .result("SendMessage", send("what is it?", None, false))
        .await["task"]
        .clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
    let artifacts = task["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1, "{task}");
    assert_eq!(artifacts[0]["artifactId"], "result");
    let parts = &artifacts[0]["parts"];
    assert_eq!(parts[0]["text"], "the answer", "{task}");
    assert_eq!(parts[1]["data"]["answer"], 42, "{task}");

    // The host was handed what the server decided, and its id stays its own.
    let start = &host.calls().starts[0];
    assert_eq!(start.prompt, "what is it?");
    assert_eq!(start.parts.len(), 1);
    assert_eq!(start.principal, "default");
    assert_eq!(start.context_id, task["contextId"].as_str().unwrap());
    assert!(start.continues.is_none());
    assert_eq!(start.hop, 0);
    let id = task["id"].as_str().unwrap();
    assert_eq!(id.len(), 32, "{id}");
    assert!(
        id.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_ne!(id, "run-0");
}

#[tokio::test]
async fn a_hosted_task_runs_under_the_servers_task_deadline_unless_its_caller_is_sooner() {
    let now_ms = || u64::try_from(Utc::now().timestamp_millis()).unwrap();
    let hour_ms = 3_600_000;

    // Unset: basis-tasks' 30-minute default.
    let host = FakeHost::answering(answer());
    let served = serve(&host, |config| config).await;
    let before = now_ms();
    served.result("SendMessage", send("q", None, false)).await;
    let granted = host.calls().starts[0].deadline_ms - before;
    assert!(
        (hour_ms / 2 - 5_000..=hour_ms / 2 + 5_000).contains(&granted),
        "{granted}"
    );

    // Configured: the server's own, longer deadline.
    let host = FakeHost::answering(answer());
    let served = serve(&host, |config| {
        config.with_task_deadline(Duration::from_secs(2 * 3600))
    })
    .await;
    let before = now_ms();
    served.result("SendMessage", send("q", None, false)).await;
    let granted = host.calls().starts[0].deadline_ms - before;
    assert!(
        (2 * hour_ms - 5_000..=2 * hour_ms + 5_000).contains(&granted),
        "{granted}"
    );

    // A caller's sooner deadline still wins.
    let host = FakeHost::answering(answer());
    let served = serve(&host, |config| {
        config.with_task_deadline(Duration::from_secs(2 * 3600))
    })
    .await;
    let deadline = Utc::now() + Span::seconds(90);
    let mut message = user_message("q", None);
    message["metadata"] = json!({HOPS: {"hop": 1, "deadline": deadline.to_rfc3339()}});
    served
        .result("SendMessage", json!({"message": message}))
        .await;
    let deadline_ms = u64::try_from(deadline.timestamp_millis()).unwrap();
    assert!(host.calls().starts[0].deadline_ms <= deadline_ms);
}

#[tokio::test]
async fn returning_immediately_hands_back_a_task_that_settles_later() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config).await;
    let task = served
        .result("SendMessage", send("later", None, true))
        .await["task"]
        .clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_WORKING", "{task}");
    let id = task["id"].as_str().unwrap();

    let running = served.result("GetTask", json!({"id": id})).await;
    assert_eq!(running["status"]["state"], "TASK_STATE_WORKING");
    assert!(running.get("artifacts").is_none(), "{running}");

    host.complete(&host.started(0).await, answer());
    let done = served.wait_for(id, "TASK_STATE_COMPLETED").await;
    assert_eq!(done["artifacts"][0]["parts"][1]["data"]["answer"], 42);

    // The HTTP+JSON binding answers the same task the same way.
    let url = format!("{}{}/tasks/{id}", served.base, basis_a2a::HTTP_JSON_PATH);
    let rest: Value = served
        .caller()
        .request(reqwest::Method::GET, &url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rest["status"]["state"], "TASK_STATE_COMPLETED", "{rest}");
    assert_eq!(rest["artifacts"], done["artifacts"]);

    let failed = served.result("SendMessage", send("fail", None, true)).await["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    host.set(
        &host.started(1).await,
        HostedStanding::Failed {
            error: "the index is gone".into(),
        },
    );
    let failed = served.wait_for(&failed, "TASK_STATE_FAILED").await;
    assert_eq!(
        failed["status"]["message"]["parts"][0]["text"],
        "the index is gone"
    );
}

/// Streams `params` with `method`, while `drive` moves the host.
async fn stream_while(
    served: &Served,
    method: &str,
    params: Value,
    drive: impl Future<Output = ()> + Send + 'static,
) -> Vec<Value> {
    let driving = tokio::spawn(drive);
    let response = served
        .post(json!({"jsonrpc": "2.0", "id": 3, "method": method, "params": params}))
        .await;
    assert_eq!(response.status(), 200);
    let body = tokio::time::timeout(Duration::from_secs(20), response.text())
        .await
        .expect("the stream ends")
        .unwrap();
    driving.await.unwrap();
    sse_events(&body)
}

fn notes(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            event["result"]["statusUpdate"]["status"]["message"]["parts"][0]["text"].as_str()
        })
        .map(str::to_string)
        .collect()
}

fn final_state(events: &[Value]) -> &Value {
    &events.last().unwrap()["result"]["statusUpdate"]["status"]["state"]
}

#[tokio::test]
async fn a_stream_is_the_task_then_each_note_then_the_answer_then_its_state() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config).await;
    let driver = Arc::clone(&host);
    let events = stream_while(
        &served,
        "SendStreamingMessage",
        json!({"message": user_message("stream me", None)}),
        async move {
            let id = driver.started(0).await;
            driver.followed(&id, 1).await;
            driver.note(&id, "reading the index");
            driver.note(&id, "writing it up");
            tokio::time::sleep(Duration::from_millis(50)).await;
            driver.complete(&id, answer());
        },
    )
    .await;

    assert!(events[0]["result"]["task"].is_object(), "{events:?}");
    assert_eq!(notes(&events), ["reading the index", "writing it up"]);
    let artifact = events
        .iter()
        .position(|event| event["result"].get("artifactUpdate").is_some())
        .expect("the answer is streamed");
    assert_eq!(
        events[artifact]["result"]["artifactUpdate"]["artifact"]["parts"][1]["data"]["answer"],
        42
    );
    assert_eq!(artifact, events.len() - 2, "the state closes the stream");
    assert_eq!(*final_state(&events), "TASK_STATE_COMPLETED");
}

#[tokio::test]
async fn a_stream_outlives_progress_that_ends_early_and_closes_on_the_state() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config).await;
    let id = served
        .result("SendMessage", send("quiet", None, true))
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let driver = Arc::clone(&host);
    let events = stream_while(&served, "SubscribeToTask", json!({"id": id}), async move {
        let id = driver.started(0).await;
        driver.followed(&id, 1).await;
        driver.note(&id, "one note");
        driver.end_notes(&id);
        tokio::time::sleep(Duration::from_millis(200)).await;
        driver.set(&id, HostedStanding::Canceled);
    })
    .await;
    assert_eq!(notes(&events), ["one note"]);
    assert_eq!(*final_state(&events), "TASK_STATE_CANCELED", "{events:?}");
}

#[tokio::test]
async fn a_follow_up_continues_the_latest_task_in_its_context_by_the_hosts_id() {
    let host = FakeHost::answering(answer());
    let served = serve(&host, |config| config).await;
    let first = served
        .result("SendMessage", send("first", None, false))
        .await["task"]
        .clone();
    let context = first["contextId"].as_str().unwrap();
    served
        .result("SendMessage", send("second", Some(context), false))
        .await;
    served
        .result("SendMessage", send("third", Some(context), false))
        .await;

    let starts = host.calls().starts;
    assert_eq!(starts[1].continues.as_deref(), Some("run-0"));
    assert_eq!(starts[2].continues.as_deref(), Some("run-1"));
    assert!(starts.iter().all(|start| start.context_id == context));
}

#[tokio::test]
async fn a_busy_context_is_the_wait_error_and_a_refusal_is_the_callers() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config).await;
    let first = served
        .result("SendMessage", send("first", None, true))
        .await["task"]
        .clone();
    let busy = served
        .call(
            "SendMessage",
            send("again", first["contextId"].as_str(), true),
        )
        .await;
    assert_eq!(busy["error"]["code"], error_code::UNSUPPORTED_OPERATION);
    assert!(
        busy["error"]["message"]
            .as_str()
            .unwrap()
            .contains("still working; wait for it to settle"),
        "{busy}"
    );

    let refused = served.call("SendMessage", send(REFUSED, None, true)).await;
    assert_eq!(refused["error"]["code"], error_code::INVALID_PARAMS);
    assert_eq!(refused["error"]["message"], "this host will not do that");
}

#[tokio::test]
async fn cancelling_asks_the_host_and_answers_canceled() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config).await;
    let id = served
        .result("SendMessage", send("never mind", None, true))
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let canceled = served.result("CancelTask", json!({"id": id})).await;
    assert_eq!(
        canceled["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );
    assert_eq!(host.calls().cancels, ["run-0"]);
    assert_eq!(
        served.error_code("CancelTask", json!({"id": id})).await,
        error_code::TASK_NOT_CANCELABLE
    );
}

#[tokio::test]
async fn another_principals_hosted_task_is_not_found() {
    let host = FakeHost::held();
    let state = tempfile::tempdir().unwrap();
    let config = hosted(&state, &host)
        .with_principal(
            Principal::new("alice").unwrap(),
            BearerToken::new("alice-token").unwrap(),
        )
        .with_principal(
            Principal::new("bob").unwrap(),
            BearerToken::new("bob-token").unwrap(),
        );
    let served = Served::start_config(config, state, None).await;
    let alice = served.as_token("alice-token");
    let bob = served.as_token("bob-token");
    let task = alice.result("SendMessage", send("mine", None, true)).await["task"].clone();
    let id = task["id"].as_str().unwrap();
    assert_eq!(host.calls().starts[0].principal, "alice");

    assert_eq!(
        bob.error_code("GetTask", json!({"id": id})).await,
        error_code::TASK_NOT_FOUND
    );
    assert_eq!(
        bob.error_code("CancelTask", json!({"id": id})).await,
        error_code::TASK_NOT_FOUND
    );
    assert_eq!(bob.result("ListTasks", json!({})).await["totalSize"], 0);
    assert_eq!(alice.result("ListTasks", json!({})).await["totalSize"], 1);
    assert!(host.calls().cancels.is_empty());
}

#[tokio::test]
async fn the_live_task_quota_counts_hosted_tasks_that_have_not_settled() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config.with_live_task_quota(1)).await;
    served.result("SendMessage", send("one", None, true)).await;
    assert_eq!(
        served
            .error_code("SendMessage", send("two", None, true))
            .await,
        error_code::UNSUPPORTED_OPERATION
    );
    host.complete(&host.started(0).await, answer());
    served.result("SendMessage", send("two", None, true)).await;
}

#[tokio::test]
async fn the_host_is_handed_the_hop_and_the_callers_deadline() {
    let host = FakeHost::answering(answer());
    let served = serve(&host, |config| config).await;
    let deadline = Utc::now() + Span::seconds(90);
    let mut message = user_message("deep", None);
    message["metadata"] = json!({HOPS: {"hop": 3, "deadline": deadline.to_rfc3339()}});
    let task = served
        .result("SendMessage", json!({"message": message}))
        .await["task"]
        .clone();
    let start = &host.calls().starts[0];
    assert_eq!(start.hop, 3);
    let deadline_ms = u64::try_from(deadline.timestamp_millis()).unwrap();
    assert!(start.deadline_ms <= deadline_ms, "never past the caller's");
    assert!(
        start.deadline_ms + 1_000 >= deadline_ms,
        "clamped to it, not far below"
    );
    assert_eq!(task["metadata"][HOPS]["hop"], 3, "{task}");
}

/// A loopback webhook that accepts every delivery and keeps its body.
fn webhook() -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let request = harness::read_http_request(&mut stream);
            let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
            recorded
                .lock()
                .unwrap()
                .push(serde_json::from_str(body).unwrap_or(Value::Null));
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
            );
        }
    });
    (url, bodies)
}

#[tokio::test]
async fn a_webhook_hears_a_hosted_task_settle() {
    let host = FakeHost::held();
    let served = serve(&host, |config| {
        config.with_push_notifications().with_private_webhooks()
    })
    .await;
    let (url, bodies) = webhook();
    let sent = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("call me back", None),
                "configuration": {
                    "returnImmediately": true,
                    "taskPushNotificationConfig": {"url": url},
                },
            }),
        )
        .await;
    assert_eq!(sent["task"]["status"]["state"], "TASK_STATE_WORKING");
    host.complete(&host.started(0).await, answer());
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let heard = bodies
            .lock()
            .unwrap()
            .iter()
            .any(|body| body["statusUpdate"]["status"]["state"] == "TASK_STATE_COMPLETED");
        if heard {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never heard it settle"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn a_restarted_server_awaits_its_hosts_tasks_again_and_never_restarts_them() {
    let host = FakeHost::held();
    let state = tempfile::tempdir().unwrap();
    let caller = Principal::anonymous();

    let first_life = tokio::runtime::Runtime::new().unwrap();
    let id = first_life.block_on(async {
        let service = A2aService::open(hosted(&state, &host)).unwrap();
        let task = service
            .send_message(
                &caller,
                serde_json::from_value(send("survive", None, true)).unwrap(),
            )
            .await
            .unwrap();
        let basis_a2a::a2a::SendMessageResponse::Task(task) = task else {
            panic!("a task");
        };
        host.awaited("run-0", 1).await;
        service.shutdown().await;
        task.id
    });
    drop(first_life);

    let second_life = tokio::runtime::Runtime::new().unwrap();
    second_life.block_on(async {
        let service = A2aService::open(hosted(&state, &host)).unwrap();
        host.awaited("run-0", 2).await;
        host.complete("run-0", answer());
        let get = || service.get_task(&caller, serde_json::from_value(json!({"id": id})).unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let task = get().await.unwrap();
            if task.status.state == basis_a2a::a2a::TaskState::Completed {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "never completed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Recorded as settled: the next start has nothing left to await.
        service.shutdown().await;
    });
    drop(second_life);
    assert_eq!(
        host.calls().starts.len(),
        1,
        "a message is never started twice"
    );

    let third_life = tokio::runtime::Runtime::new().unwrap();
    third_life.block_on(async {
        let _service = A2aService::open(hosted(&state, &host)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    assert_eq!(
        host.calls().settled.len(),
        2,
        "a task recorded as settled is not awaited again"
    );
}

type Configure = fn(ServeConfig) -> ServeConfig;

#[tokio::test]
async fn settings_that_configure_a_basis_tasks_run_are_refused_by_name() {
    let host = FakeHost::held();
    let state = tempfile::tempdir().unwrap();
    let refusals: [(&str, Configure); 5] = [
        ("with_shell", ServeConfig::with_shell),
        ("with_run_defaults", |config| {
            config.with_run_defaults(RunDefaults::default().with_model("m"))
        }),
        ("InboundApproval::AskCaller", |config| {
            config
                .with_approval(InboundApproval::AskCaller {
                    deadline: Duration::from_secs(60),
                })
                .with_token(BearerToken::new(TOKEN).unwrap())
        }),
        ("InboundApproval::Always", |config| {
            config.with_approval(InboundApproval::Always)
        }),
        ("with_data_dir", |config| {
            config.with_data_dir("/tmp/nowhere")
        }),
    ];
    for (name, configure) in refusals {
        let error = A2aService::open(configure(hosted(&state, &host)))
            .err()
            .unwrap_or_else(|| panic!("{name} was not refused"));
        assert!(
            matches!(error, OpenError::NotForHostedTasks(refused) if refused == name),
            "{name}: {error}"
        );
        assert!(error.to_string().contains(name), "{error}");
    }
    // Defaults and every shared option are fine.
    A2aService::open(
        hosted(&state, &host)
            .with_run_defaults(RunDefaults::default())
            .with_approval(InboundApproval::Never)
            .with_live_task_quota(3),
    )
    .expect("opens");
}

fn question() -> Vec<Part> {
    vec![
        Part::text("Which branch?"),
        Part::data(json!({"choices": ["main", "dev"]})),
    ]
}

/// A reply to the task `id`, in `context`.
fn reply(id: &str, context: &str, text: &str) -> Value {
    let mut message = user_message(text, Some(context));
    message["taskId"] = json!(id);
    json!({"message": message})
}

/// Starts a task with a blocking send that the host pauses on
/// [`question`], and returns the task as that send answered it.
async fn asked(served: &Served, host: &Arc<FakeHost>) -> Value {
    let driver = Arc::clone(host);
    let n = host.calls().starts.len();
    let asking = tokio::spawn(async move {
        let id = driver.started(n).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver.ask(&id, question());
    });
    let task = served
        .result("SendMessage", send("deploy it", None, false))
        .await["task"]
        .clone();
    asking.await.unwrap();
    task
}

#[tokio::test]
async fn a_hosts_question_is_input_required_and_a_reply_resumes_the_same_task() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config).await;
    let task = asked(&served, &host).await;
    assert_eq!(
        task["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{task}"
    );
    let message = &task["status"]["message"];
    assert_eq!(message["role"], "ROLE_AGENT", "{task}");
    assert_eq!(message["parts"][0]["text"], "Which branch?", "{task}");
    assert_eq!(message["parts"][1]["data"]["choices"][1], "dev", "{task}");
    let id = task["id"].as_str().unwrap();
    let context = task["contextId"].as_str().unwrap();

    // Listed, and counted, as what it is.
    let listed = served
        .result("ListTasks", json!({"status": "TASK_STATE_INPUT_REQUIRED"}))
        .await;
    assert_eq!(listed["totalSize"], 1, "{listed}");

    // The reply reaches the host, and the blocking send answers once the
    // same task settles.
    let driver = Arc::clone(&host);
    let finishing = tokio::spawn(async move {
        driver.replied("run-0", 1).await;
        driver.complete("run-0", answer());
    });
    let done = served
        .result("SendMessage", reply(id, context, "main"))
        .await["task"]
        .clone();
    finishing.await.unwrap();
    assert_eq!(done["id"], id, "{done}");
    assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED", "{done}");
    let (replied, given) = host.calls().replies[0].clone();
    assert_eq!(replied, "run-0");
    assert_eq!(given.message_id, "m-main");
    assert_eq!(given.text, "main");
    assert_eq!(given.parts, vec![Part::text("main")]);
    assert_eq!(host.calls().starts.len(), 1, "a reply starts nothing");
}

#[tokio::test]
async fn a_stream_closes_on_the_question_and_a_subscription_follows_the_resumed_task() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config).await;
    let driver = Arc::clone(&host);
    let events = stream_while(
        &served,
        "SendStreamingMessage",
        json!({"message": user_message("stream me", None)}),
        async move {
            let id = driver.started(0).await;
            driver.followed(&id, 1).await;
            driver.note(&id, "looking");
            tokio::time::sleep(Duration::from_millis(50)).await;
            driver.ask(&id, question());
        },
    )
    .await;
    assert_eq!(notes(&events)[0], "looking", "{events:?}");
    assert_eq!(
        *final_state(&events),
        "TASK_STATE_INPUT_REQUIRED",
        "{events:?}"
    );
    let last = &events.last().unwrap()["result"]["statusUpdate"];
    assert_eq!(
        last["status"]["message"]["parts"][0]["text"],
        "Which branch?"
    );
    let id = last["taskId"].as_str().unwrap().to_string();
    let context = last["contextId"].as_str().unwrap().to_string();

    // Subscribing while it asks has nothing to follow but the question.
    let paused = stream_while(&served, "SubscribeToTask", json!({"id": id}), async {}).await;
    assert_eq!(paused.len(), 1, "{paused:?}");
    assert_eq!(
        paused[0]["result"]["task"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );

    let resumed = served
        .result(
            "SendMessage",
            json!({
                "message": reply(&id, &context, "dev")["message"],
                "configuration": {"returnImmediately": true},
            }),
        )
        .await;
    assert_eq!(resumed["task"]["status"]["state"], "TASK_STATE_WORKING");
    let driver = Arc::clone(&host);
    let events = stream_while(&served, "SubscribeToTask", json!({"id": id}), async move {
        driver.followed("run-0", 3).await;
        driver.note("run-0", "deploying dev");
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver.complete("run-0", answer());
    })
    .await;
    assert_eq!(notes(&events), ["deploying dev"], "{events:?}");
    assert_eq!(*final_state(&events), "TASK_STATE_COMPLETED", "{events:?}");
}

#[tokio::test]
async fn a_reply_is_refused_unless_its_task_is_asking_and_its_callers() {
    let host = FakeHost::held();
    let state = tempfile::tempdir().unwrap();
    let config = hosted(&state, &host)
        .with_principal(
            Principal::new("alice").unwrap(),
            BearerToken::new("alice-token").unwrap(),
        )
        .with_principal(
            Principal::new("bob").unwrap(),
            BearerToken::new("bob-token").unwrap(),
        );
    let served = Served::start_config(config, state, None).await;
    let alice = served.as_token("alice-token");
    let bob = served.as_token("bob-token");
    let task = alice.result("SendMessage", send("work", None, true)).await["task"].clone();
    let id = task["id"].as_str().unwrap();
    let context = task["contextId"].as_str().unwrap();

    // Not asking: the reply belongs in a new message on the context.
    let refused = alice.call("SendMessage", reply(id, context, "early")).await;
    assert_eq!(refused["error"]["code"], error_code::UNSUPPORTED_OPERATION);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not waiting for input"),
        "{refused}"
    );

    host.ask(&host.started(0).await, question());
    alice.wait_for(id, "TASK_STATE_INPUT_REQUIRED").await;
    assert_eq!(
        bob.error_code("SendMessage", reply(id, context, "mine now"))
            .await,
        error_code::TASK_NOT_FOUND
    );
    assert_eq!(
        alice
            .error_code("SendMessage", reply(id, "another-context", "main"))
            .await,
        error_code::INVALID_PARAMS
    );
    // The host's own refusal is the caller's to fix, and the task still asks.
    let refused = alice.call("SendMessage", reply(id, context, REFUSED)).await;
    assert_eq!(refused["error"]["code"], error_code::INVALID_PARAMS);
    assert_eq!(refused["error"]["message"], "not an answer to that");
    assert!(host.calls().replies.is_empty());
    alice.wait_for(id, "TASK_STATE_INPUT_REQUIRED").await;
}

#[tokio::test]
async fn a_host_that_takes_no_replies_refuses_them_by_default() {
    let host = FakeHost::held();
    let state = tempfile::tempdir().unwrap();
    let config = ServeConfig::hosted(
        identity(),
        state.path(),
        Arc::new(fake_host::NoReplies(Arc::clone(&host))),
    );
    let served = Served::start_config(config, state, None).await;
    let task = asked(&served, &host).await;
    let id = task["id"].as_str().unwrap();
    let refused = served
        .call(
            "SendMessage",
            reply(id, task["contextId"].as_str().unwrap(), "main"),
        )
        .await;
    assert_eq!(refused["error"]["code"], error_code::INVALID_PARAMS);
    assert_eq!(refused["error"]["message"], "this agent takes no replies");
}

#[tokio::test]
async fn a_task_asking_its_caller_is_live_and_cancelable() {
    let host = FakeHost::held();
    let served = serve(&host, |config| config.with_live_task_quota(1)).await;
    let task = asked(&served, &host).await;
    assert_eq!(
        served
            .error_code("SendMessage", send("another", None, true))
            .await,
        error_code::UNSUPPORTED_OPERATION,
        "a paused task still holds its place"
    );
    let canceled = served.result("CancelTask", json!({"id": task["id"]})).await;
    assert_eq!(
        canceled["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );
    assert_eq!(host.calls().cancels, ["run-0"]);
}

#[tokio::test]
async fn a_webhook_hears_a_hosted_task_ask_and_resume() {
    let host = FakeHost::held();
    let served = serve(&host, |config| {
        config.with_push_notifications().with_private_webhooks()
    })
    .await;
    let (url, bodies) = webhook();
    let task = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("call me back", None),
                "configuration": {
                    "returnImmediately": true,
                    "taskPushNotificationConfig": {"url": url},
                },
            }),
        )
        .await["task"]
        .clone();
    let id = task["id"].as_str().unwrap();
    let states = || -> Vec<String> {
        bodies
            .lock()
            .unwrap()
            .iter()
            .filter_map(|body| body["statusUpdate"]["status"]["state"].as_str())
            .map(str::to_string)
            .collect()
    };
    let heard = |state: &'static str, times: usize| {
        let states = &states;
        async move {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while states().iter().filter(|heard| *heard == state).count() < times {
                assert!(
                    std::time::Instant::now() < deadline,
                    "never heard {state}: {:?}",
                    states()
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    host.ask(&host.started(0).await, question());
    heard("TASK_STATE_INPUT_REQUIRED", 1).await;
    let working_before = states()
        .iter()
        .filter(|state| *state == "TASK_STATE_WORKING")
        .count();

    // The host asks again at once: the resumption is still heard, and so is
    // the second question.
    served
        .result(
            "SendMessage",
            json!({
                "message": reply(id, task["contextId"].as_str().unwrap(), "main")["message"],
                "configuration": {"returnImmediately": true},
            }),
        )
        .await;
    host.ask("run-0", vec![Part::text("And which region?")]);
    heard("TASK_STATE_WORKING", working_before + 1).await;
    heard("TASK_STATE_INPUT_REQUIRED", 2).await;
}
