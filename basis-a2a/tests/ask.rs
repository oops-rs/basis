//! Asking the caller (ADR-0030 §7, `docs/a2a/approval-v1.md`): a
//! consequential call parks its task at `INPUT_REQUIRED`, and a message on
//! the task answers it — or the deadline, a cancel, or a restart does.

#[path = "support/harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use basis_a2a::{A2aService, InboundApproval, Principal, ServeConfig, a2a::error_code};
use harness::{Reply, ScriptedEndpoint, Served, sse_events, user_message};
use serde_json::{Value, json};

const EXTENSION: &str = "https://github.com/oops-rs/basis/blob/main/docs/a2a/approval-v1.md";

fn write_then_finish() -> Vec<Reply> {
    vec![
        Reply::ToolCall {
            name: "write".to_string(),
            arguments: json!({"path": "note.txt", "content": "approved"}),
        },
        Reply::Text("finished".to_string()),
    ]
}

async fn asking(deadline: Duration, script: Vec<Reply>) -> Served {
    Served::start_with(ScriptedEndpoint::scripted(false, script), |config| {
        config.with_approval(InboundApproval::AskCaller { deadline })
    })
    .await
}

/// The call id a task's question names, read off its data part.
fn call_id(task: &Value) -> String {
    let message = &task["status"]["message"];
    assert_eq!(
        task["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{task}"
    );
    assert_eq!(message["extensions"][0], EXTENSION, "{message}");
    let question = &message["parts"][1]["data"][EXTENSION];
    assert_eq!(question["tool"], "write", "{message}");
    let id = question["callId"].as_str().unwrap().to_string();
    let text = message["parts"][0]["text"].as_str().unwrap();
    assert!(
        text.contains(&format!("approve {id}")) && text.contains(&format!("deny {id}")),
        "{text}"
    );
    id
}

fn reply(task: &Value, parts: Value) -> Value {
    json!({"message": {
        "messageId": format!("answer-{}", parts.to_string().len()),
        "role": "ROLE_USER",
        "taskId": task["id"],
        "parts": parts,
    }})
}

#[tokio::test]
async fn an_approved_call_is_made_and_the_card_declares_the_extension() {
    let served = asking(Duration::from_secs(30), write_then_finish()).await;
    let card = served.card().await;
    let declares = |card: &Value| {
        card["capabilities"]["extensions"]
            .as_array()
            .is_some_and(|extensions| {
                extensions
                    .iter()
                    .any(|extension| extension["uri"] == EXTENSION)
            })
    };
    assert!(declares(&card), "{card}");
    let never = Served::start().await;
    assert!(
        !declares(&never.card().await),
        "a server that never asks does not declare it"
    );

    let asked = served
        .result(
            "SendMessage",
            json!({"message": user_message("write it", None)}),
        )
        .await["task"]
        .clone();
    let id = call_id(&asked);

    let mismatch = served
        .call(
            "SendMessage",
            reply(&asked, json!([{"text": "approve not-the-call"}])),
        )
        .await;
    assert_eq!(mismatch["error"]["code"], error_code::INVALID_PARAMS);
    let undecided = served
        .call(
            "SendMessage",
            reply(&asked, json!([{"data": {"callId": id}}])),
        )
        .await;
    assert_eq!(undecided["error"]["code"], error_code::INVALID_PARAMS);
    let still = served.result("GetTask", json!({"id": asked["id"]})).await;
    assert_eq!(call_id(&still), id, "the question stays open");

    let answered = served
        .result(
            "SendMessage",
            reply(&asked, json!([{"text": format!("  APPROVE {id} ")}])),
        )
        .await;
    assert_eq!(
        answered["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{answered}"
    );
    let written = std::fs::read_to_string(served.workspace.path().join("note.txt")).unwrap();
    assert_eq!(written, "approved");
}

#[tokio::test]
async fn a_denied_call_is_not_made() {
    let served = asking(Duration::from_secs(30), write_then_finish()).await;
    let asked = served
        .result(
            "SendMessage",
            json!({"message": user_message("write it", None)}),
        )
        .await["task"]
        .clone();
    let id = call_id(&asked);
    let answered = served
        .result(
            "SendMessage",
            reply(
                &asked,
                json!([{"data": {EXTENSION: {"callId": id, "decision": "deny"}}}]),
            ),
        )
        .await;
    assert_eq!(answered["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    assert!(!served.workspace.path().join("note.txt").exists());
}

#[tokio::test]
async fn the_deadline_answers_deny() {
    let served = asking(Duration::from_millis(300), write_then_finish()).await;
    let sent = served
        .result(
            "SendMessage",
            json!({
                "message": user_message("write it", None),
                "configuration": {"returnImmediately": true},
            }),
        )
        .await;
    let id = sent["task"]["id"].as_str().unwrap();
    served.wait_for(id, "TASK_STATE_COMPLETED").await;
    assert!(!served.workspace.path().join("note.txt").exists());
}

#[tokio::test]
async fn a_cancel_while_parked_cancels() {
    let served = asking(Duration::from_secs(30), write_then_finish()).await;
    let asked = served
        .result(
            "SendMessage",
            json!({"message": user_message("write it", None)}),
        )
        .await["task"]
        .clone();
    call_id(&asked);
    served
        .result("CancelTask", json!({"id": asked["id"]}))
        .await;
    served
        .wait_for(asked["id"].as_str().unwrap(), "TASK_STATE_CANCELED")
        .await;
    assert!(!served.workspace.path().join("note.txt").exists());
}

#[tokio::test]
async fn a_stream_closes_on_the_question() {
    let served = asking(Duration::from_secs(30), write_then_finish()).await;
    let response = served
        .post(json!({
            "jsonrpc": "2.0", "id": 3, "method": "SendStreamingMessage",
            "params": {"message": user_message("write it", None)},
        }))
        .await;
    let body = response.text().await.unwrap();
    let events = sse_events(&body);
    let last = &events.last().unwrap()["result"]["statusUpdate"];
    assert_eq!(
        last["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{body}"
    );
}

#[tokio::test]
async fn a_reply_to_a_task_not_asking_is_still_refused() {
    let served = asking(Duration::from_secs(30), vec![Reply::Text("hi".into())]).await;
    let done = served
        .result(
            "SendMessage",
            json!({"message": user_message("hello", None)}),
        )
        .await["task"]
        .clone();
    let refused = served
        .call(
            "SendMessage",
            reply(&done, json!([{"text": "approve abc"}])),
        )
        .await;
    assert_eq!(refused["error"]["code"], error_code::UNSUPPORTED_OPERATION);
}

/// Where the crashing first life finds what the test set up for it.
const FIRST_LIFE: &str = "BASIS_A2A_TEST_FIRST_LIFE";

fn restart_config(data: &std::path::Path, workspace: &std::path::Path, model: &str) -> ServeConfig {
    ServeConfig::new(workspace, basis_a2a::AgentIdentity::new("helper", "Asks."))
        .with_data_dir(data)
        .with_run_defaults(
            basis_a2a::RunDefaults::default()
                .with_base_url(model)
                .with_model("test-model")
                .with_deadline(Duration::from_secs(60)),
        )
        .with_approval(InboundApproval::AskCaller {
            deadline: Duration::from_secs(60),
        })
        // Called in-process as the anonymous principal; nothing listens.
        .with_trusted_anonymous_callers()
}

/// The first life of [`a_restart_never_honors_the_dead_questions_answer`]:
/// run as a child process, which the test kills while its question is open.
/// A crash has to be a real one: shutting a service down in-process is the
/// graceful path, which [`a_shutdown_while_asking_records_nothing_and_the_next_start_asks_again`]
/// covers.
#[test]
#[ignore = "run as a child process by a_restart_never_honors_the_dead_questions_answer"]
fn first_life_that_crashes_while_asking() {
    let Ok(setup) = std::env::var(FIRST_LIFE) else {
        return;
    };
    harness::isolate_environment();
    let setup: Value = serde_json::from_str(&setup).unwrap();
    let config = restart_config(
        std::path::Path::new(setup["data"].as_str().unwrap()),
        std::path::Path::new(setup["workspace"].as_str().unwrap()),
        setup["model"].as_str().unwrap(),
    );
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let service = A2aService::open(config).unwrap();
        let request =
            serde_json::from_value(json!({"message": user_message("write it", None)})).unwrap();
        let response = service
            .send_message(&Principal::anonymous(), request)
            .await
            .unwrap();
        let task = serde_json::to_value(response).unwrap()["task"].clone();
        println!("ASKED {} {}", task["id"].as_str().unwrap(), call_id(&task));
        std::future::pending::<()>().await;
    });
}

#[test]
fn a_restart_never_honors_the_dead_questions_answer() {
    harness::isolate_environment();
    // The first life's turn asks, and dies asking.
    let write = Reply::ToolCall {
        name: "write".to_string(),
        arguments: json!({"path": "note.txt", "content": "approved"}),
    };
    let model = ScriptedEndpoint::scripted(
        false,
        vec![write.clone(), write, Reply::Text("finished".to_string())],
    );
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let setup = json!({
        "data": data.path(), "workspace": workspace.path(), "model": model.base_url,
    });

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "first_life_that_crashes_while_asking",
            "--ignored",
            "--nocapture",
        ])
        .env(FIRST_LIFE, setup.to_string())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(child.stdout.take().unwrap()));
    let asked = lines
        .find_map(|line| line.ok()?.strip_prefix("ASKED ").map(str::to_owned))
        .expect("the first life asks before it dies");
    let (task_id, old_id) = asked.split_once(' ').unwrap();
    child.kill().unwrap();
    child.wait().unwrap();

    let caller = Principal::anonymous();
    let reply_to = |text: String| -> basis_a2a::a2a::SendMessageRequest {
        serde_json::from_value(reply(&json!({"id": task_id}), json!([{"text": text}]))).unwrap()
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let service = A2aService::open(restart_config(
            data.path(),
            workspace.path(),
            &model.base_url,
        ))
        .unwrap();
        // The restarted server drives the task again. The turn died with its
        // call committed and unanswered, so it is not finished: it is
        // continued, with the call closed as having an unknown effect, and
        // the model's next attempt at the call asks again (ADR-0030 §7).
        let task = asked_again(&service, &caller, task_id).await;

        // No answer outlives the question it was given for.
        let stale = service
            .send_message(&caller, reply_to(format!("approve {old_id}")))
            .await;
        assert!(stale.is_err(), "the old question's answer is refused");
        assert!(
            !workspace.path().join("note.txt").exists(),
            "the call was never made on an answer nobody can give"
        );

        let new_id = call_id(&task);
        assert_ne!(new_id, old_id, "a re-run question has a fresh call id");
        let answered = service
            .send_message(&caller, reply_to(format!("approve {new_id}")))
            .await
            .unwrap();
        let answered = serde_json::to_value(answered).unwrap();
        assert_eq!(
            answered["task"]["status"]["state"], "TASK_STATE_COMPLETED",
            "{answered}"
        );
        assert_eq!(harness::artifact_text(&answered["task"]), "finished");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
            "approved"
        );
    });
}

