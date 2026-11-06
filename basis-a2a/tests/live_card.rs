#[path = "support/fake_host.rs"]
mod fake_host;
use basis_a2a::a2a::AgentExtension;
use basis_a2a::{A2aServer, A2aService, AgentIdentity, BearerToken, ExtendedCard, ServeConfig};
use serde_json::{Value, json};

#[tokio::test]
async fn identity_updates_reach_live_cards_without_changing_protocol_or_auth() {
    let state = tempfile::tempdir().unwrap();
    let host = fake_host::FakeHost::answering(vec![]);
    let config = ServeConfig::hosted(
        AgentIdentity::new("before", "Before update"),
        state.path(),
        host,
    )
    .with_token(BearerToken::new("fixture-token").unwrap())
    .with_extended_card(ExtendedCard::new());
    let service = A2aService::open(config).unwrap();
    let server = A2aServer::bind("127.0.0.1:0".parse().unwrap(), service.clone())
        .await
        .unwrap();
    let endpoint = server.endpoint().to_owned();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    let client = reqwest::Client::new();
    let card_url = format!("{endpoint}.well-known/agent-card.json");
    let before: Value = client
        .get(&card_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    service
        .update_identity(AgentIdentity::new("after", "After update").with_version("2"))
        .unwrap();
    let after: Value = client
        .get(&card_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["name"], "after");
    assert_eq!(after["description"], "After update");
    assert_eq!(after["version"], "2");
    for field in [
        "supportedInterfaces",
        "capabilities",
        "securitySchemes",
        "securityRequirements",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    let extended: Value = client
        .post(&endpoint)
        .bearer_auth("fixture-token")
        .header("A2A-Version", "1.0")
        .json(&json!({"jsonrpc":"2.0", "id":1,"method":"GetExtendedAgentCard","params":{}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(extended["result"]["name"], "after");
    assert_eq!(
        client
            .post(&endpoint)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let invalid = AgentIdentity::new("invalid", "Must not appear").with_extension(AgentExtension {
        uri: "https://example.test/required".into(),
        description: None,
        required: Some(true),
        params: None,
    });
    assert!(service.update_identity(invalid).is_err());
    let retained: Value = client
        .get(&card_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(retained, after);
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}
