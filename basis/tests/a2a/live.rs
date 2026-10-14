//! ADR-0031: a workspace's remote agents are live. A reload reads the same
//! sources the open did and applies the difference; a source that fails to
//! read changes nothing; what a run reports follows the set.

use basis::{A2aError, Event};

use crate::support::{CardServer, a2a_file, offline_runtime, pinned, write};

fn reported(header: Event) -> (Vec<String>, usize) {
    let Event::RunStarted {
        remote_agents,
        bindings,
        ..
    } = header
    else {
        panic!("the header is run_started");
    };
    let files = bindings
        .iter()
        .find(|binding| binding.kind == "a2a")
        .map_or(0, |binding| binding.files.len());
    (remote_agents, files)
}

#[tokio::test]
async fn a_file_written_after_open_is_connected_on_reload() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens with nothing configured");
    let run = workspace.prepare("go").expect("mints");
    assert_eq!(reported(run.header()), (Vec::new(), 0));

    write(
        &dir.path().join(".basis/a2a.json"),
        &a2a_file(&[("nous", &server.base)]),
    );
    let report = workspace.reload_a2a().await.expect("reloads");

    assert_eq!(report.added, ["nous"]);
    assert!(report.unreachable.is_empty(), "{report:?}");
    assert_eq!(workspace.remote_agents(), ["nous"]);
    assert_eq!(
        reported(run.header()),
        (vec!["nous".to_string()], 1),
        "a run minted before the reload announces the set as it is now"
    );
}

#[tokio::test]
async fn a_half_typed_file_never_drops_an_agent() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".basis/a2a.json");
    write(&file, &a2a_file(&[("nous", &server.base)]));
    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens");
    let run = workspace.prepare("go").expect("mints");

    // Saved mid-edit: the closing braces are not typed yet.
    write(
        &file,
        &format!(r#"{{"agents":{{"nous":{{"url":"{}""#, server.base),
    );
    let error = workspace
        .reload_a2a()
        .await
        .expect_err("a file that does not parse is an error");

    assert!(matches!(error, A2aError::Parse { .. }), "{error}");
    assert!(error.to_string().contains("a2a.json"), "{error}");
    assert_eq!(workspace.remote_agents(), ["nous"], "the set stands");
    assert_eq!(reported(run.header()), (vec!["nous".to_string()], 1));
    assert_eq!(server.fetches(), 1, "nothing was reconnected");

    // Finished typing: the next reload sees the same agent, unchanged.
    write(&file, &a2a_file(&[("nous", &server.base)]));
    let report = workspace.reload_a2a().await.expect("reloads");
    assert_eq!(report.refreshed, ["nous"]);
}

#[tokio::test]
async fn a_reload_removes_changes_and_refetches_by_name() {
    let server = CardServer::start();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".basis/a2a.json");
    write(
        &file,
        &a2a_file(&[
            ("kept", &server.base),
            ("moved", "http://127.0.0.1:1"),
            ("gone", &server.base),
        ]),
    );
    let workspace = pinned(dir.path(), offline_runtime())
        .open()
        .await
        .expect("opens");
    let fetched_at_open = server.fetches();

    write(
        &file,
        &a2a_file(&[("kept", &server.base), ("moved", &server.base)]),
    );
    let report = workspace.reload_a2a().await.expect("reloads");

    assert_eq!(report.removed, ["gone"]);
    assert_eq!(report.changed, ["moved"]);
    assert_eq!(report.refreshed, ["kept"]);
    assert!(report.added.is_empty() && report.unreachable.is_empty());
    assert_eq!(workspace.remote_agents(), ["kept", "moved"]);
    assert_eq!(
        server.fetches(),
        fetched_at_open + 2,
        "the unchanged agent's card is fetched again, the moved one's for the first time"
    );
}
