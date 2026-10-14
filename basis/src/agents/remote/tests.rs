//! A remembered agent's conversations outlive the process that recorded them
//! (ADR-0029 §5); a delegated child's, and an ephemeral runtime's, do not.
//! And the set they are kept beside is live (ADR-0031): a reload reports what
//! it changed by name and keeps every conversation whatever it did.

use super::*;
#[cfg(feature = "a2a")]
use crate::a2a::configured::{ConfiguredAgents, testing::agent_at};
#[cfg(feature = "a2a")]
use std::sync::Arc;

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
    let before = RemoteAgents::alone(Some(dir.path()));
    before
        .record("agent-1", "reviewer", working(), &Remembered::ByAgent)
        .expect("written through");
    drop(before);

    // A new process: nothing in memory, the same directory on disk.
    let after = RemoteAgents::alone(Some(dir.path()));

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
    let agents = RemoteAgents::alone(Some(dir.path()));

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
    let agents = RemoteAgents::alone(None);

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
    let agents = RemoteAgents::alone(Some(dir.path()));
    agents
        .record("agent-1", "reviewer", working(), &Remembered::ByAgent)
        .expect("written through");

    forget(dir.path(), "agent-1").expect("removed");
    forget(dir.path(), "agent-1").expect("and absent is not an error");

    let after = RemoteAgents::alone(Some(dir.path()));
    assert_eq!(
        after.conversation("agent-1", "reviewer", &Remembered::ByAgent),
        Conversation::default()
    );
}

#[test]
fn an_id_that_is_not_a_file_name_is_never_made_into_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let agents = RemoteAgents::alone(Some(dir.path()));

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
    let first = RemoteAgents::alone(Some(dir.path()));
    first
        .record("agent-1", "nous", working(), &by_key("chat-7"))
        .expect("written through");

    // Another open on the same store, another minted agent: the same record,
    // read afresh rather than from anything cached.
    let second = RemoteAgents::alone(Some(dir.path()));
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
    let agents = RemoteAgents::alone(Some(dir.path()));
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
    let agents = RemoteAgents::alone(None);
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
    let agents = RemoteAgents::alone(Some(&store));
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

#[cfg(feature = "a2a")]
#[tokio::test]
async fn a_reload_keeps_every_conversation_whatever_it_did_to_the_agent() {
    let configured = Arc::new(ConfiguredAgents::connect(vec![agent_at("reviewer", 1)]).await);
    let agents = RemoteAgents::over_configured(&configured);
    agents
        .record("agent-1", "reviewer", working(), &Remembered::Not)
        .expect("in memory");

    configured.reload(Vec::new()).await;
    configured.reload(vec![agent_at("reviewer", 2)]).await;

    assert_eq!(
        agents.conversation("agent-1", "reviewer", &Remembered::Not),
        working()
    );
}

/// A host's resolver: knows one name, connects it to a card server, and
/// records the change hook it was given.
#[cfg(feature = "a2a")]
struct Registry {
    name: &'static str,
    port: u16,
    hook: std::sync::Mutex<Option<crate::agents::OnChange>>,
}

#[cfg(feature = "a2a")]
#[async_trait::async_trait]
impl crate::agents::AgentResolver for Registry {
    async fn resolve(
        &self,
        name: &str,
    ) -> Result<crate::agents::Resolved, crate::agents::Unresolved> {
        if name != self.name {
            return Err(crate::agents::Unresolved::Unknown);
        }
        let client = mentra::a2a::A2aClient::connect(
            crate::a2a::configured::testing::agent_at(name, self.port).config(),
        )
        .await
        .map(std::sync::Arc::new)
        .map_err(|error| crate::agents::Unresolved::Unreachable {
            error: error.to_string(),
            retry_in: None,
        })?;
        Ok(crate::agents::Resolved::over_a2a(client, format!("team/{name}")).with_id("agt_1"))
    }

    fn route(&self, name: &str) -> crate::agents::Route {
        if name == self.name {
            crate::agents::Route::Known
        } else {
            crate::agents::Route::Unknown
        }
    }

    fn taught(&self) -> crate::agents::Taught {
        crate::agents::Taught::default()
            .with_agent(crate::agents::TaughtAgent::new(self.name))
            .with_note("`@<team>/<agent>` reaches any agent in the registry.")
    }

    fn on_change(&self, hook: crate::agents::OnChange) {
        *self.hook.lock().unwrap() = Some(hook);
    }
}

#[cfg(feature = "a2a")]
#[tokio::test]
async fn the_hosts_resolver_is_asked_first_and_an_unknown_name_falls_through() {
    use crate::a2a::configured::testing::{free_port, serve_card};
    use crate::agents::{Route, Unresolved};

    let port = free_port();
    serve_card(port);
    let registry = Arc::new(Registry {
        name: "reviewer",
        port,
        hook: std::sync::Mutex::new(None),
    });
    let configured = Arc::new(ConfiguredAgents::connect(vec![agent_at("nous", port)]).await);
    let agents = RemoteAgents::over(
        vec![
            Arc::clone(&registry) as Arc<dyn crate::agents::AgentResolver>,
            Arc::clone(&configured) as Arc<dyn crate::agents::AgentResolver>,
        ],
        None,
    );

    let resolved = agents
        .resolve("reviewer")
        .await
        .expect("the registry knows it");
    assert_eq!(resolved.id, "agt_1", "the id the resolver gave");
    assert_eq!(
        resolved.name, "team/reviewer",
        "the canonical name it expanded"
    );
    assert_eq!(agents.route("reviewer"), Route::Known);

    let through = agents
        .resolve("nous")
        .await
        .expect("the file configures it");
    assert_eq!(through.id, "nous", "a configured agent's id is its name");
    assert_eq!(agents.route("nous"), Route::Known);

    assert!(matches!(
        agents.resolve("nobody").await,
        Err(Unresolved::Unknown)
    ));
    assert_eq!(agents.route("nobody"), Route::Unknown);

    assert_eq!(
        agents.names(),
        ["reviewer", "nous"],
        "taught names are the host's first"
    );
    assert_eq!(
        agents.taught().note.as_deref(),
        Some("`@<team>/<agent>` reaches any agent in the registry.")
    );
}
