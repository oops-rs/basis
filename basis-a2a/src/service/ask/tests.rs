use a2a::{Part, Role, error_code};
use serde_json::json;

use super::*;

fn request(tool: &str) -> ApprovalRequest {
    ApprovalRequest {
        request_id: "r".into(),
        tool_call_id: "t".into(),
        tool_name: tool.into(),
        description: "writes a file".into(),
        input: json!({"path": "a.txt"}),
        side_effect_level: None,
    }
}

async fn open_question(questions: &Questions) -> Asked {
    let mut watch = questions.watch();
    watch
        .wait_for(Option::is_some)
        .await
        .unwrap()
        .clone()
        .unwrap()
}

#[tokio::test]
async fn an_answer_for_the_open_call_unparks_it_and_another_is_refused() {
    let questions = Arc::new(Questions::new());
    let asking = {
        let questions = Arc::clone(&questions);
        tokio::spawn(async move {
            questions
                .ask(&request("write"), Duration::from_secs(30))
                .await
        })
    };
    let asked = open_question(&questions).await;
    assert_eq!(asked.tool, "write");
    assert!(asked.preview.contains("a.txt"), "{}", asked.preview);

    assert_eq!(
        questions.answer("not-the-call", true),
        Err(Unanswered::Mismatch {
            open: asked.call_id.clone()
        })
    );
    assert!(questions.asked().is_some(), "the question stays open");
    questions
        .answer(&asked.call_id.to_uppercase(), true)
        .unwrap();
    assert_eq!(asking.await.unwrap().decision, ApprovalDecision::Allow);
    assert!(questions.asked().is_none());
    assert_eq!(
        questions.answer(&asked.call_id, true),
        Err(Unanswered::NotAsking),
        "an answer does not outlive its question"
    );
}

#[tokio::test]
async fn the_deadline_answers_deny() {
    let questions = Questions::new();
    let answer = questions
        .ask(&request("write"), Duration::from_millis(20))
        .await;
    assert_eq!(answer.decision, ApprovalDecision::Deny);
    assert!(answer.reason.unwrap().contains("did not answer"));
    assert!(questions.asked().is_none());
}

#[tokio::test]
async fn questions_are_asked_one_at_a_time_under_fresh_ids() {
    let questions = Arc::new(Questions::new());
    let ask = |tool: &'static str| {
        let questions = Arc::clone(&questions);
        tokio::spawn(async move { questions.ask(&request(tool), Duration::from_secs(30)).await })
    };
    let first = ask("one");
    let first_asked = open_question(&questions).await;
    let second = ask("two");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        questions.asked().unwrap().call_id,
        first_asked.call_id,
        "the second waits"
    );
    questions.answer(&first_asked.call_id, false).unwrap();
    assert_eq!(first.await.unwrap().decision, ApprovalDecision::Deny);

    let mut watch = questions.watch();
    let second_asked = watch
        .wait_for(|asked| {
            asked
                .as_ref()
                .is_some_and(|asked| asked.call_id != first_asked.call_id)
        })
        .await
        .unwrap()
        .clone()
        .unwrap();
    assert_eq!(second_asked.tool, "two");
    questions.answer(&second_asked.call_id, true).unwrap();
    assert_eq!(second.await.unwrap().decision, ApprovalDecision::Allow);
}

#[tokio::test]
async fn a_cancel_denies_the_open_question_and_every_later_one() {
    let questions = Arc::new(Questions::new());
    let asking = {
        let questions = Arc::clone(&questions);
        tokio::spawn(async move {
            questions
                .ask(&request("write"), Duration::from_secs(30))
                .await
        })
    };
    open_question(&questions).await;
    questions.cancel();
    assert_eq!(asking.await.unwrap().decision, ApprovalDecision::Deny);
    let later = questions
        .ask(&request("write"), Duration::from_secs(30))
        .await;
    assert_eq!(later.decision, ApprovalDecision::Deny);
}

fn message(parts: Vec<Part>) -> Message {
    Message::new(Role::User, parts)
}

#[test]
fn an_answer_is_words_or_a_data_part() {
    assert_eq!(
        answer_of(&message(vec![Part::text("  Approve ABC123 \n")])).unwrap(),
        ("ABC123".to_string(), true)
    );
    assert_eq!(
        answer_of(&message(vec![Part::text("deny abc")])).unwrap(),
        ("abc".to_string(), false)
    );
    assert_eq!(
        answer_of(&message(vec![Part::data(
            json!({"callId": "abc", "decision": "approve"})
        )]))
        .unwrap(),
        ("abc".to_string(), true)
    );
    assert_eq!(
        answer_of(&message(vec![Part::data(
            json!({APPROVAL_EXTENSION: {"callId": "abc", "decision": "deny"}})
        )]))
        .unwrap(),
        ("abc".to_string(), false)
    );
}

#[test]
fn an_answer_without_a_decision_is_refused() {
    for parts in [
        vec![Part::text("sure, go ahead")],
        vec![Part::text("approve")],
        vec![Part::text("approve abc please")],
        vec![Part::data(json!({"callId": "abc"}))],
        vec![Part::data(json!({"callId": "abc", "decision": "maybe"}))],
    ] {
        assert_eq!(
            answer_of(&message(parts.clone())).unwrap_err().code,
            error_code::INVALID_PARAMS,
            "{parts:?}"
        );
    }
}
