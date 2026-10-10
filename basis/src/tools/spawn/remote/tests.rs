//! The conversation basis keeps for the model (ADR-0029 §5), decided without a
//! server: which request a call sends given where the conversation stands, and
//! where it stands after each kind of answer.

use std::time::Duration;

use mentra::a2a::types::{Artifact, Part, Task, TaskStatus};

use super::*;
use crate::a2a::remote::testing::{agent_at, free_port, serve_card};

const AGENT: &str = "reviewer";
const OUTPUT_BYTES: usize = 64 * 1024;
/// The endpoint every exchange here is sent to.
const ENDPOINT: &str = "endpoint-1";

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
            endpoint: Some(ENDPOINT.to_string()),
        }),
    }
}

#[test]
fn a_first_task_opens_a_conversation() {
    assert_eq!(
        request(AGENT, "look at this diff", &Conversation::default()),
        Ok(send("look at this diff", None, None))
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
        Ok(send("and this one", None, Some("context-1")))
    );
}

#[test]
fn the_agent_named_alone_waits_on_its_outstanding_task() {
    assert_eq!(
        request(AGENT, "", &outstanding(Waiting::Agent)),
        Ok(A2aRequest::resume("task-1"))
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
        Ok(send(
            "yes, the main branch",
            Some("task-1"),
            Some("context-1")
        ))
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
        ENDPOINT,
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
        ENDPOINT,
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
        ENDPOINT,
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
        ENDPOINT,
    );

    let error = result.expect_err("the work was not done");
    assert!(error.contains("failed"), "{error}");
    assert_eq!(next.outstanding, None);
    assert_eq!(next.context_id.as_deref(), Some("context-1"));
}

#[test]
fn an_agent_tried_too_recently_is_refused_with_how_long_to_wait() {
    // Configured, never reached, and its connect was attempted just now.
    let agents = RemoteAgents::unreachable(&["reviewer"]);

    let refusal = authorize(&agents, AGENT).expect_err("not worth asking yet");

    assert!(refusal.contains("could not be reached"), "{refusal}");
    assert!(refusal.contains("tries it again in 30s"), "{refusal}");
}

#[test]
fn a_name_the_workspace_does_not_configure_is_refused_naming_the_ones_it_does() {
    let agents = RemoteAgents::unreachable(&["reviewer", "planner"]);

    let refusal = authorize(&agents, "nobody").expect_err("not configured");

    assert!(
        refusal.contains("`reviewer`, `planner`"),
        "an unreachable agent is still one the model may ask for: {refusal}"
    );
    assert!(refusal.contains("`@@`"), "{refusal}");
}

#[test]
fn a_workspace_with_no_agents_says_so() {
    let refusal = authorize(&RemoteAgents::unreachable(&[]), AGENT).expect_err("none");

    assert!(refusal.contains("no remote agents"), "{refusal}");
}

fn not_found(method: &'static str) -> ClientError {
    ClientError::Rpc {
        method,
        error: A2aRpcError::TaskNotFound,
    }
}

#[test]
fn an_outstanding_task_the_agent_no_longer_knows_is_forgotten_and_said_so() {
    // The agent moved to a new endpoint while its task was outstanding:
    // `@reviewer` alone resumes against the new one, which has never heard of
    // it (ADR-0031 §5).
    let (next, result) = settle(
        AGENT,
        outstanding(Waiting::Agent),
        Err(A2aExchangeError::Send(not_found("GetTask"))),
        OUTPUT_BYTES,
        ENDPOINT,
    );

    let error = result.expect_err("the work was not done");
    assert!(error.contains("no longer knows the task"), "{error}");
    assert!(error.contains("`@reviewer <task>`"), "{error}");
    assert!(
        !error.contains("task-1") && !error.contains("context-1"),
        "the model never carries an id: {error}"
    );
    assert_eq!(next, Conversation::default(), "nothing is left to wait on");
}

#[test]
fn a_task_that_vanishes_mid_wait_is_forgotten_too() {
    let (next, result) = settle(
        AGENT,
        outstanding(Waiting::Agent),
        Err(A2aExchangeError::Poll {
            task_id: "task-1".to_string(),
            error: not_found("GetTask"),
        }),
        OUTPUT_BYTES,
        ENDPOINT,
    );

    assert!(result.is_err());
    assert_eq!(next, Conversation::default());
}

#[test]
fn a_new_task_refused_as_unknown_with_nothing_outstanding_keeps_the_conversation() {
    // Only an *outstanding* record is ended by not-found: a fresh send that
    // fails says why and leaves the conversation where it was.
    let settled = Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: None,
    };

    let (next, result) = settle(
        AGENT,
        settled.clone(),
        Err(A2aExchangeError::Send(not_found("SendMessage"))),
        OUTPUT_BYTES,
        ENDPOINT,
    );

    assert!(result.expect_err("failed").contains("Handing the task"));
    assert_eq!(next, settled);
}

