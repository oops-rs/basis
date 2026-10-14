//! The conversation basis keeps for the model (ADR-0029 §5), decided without a
//! server or a wire: which request a call sends given where the conversation
//! stands, and where it stands after each kind of answer a peer can give.

use std::time::Duration;

use super::*;

const AGENT: &str = "reviewer";
/// A top-level caller's first hop, with no deadline.
const TOP: Hop = Hop {
    hop: 1,
    deadline: None,
};
/// The endpoint every exchange here is sent to.
const ENDPOINT: &str = "endpoint-1";

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

fn sending(text: &str, answering: Option<&str>, context: Option<&str>) -> Request {
    Request::Send(
        Message::new(text)
            .in_context(context.map(str::to_string))
            .answering(answering.map(str::to_string))
            .at_hop(1, None),
    )
}

#[test]
fn a_first_task_opens_a_conversation() {
    assert_eq!(
        request(AGENT, "look at this diff", &Conversation::default(), TOP),
        Ok(sending("look at this diff", None, None))
    );
}

#[test]
fn a_later_task_continues_the_same_conversation() {
    let settled = Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: None,
    };

    assert_eq!(
        request(AGENT, "and this one", &settled, TOP),
        Ok(sending("and this one", None, Some("context-1")))
    );
}

#[test]
fn the_agent_named_alone_waits_on_its_outstanding_task() {
    assert_eq!(
        request(AGENT, "", &outstanding(Waiting::Agent), TOP),
        Ok(Request::wait("task-1"))
    );
}

#[test]
fn naming_the_agent_alone_with_nothing_outstanding_is_taught_the_task_form() {
    let refusal =
        request(AGENT, "", &Conversation::default(), TOP).expect_err("nothing to wait on");

    assert!(refusal.contains("`@reviewer <task>`"), "{refusal}");
}

#[test]
fn a_new_task_while_one_is_working_is_refused_not_sent() {
    // A remote conversation runs one task at a time; sending would either be
    // refused remotely or start a second task the model never waits on.
    let refusal = request(AGENT, "something else", &outstanding(Waiting::Agent), TOP)
        .expect_err("still working");

    assert!(refusal.contains("`@reviewer` alone"), "{refusal}");
}

#[test]
fn text_for_an_agent_waiting_on_input_answers_its_question() {
    assert_eq!(
        request(
            AGENT,
            "yes, the main branch",
            &outstanding(Waiting::Input),
            TOP
        ),
        Ok(sending(
            "yes, the main branch",
            Some("task-1"),
            Some("context-1")
        ))
    );
}

#[test]
fn every_message_says_which_hop_it_is_and_when_this_side_stops_waiting() {
    let deadline = std::time::UNIX_EPOCH + Duration::from_millis(1_791_000_000_123);
    let hop = Hop {
        hop: 3,
        deadline: Some(deadline),
    };

    let Ok(Request::Send(message)) = request(AGENT, "look", &Conversation::default(), hop) else {
        panic!("a task is sent");
    };

    assert_eq!(message.hop, 3);
    assert_eq!(message.deadline, Some(deadline));
}

fn finished(text: &str) -> Result<Outcome, PeerError> {
    Ok(Outcome::Finished {
        task_id: "task-1".to_string(),
        context_id: Some("context-1".to_string()),
        text: text.to_string(),
    })
}

#[test]
fn a_completed_task_is_the_answer_and_leaves_nothing_outstanding() {
    let (next, result) = settle(
        AGENT,
        Conversation::default(),
        finished("looks good"),
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
fn a_reply_with_no_task_keeps_the_context_it_had() {
    let settled = Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: None,
    };
    let (next, result) = settle(
        AGENT,
        settled.clone(),
        Ok(Outcome::Replied {
            context_id: None,
            text: "quick answer".to_string(),
        }),
        ENDPOINT,
    );

    assert!(result.expect("replied").contains("quick answer"));
    assert_eq!(next, settled);
}

#[test]
fn a_task_still_working_is_kept_and_the_model_is_told_how_to_wait() {
    let (next, result) = settle(
        AGENT,
        Conversation::default(),
        Ok(Outcome::Working {
            task_id: "task-1".to_string(),
            context_id: Some("context-1".to_string()),
            text: String::new(),
            why: StillWorking::WaitBound(Duration::from_secs(300)),
        }),
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
        Ok(Outcome::NeedsInput {
            task_id: "task-1".to_string(),
            context_id: Some("context-1".to_string()),
            text: "which branch?".to_string(),
        }),
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
        Ok(Outcome::Failed {
            task_id: "task-1".to_string(),
            context_id: Some("context-1".to_string()),
            text: String::new(),
            how: Failure::Failed,
        }),
        ENDPOINT,
    );

    let error = result.expect_err("the work was not done");
    assert!(error.contains("failed"), "{error}");
    assert_eq!(next.outstanding, None);
    assert_eq!(next.context_id.as_deref(), Some("context-1"));
}

#[test]
fn an_outstanding_task_the_agent_no_longer_knows_is_forgotten_and_said_so() {
    // The agent moved to a new endpoint while its task was outstanding:
    // `@reviewer` alone resumes against the new one, which has never heard of
    // it (ADR-0031 §5).
    let (next, result) = settle(
        AGENT,
        outstanding(Waiting::Agent),
        Err(PeerError::UnknownTask),
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
        Err(PeerError::UnknownTask),
        ENDPOINT,
    );

    assert!(result.expect_err("failed").contains("Handing the task"));
    assert_eq!(next, settled);
}

#[test]
fn a_moved_agent_that_cannot_pick_up_its_task_ends_the_record_whatever_it_answered() {
    // The task was handed to `endpoint-0`; the agent has since moved, and its
    // new endpoint answers the resume with something other than "unknown
    // task". Keeping the record would wedge the conversation.
    let recorded = Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: Some(Outstanding {
            task_id: "task-1".to_string(),
            waiting_for: Waiting::Agent,
            endpoint: Some("endpoint-0".to_string()),
        }),
    };
    let answers = [
        PeerError::Send("could not decode the answer".to_string()),
        PeerError::Send("invalid params".to_string()),
        PeerError::Check {
            task_id: "task-1".to_string(),
            error: "invalid id".to_string(),
        },
        PeerError::UnknownTask,
    ];

    for answer in answers {
        let (next, result) = settle(AGENT, recorded.clone(), Err(answer), ENDPOINT);

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
        Err(PeerError::Send("connection reset".to_string())),
        ENDPOINT,
    );

    assert!(result.is_err());
    assert_eq!(next, outstanding(Waiting::Agent));
}