/// Polls `task_id` until it is asking, which it must come to.
async fn asked_again(service: &A2aService, caller: &Principal, task_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let task = service
            .get_task(
                caller,
                serde_json::from_value(json!({"id": task_id})).unwrap(),
            )
            .unwrap();
        let task = serde_json::to_value(task).unwrap();
        let state = task["status"]["state"].as_str().unwrap().to_string();
        if state == "TASK_STATE_INPUT_REQUIRED" {
            return task;
        }
        assert!(
            state == "TASK_STATE_SUBMITTED" || state == "TASK_STATE_WORKING",
            "the task settled instead of asking again: {task}"
        );
        assert!(Instant::now() < deadline, "never asked again: {task}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_shutdown_while_asking_records_nothing_and_the_next_start_asks_again() {
    harness::isolate_environment();
    let write = Reply::ToolCall {
        name: "write".to_string(),
        arguments: json!({"path": "note.txt", "content": "approved"}),
    };
    let model = ScriptedEndpoint::scripted(
        false,
        vec![write.clone(), write, Reply::Text("finished".to_string())],
    );
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = || restart_config(data.path(), workspace.path(), &model.base_url);
    let caller = Principal::anonymous();

    let service = A2aService::open(config()).unwrap();
    let request =
        serde_json::from_value(json!({"message": user_message("write it", None)})).unwrap();
    let asked = service.send_message(&caller, request).await.unwrap();
    let asked = serde_json::to_value(asked).unwrap()["task"].clone();
    let task_id = asked["id"].as_str().unwrap().to_string();
    let old_id = call_id(&asked);

    tokio::time::timeout(Duration::from_secs(10), service.shutdown())
        .await
        .expect("a shutdown does not wait on the open question");
    let reply_to = |text: String| -> basis_a2a::a2a::SendMessageRequest {
        serde_json::from_value(reply(&json!({"id": task_id}), json!([{"text": text}]))).unwrap()
    };
    assert!(
        service
            .send_message(&caller, reply_to(format!("approve {old_id}")))
            .await
            .is_err(),
        "the interrupted question takes no answer"
    );
    let tasks = basis_tasks::Tasks::open_at(data.path(), workspace.path()).unwrap();
    let handle = basis_tasks::TaskHandle::parse(&tasks.list().unwrap()[0].task).unwrap();
    assert!(
        tasks.terminal(&handle).unwrap().is_none(),
        "no terminal state was recorded"
    );
    let standing = serde_json::to_value(
        service
            .get_task(
                &caller,
                serde_json::from_value(json!({"id": task_id})).unwrap(),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(
        ![
            "TASK_STATE_COMPLETED",
            "TASK_STATE_FAILED",
            "TASK_STATE_CANCELED"
        ]
        .contains(&standing["status"]["state"].as_str().unwrap()),
        "{standing}"
    );
    assert!(!workspace.path().join("note.txt").exists());
    drop(service);

    let service = A2aService::open(config()).unwrap();
    let task = asked_again(&service, &caller, &task_id).await;
    let new_id = call_id(&task);
    assert_ne!(new_id, old_id);
    let answered = service
        .send_message(&caller, reply_to(format!("approve {new_id}")))
        .await
        .unwrap();
    let answered = serde_json::to_value(answered).unwrap();
    assert_eq!(
        answered["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{answered}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
        "approved"
    );
    service.shutdown().await;
}
