//! The extended card (ADR-0030 §5): declared on the public card when an
//! operator configures one, and served on both bindings to authenticated
//! callers only.

#[path = "support/harness.rs"]
mod harness;

use basis_a2a::{
    A2aService, ExtendedCard, OpenError,
    a2a::{AgentSkill, error_code},
};
use harness::{ScriptedEndpoint, Served, base_config};
use serde_json::{Value, json};

fn skill() -> AgentSkill {
    AgentSkill {
        id: "audit".to_string(),
        name: "Audit".to_string(),
        description: "Audits the repository's dependencies.".to_string(),
        tags: vec!["security".to_string()],
        examples: None,
        input_modes: None,
        output_modes: None,
        security_requirements: None,
    }
}

fn extended() -> ExtendedCard {
    ExtendedCard::new()
        .with_description("Answers questions, and audits for callers it knows.")
        .with_skill(skill())
}

#[tokio::test]
async fn an_extended_card_is_declared_and_served_on_both_bindings() {
    let served = Served::start_with(ScriptedEndpoint::start(false), |config| {
        config.with_extended_card(extended())
    })
    .await;
    let public = served.card().await;
    assert_eq!(
        public["capabilities"]["extendedAgentCard"], true,
        "{public}"
    );
    assert_eq!(public["skills"].as_array().unwrap().len(), 1);

    let over_rpc = served.result("GetExtendedAgentCard", json!({})).await;
    assert_eq!(
        over_rpc["description"],
        "Answers questions, and audits for callers it knows."
    );
    let skills: Vec<&Value> = over_rpc["skills"].as_array().unwrap().iter().collect();
    assert_eq!(skills.len(), 2, "{over_rpc}");
    assert_eq!(skills[1]["id"], "audit");
    assert_eq!(
        over_rpc["supportedInterfaces"], public["supportedInterfaces"],
        "the extended card describes the same agent"
    );

    let rest = served
        .caller()
        .request(
            reqwest::Method::GET,
            &format!("{}/json/extendedAgentCard", served.base),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(rest.status(), 200);
    let over_rest: Value = rest.json().await.unwrap();
    assert_eq!(over_rest, over_rpc);

    let anonymous = served
        .http
        .get(format!("{}/json/extendedAgentCard", served.base))
        .header("A2A-Version", "1.0")
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401, "only an authenticated caller");
}

#[tokio::test]
async fn without_one_the_card_declares_none_and_the_method_is_unsupported() {
    let served = Served::start().await;
    assert_eq!(
        served.card().await["capabilities"]["extendedAgentCard"],
        false
    );
    assert_eq!(
        served.error_code("GetExtendedAgentCard", json!({})).await,
        error_code::UNSUPPORTED_OPERATION
    );
}

#[tokio::test]
async fn an_extended_card_on_a_server_nobody_authenticates_to_is_refused() {
    harness::isolate_environment();
    let model = ScriptedEndpoint::start(false);
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = base_config(workspace.path(), data.path(), &model).with_extended_card(extended());
    assert!(matches!(
        A2aService::open(config),
        Err(OpenError::ExtendedCardWithoutToken)
    ));
}