#[test]
fn a_failed_check_keeps_the_task_and_says_how_to_check_again() {
    let (next, result) = settle(
        AGENT,
        outstanding(Waiting::Agent),
        Err(PeerError::Check {
            task_id: "task-1".to_string(),
            error: "timed out".to_string(),
        }),
        ENDPOINT,
    );

    let error = result.expect_err("the check failed");
    assert!(error.contains("`@reviewer` alone"), "{error}");
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
        Err(PeerError::Send("connection reset".to_string())),
        ENDPOINT,
    );

    assert_eq!(next, legacy);
}

#[test]
fn a_task_left_outstanding_records_the_endpoint_it_was_handed_to() {
    let (next, _) = settle(
        AGENT,
        Conversation::default(),
        Ok(Outcome::Working {
            task_id: "task-1".to_string(),
            context_id: Some("context-1".to_string()),
            text: String::new(),
            why: StillWorking::WaitBound(Duration::from_secs(1)),
        }),
        ENDPOINT,
    );

    assert_eq!(
        next.outstanding
            .and_then(|outstanding| outstanding.endpoint),
        Some(ENDPOINT.to_string())
    );
}

#[test]
fn an_abandoned_task_ends_the_record_and_says_whether_the_cancel_went_through() {
    let (next, result) = settle(
        AGENT,
        outstanding(Waiting::Agent),
        Err(PeerError::Abandoned {
            task_id: "task-1".to_string(),
            canceled: Err("unreachable".to_string()),
        }),
        ENDPOINT,
    );

    let error = result.expect_err("cancelled");
    assert!(
        error.contains("asking the task to cancel failed"),
        "{error}"
    );
    assert_eq!(next.outstanding, None);
}

#[cfg(feature = "a2a")]
mod configured {
    //! The refusals the preview gives, against the configured set.

    use super::*;

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

    #[tokio::test]
    async fn a_removed_agent_is_refused_by_name_and_continues_when_it_comes_back() {
        use crate::a2a::configured::{
            ConfiguredAgents,
            testing::{agent_at, free_port},
        };
        use std::sync::Arc;

        let configured = Arc::new(ConfiguredAgents::unreachable(&["reviewer"]));
        let agents = RemoteAgents::over_configured(&configured);
        let settled = Conversation {
            context_id: Some("context-1".to_string()),
            outstanding: None,
        };
        agents
            .record("agent-1", AGENT, settled.clone(), &Remembered::Not)
            .expect("in memory");

        configured.reload(Vec::new()).await;
        let refusal = authorize(&agents, AGENT).expect_err("removed");
        assert!(
            refusal.contains("`reviewer` is no longer configured"),
            "{refusal}"
        );

        configured.reload(vec![agent_at(AGENT, free_port())]).await;
        assert_eq!(
            request(
                AGENT,
                "and this one",
                &agents.conversation("agent-1", AGENT, &Remembered::Not),
                TOP
            ),
            Ok(sending("and this one", None, Some("context-1"))),
            "the same name, the same conversation"
        );
    }

    #[tokio::test]
    async fn an_outstanding_task_follows_its_agent_to_a_new_address() {
        use crate::a2a::configured::{
            ConfiguredAgents,
            testing::{agent_at, free_port, serve_card},
        };
        use std::sync::Arc;

        let configured = Arc::new(ConfiguredAgents::unreachable(&["reviewer"]));
        let agents = RemoteAgents::over_configured(&configured);
        agents
            .record(
                "agent-1",
                AGENT,
                outstanding(Waiting::Agent),
                &Remembered::Not,
            )
            .expect("in memory");
        let moved_to = free_port();
        serve_card(moved_to);

        let report = configured.reload(vec![agent_at(AGENT, moved_to)]).await;
        assert_eq!(report.changed, [AGENT]);

        let reached = agents
            .resolve(AGENT)
            .await
            .expect("the new address answers");
        assert!(
            reached.endpoint != ENDPOINT,
            "the fingerprint names the new configuration"
        );
        assert_eq!(
            request(
                AGENT,
                "",
                &agents.conversation("agent-1", AGENT, &Remembered::Not),
                TOP
            ),
            Ok(Request::wait("task-1")),
            "`@reviewer` alone waits on the same task, now asked of the new address"
        );
    }
}
