//! The JSON-RPC binding: one request body in, one response or one event
//! stream out.
//!
//! Errors travel as JSON-RPC error objects in an HTTP 200, as the reference
//! server sends them; only authentication is refused at the HTTP layer,
//! before anything here runs.

use a2a::{A2AError, JsonRpcId, JsonRpcRequest, JsonRpcResponse, StreamResponse, methods};
use futures::stream::BoxStream;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::service::A2aService;

/// The protocol major version served (ADR-0028 §5). Any minor is accepted,
/// since nothing here branches on it.
const MAJOR_VERSION: u32 = 1;

/// What one request produced.
pub(crate) enum Reply {
    Json(JsonRpcResponse),
    Stream(JsonRpcId, BoxStream<'static, StreamResponse>),
}

impl Reply {
    fn error(id: JsonRpcId, error: A2AError) -> Self {
        Self::Json(JsonRpcResponse::error(id, error.into()))
    }
}

/// Handles one JSON-RPC request body. `version` is the caller's
/// `A2A-Version`, from its header or query parameter.
pub(crate) async fn dispatch(service: &A2aService, version: Option<&str>, body: &[u8]) -> Reply {
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => {
            return Reply::error(
                JsonRpcId::Null,
                A2AError::parse_error(format!("the body is not JSON: {error}")),
            );
        }
    };
    let id = value
        .get("id")
        .and_then(|id| serde_json::from_value(id.clone()).ok())
        .unwrap_or(JsonRpcId::Null);
    let request: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(error) => {
            return Reply::error(
                id,
                A2AError::invalid_request(format!("not a JSON-RPC request: {error}")),
            );
        }
    };
    if request.jsonrpc != "2.0" {
        return Reply::error(id, A2AError::invalid_request("jsonrpc must be \"2.0\""));
    }
    if let Err(error) = check_version(version) {
        return Reply::error(id, error);
    }
    match route(service, request.method.as_str(), request.params).await {
        Ok(Routed::Value(result)) => Reply::Json(JsonRpcResponse::success(id, result)),
        Ok(Routed::Stream(stream)) => Reply::Stream(id, stream),
        Err(error) => Reply::error(id, error),
    }
}

enum Routed {
    Value(Value),
    Stream(BoxStream<'static, StreamResponse>),
}

async fn route(
    service: &A2aService,
    method: &str,
    params: Option<Value>,
) -> Result<Routed, A2AError> {
    match method {
        methods::SEND_MESSAGE => value(service.send_message(parse(params)?).await?),
        methods::SEND_STREAMING_MESSAGE => Ok(Routed::Stream(
            service.send_streaming_message(parse(params)?).await?,
        )),
        methods::GET_TASK => value(service.get_task(parse(params)?)?),
        methods::LIST_TASKS => value(service.list_tasks(parse_or_default(params)?)?),
        methods::CANCEL_TASK => value(service.cancel_task(parse(params)?).await?),
        methods::SUBSCRIBE_TO_TASK => {
            Ok(Routed::Stream(service.subscribe_to_task(parse(params)?)?))
        }
        methods::CREATE_PUSH_CONFIG
        | methods::GET_PUSH_CONFIG
        | methods::LIST_PUSH_CONFIGS
        | methods::DELETE_PUSH_CONFIG => Err(A2AError::push_notification_not_supported()),
        // The card does not declare an extended card, and the spec answers
        // that with `UnsupportedOperationError`, not "not configured".
        methods::GET_EXTENDED_AGENT_CARD => Err(A2AError::unsupported_operation(
            "this agent has no extended card",
        )),
        other => Err(A2AError::method_not_found(other)),
    }
}

fn value(result: impl serde::Serialize) -> Result<Routed, A2AError> {
    serde_json::to_value(result)
        .map(Routed::Value)
        .map_err(|error| A2AError::internal(format!("encode the result: {error}")))
}

fn parse<T: DeserializeOwned>(params: Option<Value>) -> Result<T, A2AError> {
    let params = params.ok_or_else(|| A2AError::invalid_params("params are required"))?;
    serde_json::from_value(params).map_err(|error| A2AError::invalid_params(format!("{error}")))
}

/// For the one method whose every parameter is optional.
fn parse_or_default<T: DeserializeOwned>(params: Option<Value>) -> Result<T, A2AError> {
    parse(Some(
        params.unwrap_or_else(|| Value::Object(Default::default())),
    ))
}

/// An absent version is 0.3 (spec §3.6), which is refused like any other
/// version this server does not serve — exactly as the reference server
/// refuses it.
fn check_version(version: Option<&str>) -> Result<(), A2AError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::error_code;

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
}
