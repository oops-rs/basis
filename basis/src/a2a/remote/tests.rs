//! A remembered agent's conversations outlive the process that recorded them
//! (ADR-0029 §5); a delegated child's, and an ephemeral runtime's, do not.
//! And the set they are kept beside is live (ADR-0031): a reload reports what
//! it changed by name and keeps every conversation whatever it did.

use std::time::{Duration, Instant};

use super::*;
use crate::a2a::A2aReload;

fn working() -> Conversation {
    Conversation {
        context_id: Some("context-1".to_string()),
        outstanding: Some(Outstanding {
            task_id: "task-1".to_string(),
            waiting_for: Waiting::Agent,
            endpoint: None,
        }),
    }
}

#[test]
fn a_remembered_agent_finds_its_outstanding_task_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let before = RemoteAgents::unreachable_in(&["reviewer"], dir.path());
    before
        .record("agent-1", "reviewer", working(), &Remembered::ByAgent)
        .expect("written through");
    drop(before);

    // A new process: nothing in memory, the same directory on disk.
    let after = RemoteAgents::unreachable_in(&["reviewer"], dir.path());

    assert_eq!(
        after.conversation("agent-1", "reviewer", &Remembered::ByAgent),
        working()
    );
    assert_eq!(
        after.conversation("agent-2", "reviewer", &Remembered::ByAgent),
        Conversation::default(),
        "another session's record is its own"
    );
}

#[test]
fn a_child_keeps_its_conversation_in_memory_only() {
    let dir = tempfile::tempdir().unwrap();
    let agents = RemoteAgents::unreachable_in(&["reviewer"], dir.path());

    agents
        .record("child-1", "reviewer", working(), &Remembered::Not)
        .expect("nothing to write");

    assert_eq!(
        agents.conversation("child-1", "reviewer", &Remembered::Not),
        working()
    );
    assert!(
        std::fs::read_dir(dir.path()).is_err()
            || std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "a delegated child is never resumed, so nothing is written for it"
    );
}

#[test]
fn a_runtime_that_keeps_no_history_keeps_no_record() {
    let agents = RemoteAgents::unreachable(&["reviewer"]);

    agents
        .record("agent-1", "reviewer", working(), &Remembered::ByAgent)
        .expect("nothing to write");

    assert_eq!(
        agents.conversation("agent-1", "reviewer", &Remembered::ByAgent),
        working()
    );
}

#[test]
fn forgetting_a_conversation_removes_its_record() {
    let dir = tempfile::tempdir().unwrap();
    let agents = RemoteAgents::unreachable_in(&["reviewer"], dir.path());
    agents
        .record("agent-1", "reviewer", working(), &Remembered::ByAgent)
        .expect("written through");

    forget(dir.path(), "agent-1").expect("removed");
    forget(dir.path(), "agent-1").expect("and absent is not an error");

    let after = RemoteAgents::unreachable_in(&["reviewer"], dir.path());
    assert_eq!(
        after.conversation("agent-1", "reviewer", &Remembered::ByAgent),
        Conversation::default()
    );
}

#[test]
fn an_id_that_is_not_a_file_name_is_never_made_into_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let agents = RemoteAgents::unreachable_in(&["reviewer"], dir.path());

    agents
        .record("../escape", "reviewer", working(), &Remembered::ByAgent)
        .expect("kept in memory");

    assert!(!dir.path().join("../escape.json").exists());
    assert_eq!(
        agents.conversation("../escape", "reviewer", &Remembered::ByAgent),
        working()
    );
}

fn by_key(key: &str) -> Remembered {
    Remembered::ByKey(key.to_string())
}

#[test]
fn runs_bound_to_one_key_share_its_record_across_opens_and_agents() {
    let dir = tempfile::tempdir().unwrap();
    let first = RemoteAgents::unreachable_in(&["nous"], dir.path());
    first
        .record("agent-1", "nous", working(), &by_key("chat-7"))
        .expect("written through");

    // Another open on the same store, another minted agent: the same record,
    // read afresh rather than from anything cached.
    let second = RemoteAgents::unreachable_in(&["nous"], dir.path());
    assert_eq!(
        second.conversation("agent-2", "nous", &by_key("chat-7")),
        working()
    );
    assert_eq!(second.keyed_conversation("chat-7", "nous"), Some(working()));
    assert_eq!(
        second.conversation("agent-2", "nous", &Remembered::ByAgent),
        Conversation::default(),
        "the agent's own record is apart from the key's"
    );

    // A change through one open is what the other reads next.
    second
        .record(
            "agent-2",
            "nous",
            Conversation::default(),
            &by_key("chat-7"),
        )
        .expect("written through");
    assert_eq!(
        first.conversation("agent-1", "nous", &by_key("chat-7")),
        Conversation::default()
    );
    assert!(dir.path().join("keys/chat-7.json").is_file());
}

