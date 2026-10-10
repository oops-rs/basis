//! Parts beyond text (ADR-0030 §6): data and inline textual files reach the
//! model, anything else is refused, and the card lists exactly what is
//! accepted.

#[path = "support/harness.rs"]
mod harness;

use basis_a2a::a2a::error_code;
use harness::Served;
use serde_json::json;

#[tokio::test]
async fn data_and_a_textual_file_reach_the_model_and_the_history_keeps_them() {
    let served = Served::start().await;
    // "a,b\n1,2" in base64.
    let message = json!({
        "messageId": "m-parts", "role": "ROLE_USER",
        "parts": [
            {"text": "Compare these."},
            {"data": {"threshold": 7}},
            {"raw": "YSxiCjEsMg==", "mediaType": "text/csv", "filename": "table.csv"},
        ],
    });
    let result = served
        .result("SendMessage", json!({"message": message}))
        .await;
    assert_eq!(result["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    let sent = served.model.requests().concat();
    assert!(sent.contains("Compare these."), "{sent}");
    assert!(
        sent.contains("Structured data from the caller (JSON)"),
        "{sent}"
    );
    assert!(sent.contains("threshold"), "{sent}");
    assert!(
        sent.contains("File `table.csv` from the caller (text/csv"),
        "{sent}"
    );
    assert!(sent.contains("a,b"), "{sent}");

    let history = &result["task"]["history"][0]["parts"];
    assert_eq!(history[1]["data"]["threshold"], 7, "{history}");
    assert_eq!(history[2]["filename"], "table.csv", "{history}");
}

#[tokio::test]
async fn a_file_by_url_or_binary_bytes_is_refused() {
    let served = Served::start().await;
    for part in [
        json!({"url": "https://example.com/notes.txt", "mediaType": "text/plain"}),
        json!({"raw": "iVBORw0KGgo=", "mediaType": "image/png"}),
    ] {
        let message = json!({"messageId": "m", "role": "ROLE_USER", "parts": [part]});
        assert_eq!(
            served
                .error_code("SendMessage", json!({"message": message}))
                .await,
            error_code::CONTENT_TYPE_NOT_SUPPORTED
        );
    }
    assert!(served.model.requests().is_empty(), "nothing refused is run");
}

#[tokio::test]
async fn the_card_lists_exactly_the_accepted_input_modes() {
    let served = Served::start().await;
    let card = served.card().await;
    let modes: Vec<&str> = card["defaultInputModes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|mode| mode.as_str().unwrap())
        .collect();
    for mode in [
        "text/plain",
        "text/*",
        "application/json",
        "application/xml",
        "application/yaml",
    ] {
        assert!(modes.contains(&mode), "{mode} in {modes:?}");
    }
    assert!(!modes.iter().any(|mode| mode.starts_with("image/")));
    assert_eq!(card["defaultOutputModes"], json!(["text/plain"]));
}
