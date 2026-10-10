//! The JSON-RPC binding: one request body in, one response or one event
//! stream out.
//!
//! Errors travel as JSON-RPC error objects in an HTTP 200, as the reference
//! server sends them; only authentication is refused at the HTTP layer,
//! before anything here runs.

use a2a::{A2AError, JsonRpcId, JsonRpcRequest, JsonRpcResponse, StreamResponse};
use futures::stream::BoxStream;
use serde_json::Value;

use crate::{
    config::Principal,
    ops::{self, Operation, Outcome},
    service::A2aService,
};

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
pub(crate) async fn dispatch(
    service: &A2aService,
    caller: &Principal,
    version: Option<&str>,
    body: &[u8],
) -> Reply {
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
    if let Err(error) = ops::check_version(version) {
        return Reply::error(id, error);
    }
    let operation = match Operation::from_rpc(request.method.as_str(), request.params) {
        Ok(operation) => operation,
        Err(error) => return Reply::error(id, error),
    };
    match ops::perform(service, caller, operation).await {
        Ok(Outcome::Value(result)) => Reply::Json(JsonRpcResponse::success(id, result)),
        Ok(Outcome::Stream(stream)) => Reply::Stream(id, stream),
        Err(error) => Reply::error(id, error),
    }
}
