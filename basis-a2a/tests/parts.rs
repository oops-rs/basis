//! Parts beyond text (ADR-0030 §6, revised by ADR-0041 §6): data and inline
//! textual files reach the model, any other inline file is saved in the
//! workspace's inbox and named, a file by URL is refused, and the card says
//! any media type is accepted.

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
async fn a_file_by_url_is_refused() {
    let served = Served::start().await;
    let part = json!({"url": "https://example.com/notes.txt", "mediaType": "text/plain"});
    let message = json!({"messageId": "m", "role": "ROLE_USER", "parts": [part]});
    assert_eq!(
        served
            .error_code("SendMessage", json!({"message": message}))
            .await,
        error_code::CONTENT_TYPE_NOT_SUPPORTED
    );
    assert!(served.model.requests().is_empty(), "nothing refused is run");
}

#[tokio::test]
async fn an_image_is_saved_in_the_inbox_and_the_model_is_told_where() {
    let served = Served::start().await;
    // The PNG signature in base64.
    let message = json!({
        "messageId": "m-image", "role": "ROLE_USER",
        "parts": [
            {"text": "Why is the button red?"},
            {"raw": "iVBORw0KGgo=", "mediaType": "image/png", "filename": "login.png"},
        ],
    });
    let result = served
        .result("SendMessage", json!({"message": message}))
        .await;
    assert_eq!(result["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    let sent = served.model.requests().concat();
    let at = sent
        .find("saved at `.basis/inbox/")
        .unwrap_or_else(|| panic!("the model is told where: {sent}"));
    let path = sent[at + "saved at `".len()..]
        .split('`')
        .next()
        .unwrap()
        .to_string();
    assert!(path.ends_with("/login.png"), "{path}");
    assert_eq!(
        std::fs::read(served.workspace.path().join(&path)).expect("saved"),
        b"\x89PNG\r\n\x1a\n"
    );
}

#[tokio::test]
async fn the_card_accepts_any_media_type() {
    let served = Served::start().await;
    let card = served.card().await;
    assert_eq!(card["defaultInputModes"], json!(["*/*"]));
}
