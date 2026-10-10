//! What a request passes before either binding sees it: authentication
//! before its body is read, a bounded body, and — on a loopback server — the
//! checks that keep a web page from calling it.

#[path = "support/harness.rs"]
mod harness;

use std::time::Duration;

use harness::Served;
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

#[tokio::test]
async fn the_card_needs_no_token() {
    let served = Served::start().await;
    let card = reqwest::get(&served.card_url).await.unwrap();
    assert_eq!(card.status(), 200);
}
