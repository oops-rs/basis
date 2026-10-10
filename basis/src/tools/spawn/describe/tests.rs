//! What a copy of `spawn` bound to a live set teaches (ADR-0031 §3): nothing
//! extra while the set is empty, every configured agent once it is not, and
//! whatever the set holds at the moment mentra asks.

use std::sync::Arc;

use mentra::tool::ToolDefinition;

use crate::{
    a2a::remote::{
        RemoteAgents,
        testing::{agent_at, free_port, serve_card},
    },
    tools::SpawnTool,
};

fn described(tool: &SpawnTool) -> String {
    tool.descriptor()
        .provider
        .description
        .expect("spawn is described")
}

#[test]
fn a_copy_bound_to_an_empty_set_is_the_plain_spawn_byte_for_byte() {
    let empty = Arc::new(RemoteAgents::unreachable(&[]));

    for plain in [
        SpawnTool::new(),
        SpawnTool::with_targets(["mac".to_string()]),
    ] {
        let copy = plain.for_remote_agents(&empty).remembering();
        assert_eq!(
            copy.descriptor(),
            plain.descriptor(),
            "description and schema alike"
        );
    }
}

#[test]
fn an_unreachable_agent_is_taught_as_one_the_model_may_still_ask() {
    let set = Arc::new(RemoteAgents::unreachable(&["reviewer"]));

    let text = described(&SpawnTool::new().for_remote_agents(&set));

    assert!(text.contains("`@<agent> <task>`"), "{text}");
    assert!(text.contains("- `reviewer`"), "{text}");
    assert!(text.contains("Not reachable when last tried"), "{text}");
}

#[tokio::test]
async fn the_description_follows_the_set_from_the_next_capture_on() {
    let port = free_port();
    serve_card(port);
    let set = Arc::new(RemoteAgents::connect(Vec::new(), None).await);
    let copy = SpawnTool::new().for_remote_agents(&set);
    assert!(!described(&copy).contains("@<agent>"));

    set.reload(vec![agent_at("reviewer", port)]).await;

    let text = described(&copy);
    assert!(text.contains("- `reviewer`"), "{text}");
    assert!(text.contains("Reviews diffs."), "its card's words: {text}");
    assert!(!text.contains("Not reachable"), "{text}");

    set.reload(Vec::new()).await;
    assert_eq!(
        copy.descriptor(),
        SpawnTool::new().descriptor(),
        "removing the last agent takes the paragraph away again"
    );
}
