//! What a reload records about the files it read, and when.

use std::{path::PathBuf, sync::Arc, time::Duration};

use super::*;
use crate::a2a::configured::testing::{free_port, serve_card_stalling_first};

fn offline_runtime() -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds without touching the network"),
    )
}

/// A workspace's live A2A state with nothing configured, reading only
/// `.basis/a2a.json` under `root`.
async fn live_in(root: &Path) -> LiveA2a {
    let config = A2aConfig {
        workspace_file: PathBuf::from(crate::a2a::DEFAULT_WORKSPACE_A2A_FILE),
        global_dir: None,
        supplied: Vec::new(),
    };
    let discovery = discovered(root, &config)
        .map_err(|(_, error)| error)
        .unwrap();
    LiveA2a::connect(
        root.to_path_buf(),
        config,
        discovery,
        &offline_runtime(),
        None,
    )
    .await
}

fn configure(root: &Path, port: u16) {
    let file = root.join(".basis/a2a.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        file,
        format!(r#"{{"agents":{{"nous":{{"url":"http://127.0.0.1:{port}"}}}}}}"#),
    )
    .unwrap();
}

#[tokio::test]
async fn a_reload_cancelled_before_it_applied_leaves_the_edit_unseen() {
    let dir = tempfile::tempdir().unwrap();
    let live = live_in(dir.path()).await;
    let port = free_port();
    let stalling = serve_card_stalling_first(port);
    configure(dir.path(), port);

    // A host's timeout, or a watch dropped, while the card fetch hangs.
    let cancelled = tokio::time::timeout(Duration::from_millis(200), live.reload()).await;
    assert!(
        cancelled.is_err(),
        "the fetch was held, so the reload was cut off"
    );
    stalling
        .first_arrived
        .try_recv()
        .expect("the reload's card fetch reached the agent");
    let _ = stalling.release.send(());

    assert!(live.names().is_empty(), "nothing was applied");
    assert_ne!(
        live.seen(),
        watch::stamps(&live.paths),
        "so the next poll still sees the edit as new"
    );
}

#[tokio::test]
async fn a_read_that_fails_is_recorded_as_seen_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let live = live_in(dir.path()).await;
    let file = dir.path().join(".basis/a2a.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, r#"{"agents":"#).unwrap();

    live.reload().await.expect_err("a half-typed file");

    assert_eq!(
        live.seen(),
        watch::stamps(&live.paths),
        "a broken file is not read again until it changes"
    );
}

#[tokio::test]
async fn a_reload_that_applied_records_what_it_read() {
    let dir = tempfile::tempdir().unwrap();
    let live = live_in(dir.path()).await;
    configure(dir.path(), free_port());

    live.reload().await.expect("reloads");

    assert_eq!(live.names(), ["nous"]);
    assert_eq!(live.seen(), watch::stamps(&live.paths));
}
