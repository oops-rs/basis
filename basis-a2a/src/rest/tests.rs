use a2a::error_code;

use super::*;

fn parsed(method: Method, path: &str, query: &[(&str, &str)], body: &str) -> Operation {
    let query = query
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    match operation(&method, path, query, body.as_bytes()) {
        Ok(operation) => operation,
        Err(Refusal::Method) => panic!("{method} {path} refused as the wrong verb"),
        Err(Refusal::Error(error)) => panic!("{method} {path}: {error}"),
    }
}

fn refused(method: Method, path: &str) -> Refusal {
    match operation(&method, path, Vec::new(), b"{}") {
        Ok(operation) => panic!("{method} {path} parsed as {operation:?}"),
        Err(refusal) => refusal,
    }
}

#[test]
fn every_proto_path_names_its_operation() {
    let message =
        r#"{"message": {"messageId": "m", "role": "ROLE_USER", "parts": [{"text": "hi"}]}}"#;
    assert!(matches!(
        parsed(Method::POST, "message:send", &[], message),
        Operation::SendMessage(_)
    ));
    assert!(matches!(
        parsed(Method::POST, "message:stream", &[], message),
        Operation::SendStreamingMessage(_)
    ));
    assert!(matches!(
        parsed(Method::GET, "tasks/t1", &[("historyLength", "2")], ""),
        Operation::GetTask(GetTaskRequest { id, history_length: Some(2), .. }) if id == "t1"
    ));
    assert!(matches!(
        parsed(Method::POST, "tasks/t1:cancel", &[], ""),
        Operation::CancelTask(CancelTaskRequest { id, .. }) if id == "t1"
    ));
    for verb in [Method::GET, Method::POST] {
        assert!(matches!(
            parsed(verb, "tasks/t1:subscribe", &[], ""),
            Operation::SubscribeToTask(SubscribeToTaskRequest { id, .. }) if id == "t1"
        ));
    }
    assert!(matches!(
        parsed(
            Method::POST,
            "tasks/t1/pushNotificationConfigs",
            &[],
            r#"{"url": "https://example.com/hook"}"#
        ),
        Operation::CreatePushConfig(config) if config.task_id == "t1"
    ));
    assert!(matches!(
        parsed(
            Method::GET,
            "tasks/t1/pushNotificationConfigs",
            &[("pageSize", "5")],
            ""
        ),
        Operation::ListPushConfigs(ListTaskPushNotificationConfigsRequest {
            page_size: Some(5),
            ..
        })
    ));
    assert!(matches!(
        parsed(Method::GET, "tasks/t1/pushNotificationConfigs/c1", &[], ""),
        Operation::GetPushConfig(GetTaskPushNotificationConfigRequest { id, .. }) if id == "c1"
    ));
    assert!(matches!(
        parsed(
            Method::DELETE,
            "tasks/t1/pushNotificationConfigs/c1",
            &[],
            ""
        ),
        Operation::DeletePushConfig(_)
    ));
    assert!(matches!(
        parsed(Method::GET, "extendedAgentCard", &[], ""),
        Operation::GetExtendedAgentCard
    ));
}

#[test]
fn list_query_parameters_become_the_request() {
    let Operation::ListTasks(request) = parsed(
        Method::GET,
        "tasks",
        &[
            ("contextId", "c1"),
            ("status", "working"),
            ("pageSize", "10"),
            ("includeArtifacts", "true"),
        ],
        "",
    ) else {
        panic!("a list");
    };
    assert_eq!(request.context_id.as_deref(), Some("c1"));
    assert_eq!(request.status, Some(a2a::TaskState::Working));
    assert_eq!(request.page_size, Some(10));
    assert_eq!(request.include_artifacts, Some(true));

    let Operation::ListTasks(request) = parsed(
        Method::GET,
        "tasks",
        &[("status", "TASK_STATE_COMPLETED")],
        "",
    ) else {
        panic!("a list");
    };
    assert_eq!(request.status, Some(a2a::TaskState::Completed));
}

#[test]
fn a_known_path_under_the_wrong_verb_is_not_allowed() {
    assert!(matches!(
        refused(Method::GET, "message:send"),
        Refusal::Method
    ));
    assert!(matches!(
        refused(Method::DELETE, "tasks/t1"),
        Refusal::Method
    ));
}

#[test]
fn an_unknown_path_is_not_found() {
    for path in ["", "tasks/t1:pause", "tasks/t1/other", "nope"] {
        let Refusal::Error(error) = refused(Method::GET, path) else {
            panic!("{path} is not a verb problem");
        };
        assert_eq!(error.code, error_code::METHOD_NOT_FOUND, "{path}");
    }
}

#[test]
fn a_tenant_prefix_or_field_is_refused_by_name() {
    for (method, path) in [
        (Method::POST, "acme/message:send"),
        (Method::GET, "acme/tasks/t1"),
        (Method::GET, "acme/extendedAgentCard"),
    ] {
        let Refusal::Error(error) = refused(method, path) else {
            panic!("{path} is a tenant problem");
        };
        assert_eq!(error.code, error_code::INVALID_PARAMS, "{path}");
        assert!(error.message.contains("acme"), "{}", error.message);
    }
    let body = r#"{"tenant": "acme", "message": {"messageId": "m", "role": "ROLE_USER", "parts": [{"text": "hi"}]}}"#;
    match operation(&Method::POST, "message:send", Vec::new(), body.as_bytes()) {
        Err(Refusal::Error(error)) => assert_eq!(error.code, error_code::INVALID_PARAMS),
        _ => panic!("a tenant in the body is refused"),
    }
}

#[test]
fn a_body_naming_another_task_than_the_path_is_refused() {
    let body = r#"{"taskId": "t2", "url": "https://example.com/hook"}"#;
    match operation(
        &Method::POST,
        "tasks/t1/pushNotificationConfigs",
        Vec::new(),
        body.as_bytes(),
    ) {
        Err(Refusal::Error(error)) => assert_eq!(error.code, error_code::INVALID_PARAMS),
        _ => panic!("refused"),
    }
}

#[test]
fn the_interface_url_sits_beside_the_json_rpc_root() {
    assert_eq!(
        interface_url("http://127.0.0.1:8/"),
        "http://127.0.0.1:8/json"
    );
    assert_eq!(
        interface_url("https://proxy.example/a2a/"),
        "https://proxy.example/a2a/json"
    );
}
