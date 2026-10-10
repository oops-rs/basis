//! The eleven A2A operations, independent of the binding that carried them.
//!
//! The spec requires every binding an agent serves to be functionally
//! equivalent (§5.1). Equivalence by construction is cheaper than by review:
//! each binding only *parses* — JSON-RPC a method name and `params`, HTTP+JSON
//! a verb, a path and a query or body — into an [`Operation`], and
//! [`perform`] is the one place any of them is answered.

use a2a::{
    A2AError, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
    ListTaskPushNotificationConfigsRequest, ListTasksRequest, SendMessageRequest, StreamResponse,
    SubscribeToTaskRequest, TaskPushNotificationConfig, methods,
};
use futures::stream::BoxStream;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    card::{self, Cards},
    config::Principal,
    service::A2aService,
};

/// One request, parsed.
#[derive(Debug)]
pub(crate) enum Operation {
    SendMessage(SendMessageRequest),
    SendStreamingMessage(SendMessageRequest),
    GetTask(GetTaskRequest),
    ListTasks(ListTasksRequest),
    CancelTask(CancelTaskRequest),
    SubscribeToTask(SubscribeToTaskRequest),
    CreatePushConfig(TaskPushNotificationConfig),
    GetPushConfig(GetTaskPushNotificationConfigRequest),
    ListPushConfigs(ListTaskPushNotificationConfigsRequest),
    DeletePushConfig(DeleteTaskPushNotificationConfigRequest),
    /// Its request carries nothing but a tenant, refused while parsing.
    GetExtendedAgentCard,
}

