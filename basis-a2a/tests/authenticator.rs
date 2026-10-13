//! An operator's own authenticator (ADR-0036): it decides who every request
//! is from, before the body is read, and the card declares what it asks for.

#[path = "support/harness.rs"]
mod harness;

use std::{sync::Arc, time::Duration};

use basis_a2a::{
    A2aService, Authenticator, BearerToken, OpenError, Principal, Refusal, RequestHead, Security,
    a2a::{HttpAuthSecurityScheme, SecurityScheme, error_code},
};
use futures::future::BoxFuture;
use harness::{ScriptedEndpoint, Served, base_config, user_message};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// A stand-in for a gateway's signed token: `Bearer as:<name>` is the
/// principal `<name>`, `Bearer banned` is a caller proven and not allowed,
/// and anything else proves nothing.
struct Gateway {
    security: Option<Security>,
}

impl Gateway {
    fn jwt() -> Arc<Self> {
        let scheme = SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "Bearer".to_string(),
            description: Some("A token the gateway signed for this agent.".to_string()),
            bearer_format: Some("JWT".to_string()),
        });
        Arc::new(Self {
            security: Some(Security::required("gateway", scheme)),
        })
    }
}

impl Authenticator for Gateway {
    fn authenticate<'a>(
        &'a self,
        head: &'a RequestHead,
    ) -> BoxFuture<'a, Result<Principal, Refusal>> {
        Box::pin(async move {
            // Awaits, as a key-set refresh would.
            tokio::task::yield_now().await;
            let token = head
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or(Refusal::Unauthenticated)?;
            if token == "banned" {
                return Err(Refusal::Forbidden);
            }
            token
                .strip_prefix("as:")
                .and_then(Principal::new)
                .ok_or(Refusal::Unauthenticated)
        })
    }

    fn security(&self) -> Option<Security> {
        self.security.clone()
    }

    fn challenge(&self) -> &str {
        "Bearer realm=\"gateway\""
    }
}

async fn behind_the_gateway() -> Served {
    Served::start_anonymous(ScriptedEndpoint::start(false), |config| {
        config.with_authenticator(Gateway::jwt())
    })
    .await
}

#[tokio::test]
async fn the_authenticator_names_each_caller_and_keeps_their_tasks_apart() {
    let served = behind_the_gateway().await;
    let device = served.as_token("as:anon:dev_01J9");
    let user = served.as_token("as:usr_01J9");

    let created = device
        .result(
            "SendMessage",
            json!({"message": user_message("mine", None)}),
        )
        .await;
    let id = created["task"]["id"].as_str().unwrap();
    assert_eq!(device.result("GetTask", json!({"id": id})).await["id"], id);
    assert_eq!(
        user.error_code("GetTask", json!({"id": id})).await,
        error_code::TASK_NOT_FOUND,
        "an anonymous device is a principal of its own"
    );

    let unproven = served.as_token("not-a-token").post(json!({})).await;
    assert_eq!(unproven.status(), 401);
    assert_eq!(
        unproven.headers()["www-authenticate"],
        "Bearer realm=\"gateway\""
    );
    let forbidden = served.as_token("banned").post(json!({})).await;
    assert_eq!(forbidden.status(), 403);
    assert!(forbidden.text().await.unwrap().is_empty(), "says nothing");
}

/// Sends `head` and nothing after it, and reads the status line of whatever
/// comes back within a few seconds.
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
async fn a_refusal_comes_before_the_body_is_read() {
    let served = behind_the_gateway().await;
    for (authorization, status) in [("", "401"), ("authorization: Bearer banned\r\n", "403")] {
        // A body is promised and never sent: a server that read it first
        // would wait for it instead of answering.
        let answered = status_of_raw(
            &served,
            &format!(
                "POST / HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
                 a2a-version: 1.0\r\n{authorization}content-length: 400000\r\n\r\n"
            ),
        )
        .await;
        assert!(answered.contains(status), "{answered}");
    }
}

#[tokio::test]
async fn the_card_declares_the_authenticators_scheme_in_the_v1_shape() {
    let served = behind_the_gateway().await;
    let card = served.card().await;
    assert_eq!(
        card["securitySchemes"],
        json!({"gateway": {"httpAuthSecurityScheme": {
            "scheme": "Bearer",
            "description": "A token the gateway signed for this agent.",
            "bearerFormat": "JWT",
        }}}),
        "{card}"
    );
    assert_eq!(
        card["securityRequirements"],
        json!([{"schemes": {"gateway": {"list": []}}}])
    );
}

#[tokio::test]
async fn an_authenticator_replaces_tokens_and_one_that_declares_nothing_is_tokenless() {
    harness::isolate_environment();
    let model = ScriptedEndpoint::start(false);
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = || base_config(workspace.path(), data.path(), &model);
    assert!(matches!(
        A2aService::open(
            config()
                .with_token(BearerToken::new("t").unwrap())
                .with_authenticator(Gateway::jwt())
        ),
        Err(OpenError::TokensWithAuthenticator)
    ));
    let declares_nothing = Arc::new(Gateway { security: None });
    assert!(matches!(
        A2aService::open(
            config()
                .with_authenticator(declares_nothing)
                .with_push_notifications()
        ),
        Err(OpenError::NeedsToken(_))
    ));
}
