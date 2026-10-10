//! ADR-0031 §4: basis can watch. A poll of each source path notices a file
//! that did not exist at open, keeps the set through a save that does not
//! parse, and stops when its handle is dropped.

use std::time::Duration;

use basis::Workspace;

use crate::support::{CardServer, a2a_file, offline_runtime, pinned, write};

/// Fast enough that a test does not wait long, slow enough to be a poll.
const INTERVAL: Duration = Duration::from_millis(20);

/// Waits up to five seconds for the workspace to configure exactly `names`.
async fn settles_on(workspace: &Workspace, names: &[&str]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while workspace.a2a_agents() != names {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never settled on {names:?}; configured {:?}",
            workspace.a2a_agents()
        );
        tokio::time::sleep(INTERVAL).await;
    }
}

/// Long enough for several polls to have come and gone.
async fn several_polls() {
    tokio::time::sleep(INTERVAL * 10).await;
}

#[tokio::test]
async fn a_watch_notices_a_file_that_did_not_exist_at_open() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens with nothing configured");
    let _watch = workspace.watch_a2a(INTERVAL);

    write(
        &dir.path().join(".basis/a2a.json"),
        &a2a_file(&[("nous", &server.base)]),
    );

    settles_on(&workspace, &["nous"]).await;
}

#[tokio::test]
async fn a_change_made_before_the_watch_started_is_still_noticed() {
    // What a poll compares against is what the open read, not what the
    // first poll saw.
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens");
    write(
        &dir.path().join(".basis/a2a.json"),
        &a2a_file(&[("nous", &server.base)]),
    );

    let _watch = workspace.watch_a2a(INTERVAL);

    settles_on(&workspace, &["nous"]).await;
}

#[tokio::test]
async fn a_watch_keeps_the_set_through_a_save_that_does_not_parse() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".basis/a2a.json");
    write(&file, &a2a_file(&[("nous", &server.base)]));
    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens");
    let _watch = workspace.watch_a2a(INTERVAL);

    write(&file, r#"{"agents":{"nous":{"url":"#);
    several_polls().await;
    assert_eq!(
        workspace.a2a_agents(),
        ["nous"],
        "a typo never drops an agent"
    );

    write(
        &file,
        &a2a_file(&[("nous", &server.base), ("scout", &server.base)]),
    );
    settles_on(&workspace, &["nous", "scout"]).await;
}

#[tokio::test]
async fn dropping_the_watch_stops_it() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens");
    drop(workspace.watch_a2a(INTERVAL));

    write(
        &dir.path().join(".basis/a2a.json"),
        &a2a_file(&[("nous", &server.base)]),
    );
    several_polls().await;

    assert!(workspace.a2a_agents().is_empty());
    assert_eq!(server.fetches(), 0);
}

#[tokio::test]
async fn a_save_that_renames_the_file_away_and_writes_a_new_one_never_drops_an_agent() {
    // vim's `backupcopy=no`, Emacs: the file is gone for a moment before the
    // new one lands. A poll in that moment must not reload with no agents.
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".basis/a2a.json");
    write(&file, &a2a_file(&[("nous", &server.base)]));
    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens");
    let interval = Duration::from_millis(100);
    let _watch = workspace.watch_a2a(interval);

    for _ in 0..5 {
        std::fs::rename(&file, dir.path().join(".basis/a2a.json~")).unwrap();
        tokio::time::sleep(interval / 5).await;
        assert!(!workspace.a2a_agents().is_empty(), "dropped mid-save");
        write(&file, &a2a_file(&[("nous", &server.base)]));
        tokio::time::sleep(interval / 2).await;
        assert!(!workspace.a2a_agents().is_empty(), "dropped mid-save");
    }
    write(
        &file,
        &a2a_file(&[("nous", &server.base), ("scout", &server.base)]),
    );
    settles_on(&workspace, &["nous", "scout"]).await;
}
