//! ADR-0037: a host answers `@<name>` with a resolver of its own, asked
//! before the set the workspace's files configure, and given the hook that
//! keeps what `spawn` teaches current.

use std::sync::{Arc, Mutex};

use basis::{
    AgentResolver, Resolved, Route, Taught, TaughtAgent, Unresolved, a2a::OnChange, async_trait,
};

use crate::support::{CardServer, a2a_file, offline_runtime, pinned, write};

/// A registry that knows one agent and remembers the hook it was given.
struct Registry {
    name: &'static str,
    base: String,
    hook: Mutex<Option<OnChange>>,
}

#[async_trait]
impl AgentResolver for Registry {
    async fn resolve(&self, name: &str) -> Result<Resolved, Unresolved> {
        if name != self.name {
            return Err(Unresolved::Unknown);
        }
        let config = basis::A2aAgentConfig::new(name, &self.base);
        let client = mentra::a2a::A2aClient::connect(&config)
            .await
            .map(Arc::new)
            .map_err(|error| Unresolved::Unreachable {
                error: error.to_string(),
                retry_in: None,
            })?;
        Ok(Resolved::new(client, name))
    }

    fn route(&self, name: &str) -> Route {
        if name == self.name {
            Route::Known
        } else {
            Route::Unknown
        }
    }

    fn taught(&self) -> Taught {
        Taught::default().with_agent(TaughtAgent::new(self.name).with_summary("From the registry."))
    }

    fn on_change(&self, hook: OnChange) {
        *self.hook.lock().unwrap() = Some(hook);
    }
}

#[tokio::test]
async fn a_hosts_resolver_is_reported_first_and_the_file_still_counts() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".basis/a2a.json"),
        &a2a_file(&[("nous", &server.base)]),
    );
    let registry = Arc::new(Registry {
        name: "reviewer",
        base: server.base.clone(),
        hook: Mutex::new(None),
    });

    let workspace = pinned(dir.path(), offline_runtime())
        .with_agent_resolver(Arc::clone(&registry) as Arc<dyn AgentResolver>)
        .open()
        .await
        .expect("opens");

    assert_eq!(
        workspace.a2a_agents(),
        ["reviewer", "nous"],
        "what the host resolves, then what the file configures"
    );
    assert!(
        registry.hook.lock().unwrap().is_some(),
        "the resolver was given the hook that recaptures descriptions"
    );
    assert_eq!(
        server.fetches(),
        1,
        "the registry connects on resolve, not at open; the file's agent connected at open"
    );
}

#[tokio::test]
async fn a_hosts_resolver_outranks_the_file_by_name() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".basis/a2a.json"),
        &a2a_file(&[("reviewer", &server.base)]),
    );
    let registry = Arc::new(Registry {
        name: "reviewer",
        base: server.base.clone(),
        hook: Mutex::new(None),
    });

    let workspace = pinned(dir.path(), offline_runtime())
        .with_agent_resolver(registry)
        .open()
        .await
        .expect("opens");

    assert_eq!(
        workspace.a2a_agents(),
        ["reviewer"],
        "one name is one agent, and the host's answer is the one that resolves"
    );
}
