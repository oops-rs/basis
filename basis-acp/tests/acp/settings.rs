//! The model and the reasoning effort, as a client changes them.
//!
//! Apart from `sessions` because these pin an *effect* rather than a
//! vocabulary: the assertions that matter read the request the scripted
//! provider actually received on the turn after the change, which is the only
//! evidence that `session/set_config_option` did anything at all. An option
//! that answers `Ok` and changes nothing is exactly the failure a picker
//! cannot show.

use std::{path::PathBuf, sync::Arc};

use agent_client_protocol::schema::v1::{
    NewSessionRequest, SessionConfigOption, SetSessionConfigOptionRequest,
};
use mentra::{RuntimePolicy, provider::ReasoningEffort, test::MockRuntime};

use crate::client::{connected, current_value, open, say};
use crate::source::{MOCK_RUNTIME, MockSource, text_mock, workspace};

/// `(option id, current value)` for each advertised option, in order.
fn settings(options: &[SessionConfigOption]) -> Vec<(String, String)> {
    options
        .iter()
        .map(|option| (option.id.0.to_string(), current_value(option)))
        .collect()
}

/// Two scripted turns, so a test can change something between them and read
/// the second request back.
fn two_turns() -> MockRuntime {
    MockRuntime::builder()
        .model("mock-model", "openai")
        .runtime_identifier(MOCK_RUNTIME)
        .with_policy(RuntimePolicy::permissive())
        .text("first")
        .text("second")
        .build()
        .expect("mock runtime builds")
}

#[tokio::test]
async fn a_new_session_advertises_its_model_and_its_effort() {
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (options, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let response = connection
                .send_request(NewSessionRequest::new(PathBuf::from("/")))
                .block_task()
                .await?;

            Ok(response.config_options)
        },
    )
    .await;

    let options = options.expect("a session reports the settings it has");
    assert_eq!(
        settings(&options),
        vec![
            ("model".to_string(), "mock-model".to_string()),
            ("effort".to_string(), "default".to_string()),
        ],
        "the model is the one the session is on; nothing has asked for an effort yet"
    );
}

#[tokio::test]
async fn setting_the_effort_is_echoed_and_reaches_the_next_turn() {
    let workspace = workspace();
    let mock = Arc::new(two_turns());

    let (answered, observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            // A turn before the change, so the assertion below is about the
            // change rather than about the default.
            say(&connection, &session, "one").await?;

            let response = connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session.clone(),
                    "effort",
                    "high",
                ))
                .block_task()
                .await?;

            say(&connection, &session, "two").await?;

            Ok(response.config_options)
        },
    )
    .await;

    assert_eq!(
        settings(&answered),
        vec![
            ("model".to_string(), "mock-model".to_string()),
            ("effort".to_string(), "high".to_string()),
        ],
        "the response carries the full set with the new value in it"
    );
    assert_eq!(
        observed.lock().expect("not poisoned").config_updates(),
        vec![vec![
            ("model".to_string(), "mock-model".to_string()),
            ("effort".to_string(), "high".to_string()),
        ]],
        "and a second view of the session hears about it too"
    );

    // The part only the provider can settle.
    let requests = mock.recorded_requests().await;
    assert_eq!(
        requests[0].provider_request_options.reasoning, None,
        "the first turn ran before anyone asked for an effort"
    );
    assert_eq!(
        requests[1]
            .provider_request_options
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort),
        Some(ReasoningEffort::High),
        "the turn after the change must actually carry it"
    );
}

#[tokio::test]
async fn setting_a_model_lan_never_listed_still_reaches_the_next_turn() {
    // The list is one entry — advertising a provider's catalogue costs a
    // network round trip mentra does not cache — so every model but the
    // current one is "never listed". Refusing them would make the option
    // useless for exactly the endpoints that need it.
    let workspace = workspace();
    let mock = Arc::new(two_turns());

    let (answered, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;
            say(&connection, &session, "one").await?;

            let response = connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session.clone(),
                    "model",
                    "some-other-model",
                ))
                .block_task()
                .await?;

            say(&connection, &session, "two").await?;

            Ok(response.config_options)
        },
    )
    .await;

    assert_eq!(
        settings(&answered)[0],
        ("model".to_string(), "some-other-model".to_string())
    );

    let requests = mock.recorded_requests().await;
    assert_eq!(&*requests[0].model, "mock-model");
    assert_eq!(
        &*requests[1].model, "some-other-model",
        "the model is what the next request is actually built with"
    );
}

#[tokio::test]
async fn an_option_lan_never_advertised_is_refused() {
    // The alternative is an `Ok` for a setting that did not change, which a
    // client renders as a control that silently does nothing.
    let workspace = workspace();
    let mock = Arc::new(text_mock(&["unused"]));

    let (result, _observed) = connected(
        MockSource::new(&mock, &workspace),
        None,
        |connection| async move {
            let session = open(&connection).await?;

            Ok(connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session,
                    "temperature",
                    "0.7",
                ))
                .block_task()
                .await)
        },
    )
    .await;

    let error = result.expect_err("basis advertises no such option");
    assert!(
        error
            .data
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .is_some_and(|data| data.contains("temperature")),
        "the message must name what was refused: {:?}",
        error.data
    );
}