/// What an operation answered: one value, or a stream of updates.
pub(crate) enum Outcome {
    Value(Value),
    Stream(BoxStream<'static, StreamResponse>),
}

impl Operation {
    /// The operation a JSON-RPC `method` names, with its `params`.
    pub(crate) fn from_rpc(method: &str, params: Option<Value>) -> Result<Self, A2AError> {
        if let Some(params) = &params {
            refuse_tenant(params.get("tenant").and_then(Value::as_str))?;
        }
        Ok(match method {
            methods::SEND_MESSAGE => Self::SendMessage(parse(params)?),
            methods::SEND_STREAMING_MESSAGE => Self::SendStreamingMessage(parse(params)?),
            methods::GET_TASK => Self::GetTask(parse(params)?),
            methods::LIST_TASKS => Self::ListTasks(parse_or_default(params)?),
            methods::CANCEL_TASK => Self::CancelTask(parse(params)?),
            methods::SUBSCRIBE_TO_TASK => Self::SubscribeToTask(parse(params)?),
            methods::CREATE_PUSH_CONFIG => Self::CreatePushConfig(parse(params)?),
            methods::GET_PUSH_CONFIG => Self::GetPushConfig(parse(params)?),
            methods::LIST_PUSH_CONFIGS => Self::ListPushConfigs(parse(params)?),
            methods::DELETE_PUSH_CONFIG => Self::DeletePushConfig(parse(params)?),
            methods::GET_EXTENDED_AGENT_CARD => {
                parse_or_default::<GetExtendedAgentCardRequest>(params)?;
                Self::GetExtendedAgentCard
            }
            other => return Err(A2AError::method_not_found(other)),
        })
    }
}

/// Answers `operation` for `caller`.
pub(crate) async fn perform(
    service: &A2aService,
    cards: &Cards,
    caller: &Principal,
    operation: Operation,
) -> Result<Outcome, A2AError> {
    match operation {
        Operation::SendMessage(request) => value(service.send_message(caller, request).await?),
        Operation::SendStreamingMessage(request) => Ok(Outcome::Stream(
            service.send_streaming_message(caller, request).await?,
        )),
        Operation::GetTask(request) => value(service.get_task(caller, request)?),
        Operation::ListTasks(request) => value(service.list_tasks(caller, request)?),
        Operation::CancelTask(request) => value(service.cancel_task(caller, request).await?),
        Operation::SubscribeToTask(request) => {
            Ok(Outcome::Stream(service.subscribe_to_task(caller, request)?))
        }
        Operation::CreatePushConfig(config) => {
            value(service.create_push_config(caller, config).await?)
        }
        Operation::GetPushConfig(request) => value(service.get_push_config(caller, request)?),
        Operation::ListPushConfigs(request) => value(service.list_push_configs(caller, request)?),
        // `google.protobuf.Empty`.
        Operation::DeletePushConfig(request) => {
            service.delete_push_config(caller, request)?;
            Ok(Outcome::Value(Value::Object(Default::default())))
        }
        Operation::GetExtendedAgentCard => {
            Ok(Outcome::Value(card::to_wire(cards.extended_for(caller)?)))
        }
    }
}

/// This server serves no tenant, and its card names none: a request naming
/// one was meant for some other deployment, and answering it as if it were
/// for this one would hand the caller another tenant's view. Refused by name
/// rather than ignored. An empty tenant is the absence of one.
pub(crate) fn refuse_tenant(tenant: Option<&str>) -> Result<(), A2AError> {
    match tenant.map(str::trim).filter(|tenant| !tenant.is_empty()) {
        None => Ok(()),
        Some(tenant) => Err(A2AError::invalid_params(format!(
            "this agent serves no tenant {tenant:?}; its card names none, so send requests \
             without one"
        ))),
    }
}

/// The protocol major version served (ADR-0028 §5). Any minor is accepted,
/// since nothing here branches on it.
const MAJOR_VERSION: u32 = 1;

/// An absent version is 0.3 (spec §3.6), which is refused like any other
/// version this server does not serve — exactly as the reference server
/// refuses it. Both bindings carry it as the `A2A-Version` service
/// parameter, so both check it here.
pub(crate) fn check_version(version: Option<&str>) -> Result<(), A2AError> {
    let requested = version
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("0.3");
    let major = requested
        .split('.')
        .next()
        .and_then(|major| major.parse().ok());
    if major == Some(MAJOR_VERSION) {
        Ok(())
    } else {
        Err(A2AError::version_not_supported(requested))
    }
}

fn value(result: impl serde::Serialize) -> Result<Outcome, A2AError> {
    serde_json::to_value(result)
        .map(Outcome::Value)
        .map_err(|error| A2AError::internal(format!("encode the result: {error}")))
}

pub(crate) fn parse<T: DeserializeOwned>(params: Option<Value>) -> Result<T, A2AError> {
    let params = params.ok_or_else(|| A2AError::invalid_params("params are required"))?;
    serde_json::from_value(params).map_err(|error| A2AError::invalid_params(format!("{error}")))
}

/// For the methods whose every parameter is optional.
pub(crate) fn parse_or_default<T: DeserializeOwned>(params: Option<Value>) -> Result<T, A2AError> {
    parse(Some(
        params.unwrap_or_else(|| Value::Object(Default::default())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::error_code;
    use serde_json::json;

    #[test]
    fn a_tenant_is_refused_by_name_and_an_empty_one_is_none() {
        assert!(refuse_tenant(None).is_ok());
        assert!(refuse_tenant(Some("")).is_ok());
        let error = refuse_tenant(Some("acme")).unwrap_err();
        assert_eq!(error.code, error_code::INVALID_PARAMS);
        assert!(error.message.contains("acme"), "{}", error.message);
        let error =
            Operation::from_rpc("GetTask", Some(json!({"id": "x", "tenant": "acme"}))).unwrap_err();
        assert_eq!(error.code, error_code::INVALID_PARAMS);
    }

    #[test]
    fn version_one_of_any_minor_is_served_and_nothing_else() {
        assert!(check_version(Some("1.0")).is_ok());
        assert!(check_version(Some(" 1.2 ")).is_ok());
        assert!(check_version(Some("1")).is_ok());
        for refused in [None, Some(""), Some("0.3"), Some("2.0"), Some("one")] {
            let error = check_version(refused).unwrap_err();
            assert_eq!(error.code, error_code::VERSION_NOT_SUPPORTED, "{refused:?}");
        }
    }

    #[test]
    fn an_unknown_method_is_not_found() {
        let error = Operation::from_rpc("Nope", None).unwrap_err();
        assert_eq!(error.code, error_code::METHOD_NOT_FOUND);
    }
}
