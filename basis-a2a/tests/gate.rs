//! What a request passes before either binding sees it: authentication
//! before its body is read, a bounded body, and — on a loopback server — the
//! checks that keep a web page from calling it.

#[path = "support/harness.rs"]
mod harness;

use std::time::Duration;

use basis_a2a::{A2aService, InboundApproval, OpenError};
use harness::{ScriptedEndpoint, Served, base_config, user_message};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Sends `head` (and nothing after it) and reads the status line of
/// whatever comes back within a few seconds.
async fn status_of_raw(served: &Served, head: &str) -> String {
    let address = served.base.trim_start_matches("http://");
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut response = vec![0_u8; 256];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut response))
        .await
        .expect("answered without waiting for a body")
        .unwrap();
    String::from_utf8_lossy(&response[..read])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_caller_without_the_token_is_refused_before_its_body_is_read() {
    let served = Served::start().await;
    // A body is promised and never sent: a server that read it first would
    // wait for it instead of answering.
    let status = status_of_raw(
        &served,
        "POST / HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
         a2a-version: 1.0\r\ncontent-length: 400000\r\n\r\n",
    )
    .await;
    assert!(status.contains("401"), "{status}");
}

#[tokio::test]
async fn a_body_past_the_limit_is_refused_by_its_declared_length() {
    let served = Served::start().await;
    let status = status_of_raw(
        &served,
        &format!(
            "POST / HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
             a2a-version: 1.0\r\nauthorization: Bearer {}\r\ncontent-length: 9000000\r\n\r\n",
            harness::TOKEN
        ),
    )
    .await;
    assert!(status.contains("413"), "{status}");
}

/// What a web page can send without a CORS preflight: a `text/plain` POST,
/// with the version in the query string, to a tokenless loopback server.
#[tokio::test]
async fn a_simple_cross_site_request_is_refused() {
    let served = Served::start_anonymous(ScriptedEndpoint::start(false), |config| config).await;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
        "params": {"message": user_message("from a web page", None)},
    });
    for content_type in ["text/plain", "application/x-www-form-urlencoded"] {
        let response = served
            .http
            .post(format!("{}?A2A-Version=1.0", served.endpoint))
            .header("content-type", content_type)
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 415, "{content_type}");
    }
    let rest = served
        .http
        .post(format!("{}/json/message:send", served.base))
        .header("content-type", "text/plain")
        .header("a2a-version", "1.0")
        .body(json!({"message": user_message("again", None)}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(rest.status(), 415);
    assert!(served.model.requests().is_empty(), "nothing ran");
}

/// DNS rebinding: a page whose own name now resolves to loopback sends
/// that name as its `Host`.
#[tokio::test]
async fn a_loopback_server_refuses_a_host_it_does_not_answer_to() {
    let served = Served::start_anonymous(ScriptedEndpoint::start(false), |config| config).await;
    let rebound = status_of_raw(
        &served,
        "GET /.well-known/agent-card.json HTTP/1.1\r\nhost: evil.example:8080\r\n\r\n",
    )
    .await;
    assert!(rebound.contains("421"), "{rebound}");
    let own = status_of_raw(
        &served,
        "GET /.well-known/agent-card.json HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n",
    )
    .await;
    assert!(own.contains("200"), "{own}");
}

#[tokio::test]
async fn push_or_asking_without_a_token_needs_an_explicit_trust() {
    harness::isolate_environment();
    let model = ScriptedEndpoint::start(false);
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = || base_config(workspace.path(), data.path(), &model);
    assert!(matches!(
        A2aService::open(config().with_push_notifications()),
        Err(OpenError::NeedsToken(_))
    ));
    let asking = InboundApproval::AskCaller {
        deadline: Duration::from_secs(60),
    };
    assert!(matches!(
        A2aService::open(config().with_approval(asking)),
        Err(OpenError::NeedsToken(_))
    ));
    assert!(
        A2aService::open(
            config()
                .with_approval(asking)
                .with_push_notifications()
                .with_trusted_anonymous_callers()
        )
        .is_ok()
    );
}

#[tokio::test]
async fn the_card_needs_no_token() {
    let served = Served::start().await;
    let card = reqwest::get(&served.card_url).await.unwrap();
    assert_eq!(card.status(), 200);
}