#[tokio::test]
async fn a_removed_agent_is_refused_by_name_and_continues_when_it_comes_back() {
    let agents = RemoteAgents::unreachable(&["reviewer"]);
    let settled = Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: None,
    };
    agents
        .record("agent-1", AGENT, settled.clone(), false)
        .expect("in memory");

    agents.reload(Vec::new()).await;
    let refusal = authorize(&agents, AGENT).expect_err("removed");
    assert!(
        refusal.contains("`reviewer` is no longer configured"),
        "{refusal}"
    );

    agents.reload(vec![agent_at(AGENT, free_port())]).await;
    assert_eq!(
        request(
            AGENT,
            "and this one",
            &agents.conversation("agent-1", AGENT, false)
        ),
        Ok(send("and this one", None, Some("context-1"))),
        "the same name, the same conversation"
    );
}

#[tokio::test]
async fn an_outstanding_task_follows_its_agent_to_a_new_address() {
    let agents = RemoteAgents::unreachable(&["reviewer"]);
    agents
        .record("agent-1", AGENT, outstanding(Waiting::Agent), false)
        .expect("in memory");
    let moved_to = free_port();
    serve_card(moved_to);

    let report = agents.reload(vec![agent_at(AGENT, moved_to)]).await;
    assert_eq!(report.changed, [AGENT]);

    let reached = agents.reach(AGENT).await.expect("the new address answers");
    assert_eq!(reached.client.card_url().port(), Some(moved_to));
    assert_eq!(
        request(AGENT, "", &agents.conversation("agent-1", AGENT, false)),
        Ok(A2aRequest::resume("task-1")),
        "`@reviewer` alone waits on the same task, now asked of the new address"
    );
}

#[test]
fn a_moved_agent_that_cannot_pick_up_its_task_ends_the_record_whatever_it_answered() {
    // The task was handed to `endpoint-0`; the agent has since moved, and its
    // new endpoint answers the resume with something other than
    // `TaskNotFound`. Keeping the record would wedge the conversation.
    let recorded = Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: Some(Outstanding {
            task_id: "task-1".to_string(),
            waiting_for: Waiting::Agent,
            endpoint: Some("endpoint-0".to_string()),
        }),
    };
    let answers = [
        ClientError::Decode { method: "GetTask" },
        ClientError::Rpc {
            method: "GetTask",
            error: A2aRpcError::InvalidParams,
        },
        ClientError::InvalidId { method: "GetTask" },
    ];

    for answer in answers {
        let (next, result) = settle(
            AGENT,
            recorded.clone(),
            Err(A2aExchangeError::Send(answer)),
            OUTPUT_BYTES,
            ENDPOINT,
        );

        let error = result.expect_err("the task was not picked up");
        assert!(error.contains("has moved to a new address"), "{error}");
        assert!(error.contains("`@reviewer <task>`"), "{error}");
        assert!(
            !error.contains("task-1") && !error.contains("context-1"),
            "{error}"
        );
        assert_eq!(next, Conversation::default());
    }
}

#[test]
fn an_agent_that_did_not_move_keeps_its_task_through_a_failed_check() {
    // The same endpoint failing to answer is a hiccup, not a move: the task
    // may still be running, so the record stands.
    let (next, result) = settle(
        AGENT,
        outstanding(Waiting::Agent),
        Err(A2aExchangeError::Send(ClientError::Transport(
            "connection reset",
        ))),
        OUTPUT_BYTES,
        ENDPOINT,
    );

    assert!(result.is_err());
    assert_eq!(next, outstanding(Waiting::Agent));
}

#[test]
fn a_record_from_before_endpoints_were_kept_is_not_taken_to_have_moved() {
    let legacy = Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: Some(Outstanding {
            task_id: "task-1".to_string(),
            waiting_for: Waiting::Agent,
            endpoint: None,
        }),
    };

    let (next, _) = settle(
        AGENT,
        legacy.clone(),
        Err(A2aExchangeError::Send(ClientError::Connect)),
        OUTPUT_BYTES,
        ENDPOINT,
    );

    assert_eq!(next, legacy);
}

#[test]
fn a_task_left_outstanding_records_the_endpoint_it_was_handed_to() {
    let (next, _) = settle(
        AGENT,
        Conversation::default(),
        Ok(A2aExchange::StillWorking {
            task: task(TaskState::Working, None),
            reason: A2aStillWorking::WaitBound(Duration::from_secs(1)),
        }),
        OUTPUT_BYTES,
        ENDPOINT,
    );

    assert_eq!(
        next.outstanding
            .and_then(|outstanding| outstanding.endpoint),
        Some(ENDPOINT.to_string())
    );
}

#[test]
fn an_endpoint_fingerprint_follows_the_url_and_header_names_but_not_values() {
    use crate::a2a::A2aAgent;
    let agent = |url: &str, token: &str| {
        A2aAgent::new(
            mentra::a2a::A2aAgentConfig::new(AGENT, url).with_header("Authorization", token),
        )
    };

    let here = agent("https://a.example", "Bearer one");
    assert_eq!(
        here.endpoint(),
        agent("https://a.example", "Bearer two").endpoint()
    );
    assert_ne!(
        here.endpoint(),
        agent("https://b.example", "Bearer one").endpoint()
    );
    assert!(!here.endpoint().contains("one"));
}
