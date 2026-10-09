//! The conversation basis keeps for the model (ADR-0029 §5), decided without a
//! server: which request a call sends given where the conversation stands, and
//! where it stands after each kind of answer.

use std::time::Duration;

use mentra::a2a::types::{Artifact, Part, Task, TaskStatus};

use super::*;

const AGENT: &str = "reviewer";
const OUTPUT_BYTES: usize = 64 * 1024;

fn task(state: TaskState, answer: Option<&str>) -> Task {
    Task {
        id: "task-1".to_string(),
        context_id: "context-1".to_string(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        artifacts: answer.map(|text| {
            vec![Artifact {
                artifact_id: "result".to_string(),
                name: None,
                description: None,
                parts: vec![Part::text(text.to_string())],
                metadata: None,
                extensions: None,
            }]
        }),
        history: None,
        metadata: None,
    }
}

fn outstanding(waiting_for: Waiting) -> Conversation {
    Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: Some(Outstanding {
            task_id: "task-1".to_string(),
            waiting_for,
        }),
    }
}

#[test]
fn a_first_task_opens_a_conversation() {
    assert_eq!(
        request(AGENT, "look at this diff", &Conversation::default()),
        Ok(A2aRequest::Send {
            text: "look at this diff".to_string(),
            task_id: None,
            context_id: None,
        })
    );
}

#[test]
fn a_later_task_continues_the_same_conversation() {
    let settled = Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: None,
    };

    assert_eq!(
        request(AGENT, "and this one", &settled),
        Ok(A2aRequest::Send {
            text: "and this one".to_string(),
            task_id: None,
            context_id: Some("context-1".to_string()),
        })
    );
}

#[test]
fn the_agent_named_alone_waits_on_its_outstanding_task() {
    assert_eq!(
        request(AGENT, "", &outstanding(Waiting::Agent)),
        Ok(A2aRequest::Resume {
            task_id: "task-1".to_string()
        })
    );
}

#[test]
fn naming_the_agent_alone_with_nothing_outstanding_is_taught_the_task_form() {
    let refusal = request(AGENT, "", &Conversation::default()).expect_err("nothing to wait on");

    assert!(refusal.contains("`@reviewer <task>`"), "{refusal}");
}

#[test]
fn a_new_task_while_one_is_working_is_refused_not_sent() {
    // A remote conversation runs one task at a time; sending would either be
    // refused remotely or start a second task the model never waits on.
    let refusal =
        request(AGENT, "something else", &outstanding(Waiting::Agent)).expect_err("still working");

    assert!(refusal.contains("`@reviewer` alone"), "{refusal}");
}

#[test]
fn text_for_an_agent_waiting_on_input_answers_its_question() {
    assert_eq!(
        request(AGENT, "yes, the main branch", &outstanding(Waiting::Input)),
        Ok(A2aRequest::Send {
            text: "yes, the main branch".to_string(),
            task_id: Some("task-1".to_string()),
            context_id: Some("context-1".to_string()),
        })
    );
}

#[test]
fn a_completed_task_is_the_answer_and_leaves_nothing_outstanding() {
    let (next, result) = settle(
        AGENT,
        Conversation::default(),
        Ok(A2aExchange::Settled(task(
            TaskState::Completed,
            Some("looks good"),
        ))),
        OUTPUT_BYTES,
    );

    let text = result.expect("completed");
    assert!(
        text.starts_with("Remote agent `reviewer` replied:"),
        "{text}"
    );
    assert!(text.contains("looks good"), "{text}");
    assert_eq!(
        next,
        Conversation {
            context_id: Some("context-1".to_string()),
            outstanding: None,
        }
    );
}

#[test]
fn a_task_still_working_is_kept_and_the_model_is_told_how_to_wait() {
    let (next, result) = settle(
        AGENT,
        Conversation::default(),
        Ok(A2aExchange::StillWorking {
            task: task(TaskState::Working, None),
            reason: A2aStillWorking::WaitBound(Duration::from_secs(300)),
        }),
        OUTPUT_BYTES,
    );

    let text = result.expect("not an error: the work continues");
    assert!(text.contains("within 300s"), "{text}");
    assert!(text.contains("`@reviewer` alone"), "{text}");
    assert!(
        !text.contains("task-1") && !text.contains("context-1"),
        "the model never carries an id: {text}"
    );
    assert_eq!(next, outstanding(Waiting::Agent));
}

#[test]
fn a_question_from_the_agent_is_kept_and_the_model_is_told_how_to_answer() {
    let (next, result) = settle(
        AGENT,
        Conversation::default(),
        Ok(A2aExchange::Settled(task(
            TaskState::InputRequired,
            Some("which branch?"),
        ))),
        OUTPUT_BYTES,
    );

    let text = result.expect("a question is not a failure");
    assert!(text.contains("`@reviewer <your answer>`"), "{text}");
    assert!(text.contains("which branch?"), "{text}");
    assert_eq!(next, outstanding(Waiting::Input));
}

#[test]
fn a_failed_task_is_an_error_and_ends_what_was_outstanding() {
    let (next, result) = settle(
        AGENT,
        outstanding(Waiting::Agent),
        Ok(A2aExchange::Settled(task(TaskState::Failed, None))),
        OUTPUT_BYTES,
    );

    let error = result.expect_err("the work was not done");
    assert!(error.contains("failed"), "{error}");
    assert_eq!(next.outstanding, None);
    assert_eq!(next.context_id.as_deref(), Some("context-1"));
}