#[test]
fn different_keys_are_isolated_and_one_record_keeps_every_agent() {
    let dir = tempfile::tempdir().unwrap();
    let agents = RemoteAgents::unreachable_in(&["nous", "reviewer"], dir.path());
    agents
        .record("agent-1", "nous", working(), &by_key("chat-1"))
        .unwrap();
    agents
        .record("agent-1", "reviewer", working(), &by_key("chat-1"))
        .unwrap();

    assert_eq!(agents.keyed_conversation("chat-2", "nous"), None);
    assert_eq!(
        agents.conversation("agent-1", "nous", &by_key("chat-2")),
        Conversation::default()
    );
    assert_eq!(
        agents.keyed_conversation("chat-1", "reviewer"),
        Some(working())
    );
    assert_eq!(agents.keyed_conversation("chat-1", "nous"), Some(working()));
}

#[test]
fn a_runtime_that_keeps_no_history_shares_a_key_in_memory() {
    let agents = RemoteAgents::unreachable(&["nous"]);
    agents
        .record("agent-1", "nous", working(), &by_key("chat-1"))
        .unwrap();
    assert_eq!(
        agents.conversation("agent-2", "nous", &by_key("chat-1")),
        working()
    );
}

#[test]
fn a_key_is_never_made_into_a_path_outside_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let agents = RemoteAgents::unreachable_in(&["nous"], &store);
    for key in [
        "../../escape",
        "/etc/passwd",
        "..",
        "",
        "a/b",
        "Chat",
        "chat",
    ] {
        agents
            .record("agent-1", "nous", working(), &by_key(key))
            .unwrap_or_else(|error| panic!("{key:?}: {error}"));
        assert_eq!(
            agents.keyed_conversation(key, "nous"),
            Some(working()),
            "{key:?}"
        );
    }
    let written: Vec<String> = std::fs::read_dir(store.join("keys"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(written.len(), 7, "each key its own file: {written:?}");
    assert!(
        written
            .iter()
            .all(|name| name.ends_with(".json") && !name.contains('/') && !name.starts_with('.')),
        "{written:?}"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "nothing beside the store"
    );
}

#[test]
fn a_plain_key_is_its_own_file_name_and_any_other_is_made_one() {
    assert_eq!(conversation::file_name_of("chat-7_a"), "chat-7_a");
    let made = conversation::file_name_of("../x");
    assert!(made.starts_with("___x~"), "{made}");
    assert_ne!(
        conversation::file_name_of("Chat"),
        conversation::file_name_of("chat"),
        "a case-insensitive file system cannot confuse them"
    );
    assert_ne!(
        conversation::file_name_of("a/b"),
        conversation::file_name_of("a_b")
    );
    assert_eq!(
        conversation::file_name_of("a/b"),
        conversation::file_name_of("a/b"),
        "stable"
    );
    assert!(conversation::file_name_of(&"x".repeat(500)).len() <= 64 + 17);
}

/// An agent at a port nothing listens on, so connecting fails fast and the
/// diff is all that is under test.
fn down(name: &str, port: u16) -> A2aAgent {
    testing::agent_at(name, port)
}

#[tokio::test]
async fn a_reload_reports_the_difference_by_name() {
    let agents = RemoteAgents::connect(
        vec![down("kept", 1), down("moved", 1), down("gone", 1)],
        None,
    )
    .await;
    let before = agents.snapshot();

    let report = agents
        .reload(vec![down("kept", 1), down("moved", 2), down("new", 1)])
        .await;

    assert_eq!(report.added, ["new"]);
    assert_eq!(report.removed, ["gone"]);
    assert_eq!(report.changed, ["moved"]);
    assert_eq!(report.refreshed, ["kept"]);
    assert_eq!(report.unreachable, ["kept", "moved", "new"]);
    assert_eq!(agents.names(), ["kept", "moved", "new"]);
    assert_eq!(
        before.names(),
        ["kept", "moved", "gone"],
        "a snapshot taken before the reload is the set it was taken from"
    );
}

#[tokio::test]
async fn a_reload_keeps_every_conversation_whatever_it_did_to_the_agent() {
    let agents = RemoteAgents::connect(vec![down("reviewer", 1)], None).await;
    agents
        .record("agent-1", "reviewer", working(), &Remembered::Not)
        .expect("in memory");

    agents.reload(Vec::new()).await;
    agents.reload(vec![down("reviewer", 2)]).await;

    assert_eq!(
        agents.conversation("agent-1", "reviewer", &Remembered::Not),
        working()
    );
}

#[test]
fn a_reload_report_names_no_value_it_read() {
    let report = A2aReload {
        added: vec!["nous".to_string()],
        ..A2aReload::default()
    };

    assert!(report.changed_configuration());
    assert!(!A2aReload::default().changed_configuration());
    assert_eq!(
        format!("{report:?}"),
        "A2aReload { added: [\"nous\"], removed: [], changed: [], refreshed: [], unreachable: [] }"
    );
}

#[tokio::test]
async fn a_client_a_retry_installs_mid_reload_survives_the_reload() {
    // The reload's card fetch is held; meanwhile a retry reaches the agent
    // and installs a client; then the reload's fetch fails. The reload must
    // not put back the `None` it saw before it started.
    let port = testing::free_port();
    let stalling = testing::serve_card_stalling_first(port);
    let agents = Arc::new(RemoteAgents::with_roster(
        Roster {
            agents: vec![Arc::new(RemoteAgent::new(down("reviewer", port), None))],
            removed: BTreeSet::new(),
        },
        None,
    ));

    let reloading = tokio::spawn({
        let agents = Arc::clone(&agents);
        async move { agents.reload(vec![down("reviewer", port)]).await }
    });
    let first_arrived = stalling.first_arrived;
    tokio::task::spawn_blocking(move || first_arrived.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .expect("the reload's fetch reached the agent");

    *agents.snapshot().agents[0].attempted.lock().unwrap() = Instant::now() - 2 * A2A_RETRY_BACKOFF;
    agents
        .reach("reviewer")
        .await
        .expect("the retry reaches it");
    let _ = stalling.release.send(());
    let report = reloading.await.unwrap();

    assert_eq!(
        report.unreachable,
        ["reviewer"],
        "the reload's own fetch failed"
    );
    assert!(
        agents
            .snapshot()
            .get("reviewer")
            .unwrap()
            .client()
            .is_some(),
        "and the retry's client stands"
    );
}

#[tokio::test]
async fn a_failed_fetch_never_keeps_a_connection_made_with_another_configuration() {
    let port = testing::free_port();
    testing::serve_card(port);
    let agents = RemoteAgents::connect(vec![down("reviewer", port)], None).await;
    assert!(
        agents
            .snapshot()
            .get("reviewer")
            .unwrap()
            .client()
            .is_some()
    );

    agents
        .reload(vec![down("reviewer", testing::free_port())])
        .await;

    assert!(
        agents
            .snapshot()
            .get("reviewer")
            .unwrap()
            .client()
            .is_none(),
        "a moved agent is not reached at its old address"
    );
}

#[tokio::test]
async fn an_exchange_in_flight_keeps_its_client_across_a_reload() {
    // ADR-0031 §6: a delegation holds what `reach` handed it for its whole
    // exchange; a reload that moves the agent meanwhile changes what the
    // *next* call reaches, never this one.
    let (old, new) = (testing::free_port(), testing::free_port());
    testing::serve_card(old);
    testing::serve_card(new);
    let agents = RemoteAgents::connect(vec![down("reviewer", old)], None).await;
    let in_flight = agents.reach("reviewer").await.expect("connected");

    agents.reload(vec![down("reviewer", new)]).await;
    agents.reload(Vec::new()).await;
    agents.reload(vec![down("reviewer", new)]).await;

    assert_eq!(
        in_flight.client.card_url().port(),
        Some(old),
        "still the client it started with, alive and unchanged"
    );
    let next = agents.reach("reviewer").await.expect("connected");
    assert_eq!(next.client.card_url().port(), Some(new));
    assert!(!Arc::ptr_eq(&in_flight.client, &next.client));
    assert_ne!(in_flight.endpoint, next.endpoint);
}
