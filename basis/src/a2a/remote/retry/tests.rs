//! An unreachable agent is retried when it is asked for, at most once per
//! backoff interval (ADR-0031 §2), and reaching it changes the set.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::a2a::remote::A2A_RETRY_BACKOFF;
use crate::a2a::remote::testing::{agent_at, free_port, serve_card};

/// A set whose change hook counts how often it was told the set changed.
async fn counted(port: u16) -> (RemoteAgents, Arc<AtomicUsize>) {
    let changes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&changes);
    let agents = RemoteAgents::connect(vec![agent_at("reviewer", port)], None)
        .await
        .with_on_change(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
    (agents, changes)
}

/// Pretends the last attempt was long ago, so a retry is due.
fn backdate(agents: &RemoteAgents, name: &str) {
    let roster = agents.snapshot();
    let agent = roster.get(name).expect("configured");
    *agent.attempted.lock().unwrap() = Instant::now() - 2 * agents.backoff;
}

#[tokio::test]
async fn an_agent_that_came_up_is_reached_once_its_backoff_has_passed() {
    let port = free_port();
    let (agents, changes) = counted(port).await;
    assert!(
        agents
            .snapshot()
            .get("reviewer")
            .unwrap()
            .client()
            .is_none()
    );

    // Tried at open, moments ago: not worth trying again yet.
    assert!(matches!(agents.route("reviewer"), Route::Backoff(_)));
    assert!(matches!(
        agents.reach("reviewer").await,
        Err(Unreached::Backoff(_))
    ));

    serve_card(port);
    backdate(&agents, "reviewer");
    assert_eq!(agents.route("reviewer"), Route::Retry);
    agents.reach("reviewer").await.expect("it came up");

    assert_eq!(agents.route("reviewer"), Route::Connected);
    assert_eq!(changes.load(Ordering::SeqCst), 1, "reaching it is a change");
}

#[tokio::test]
async fn a_failed_retry_waits_out_a_whole_backoff_again() {
    let (agents, changes) = counted(free_port()).await;
    backdate(&agents, "reviewer");

    let Err(Unreached::Failed { retry_in, .. }) = agents.reach("reviewer").await else {
        panic!("nothing listens, so the retry fails");
    };
    assert_eq!(retry_in, A2A_RETRY_BACKOFF);
    assert!(
        matches!(agents.route("reviewer"), Route::Backoff(_)),
        "the failed attempt starts the clock again"
    );
    assert_eq!(changes.load(Ordering::SeqCst), 0, "nothing changed");
}

#[tokio::test]
async fn two_calls_for_one_down_agent_cost_one_attempt() {
    let (agents, _) = counted(free_port()).await;
    backdate(&agents, "reviewer");

    let (first, second) = tokio::join!(agents.reach("reviewer"), agents.reach("reviewer"));

    let attempts = [&first, &second]
        .iter()
        .filter(|result| matches!(result, Err(Unreached::Failed { .. })))
        .count();
    assert_eq!(attempts, 1, "{first:?} {second:?}");
}

#[tokio::test]
async fn a_name_nobody_configured_is_unknown() {
    let (agents, _) = counted(free_port()).await;

    assert_eq!(agents.route("nobody"), Route::Unknown);
    assert!(matches!(
        agents.reach("nobody").await,
        Err(Unreached::Unknown)
    ));
}
