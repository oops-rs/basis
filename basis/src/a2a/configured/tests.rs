//! The configured set is live (ADR-0031): a reload reports what it changed
//! by name, a retry's client survives a reload that failed to fetch, and an
//! exchange in flight keeps the client it started with.

use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};

use super::*;
use crate::a2a::A2aReload;

/// An agent at a port nothing listens on, so connecting fails fast and the
/// diff is all that is under test.
fn down(name: &str, port: u16) -> A2aAgent {
    testing::agent_at(name, port)
}

#[tokio::test]
async fn a_reload_reports_the_difference_by_name() {
    let agents =
        ConfiguredAgents::connect(vec![down("kept", 1), down("moved", 1), down("gone", 1)]).await;
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
    let agents = Arc::new(ConfiguredAgents::with_roster(Roster {
        agents: vec![Arc::new(RemoteAgent::new(down("reviewer", port), None))],
        removed: BTreeSet::new(),
    }));

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
    let agents = ConfiguredAgents::connect(vec![down("reviewer", port)]).await;
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
    let agents = ConfiguredAgents::connect(vec![down("reviewer", old)]).await;
    let in_flight = agents.reach("reviewer").await.expect("connected");

    agents.reload(vec![down("reviewer", new)]).await;
    agents.reload(Vec::new()).await;
    agents.reload(vec![down("reviewer", new)]).await;

    assert_eq!(
        in_flight.endpoint,
        down("reviewer", old).endpoint(),
        "still the peer it started with, reaching the old address"
    );
    let next = agents.reach("reviewer").await.expect("connected");
    assert_eq!(next.endpoint, down("reviewer", new).endpoint());
    assert_ne!(in_flight.endpoint, next.endpoint);
}
