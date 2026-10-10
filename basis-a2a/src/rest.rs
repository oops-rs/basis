//! The HTTP+JSON binding (spec §11): the same operations as JSON-RPC, as
//! resources under [`HTTP_JSON_PATH`].
//!
//! Paths follow the normative `a2a.proto`'s HTTP annotations, which are
//! relative to the interface URL the card advertises:
//!
//! | verb | path | operation |
//! | --- | --- | --- |
//! | `POST` | `/message:send` | `SendMessage` |
//! | `POST` | `/message:stream` | `SendStreamingMessage` (SSE) |
//! | `GET` | `/tasks` | `ListTasks` |
//! | `GET` | `/tasks/{id}` | `GetTask` |
//! | `POST` | `/tasks/{id}:cancel` | `CancelTask` |
//! | `GET`, `POST` | `/tasks/{id}:subscribe` | `SubscribeToTask` (SSE) |
//! | `POST`, `GET` | `/tasks/{id}/pushNotificationConfigs` | create, list |
//! | `GET`, `DELETE` | `/tasks/{id}/pushNotificationConfigs/{configId}` | get, delete |
//! | `GET` | `/extendedAgentCard` | `GetExtendedAgentCard` |
//!
//! `SubscribeToTask` answers both verbs: the spec's prose table says `POST`
//! and the proto it calls normative says `GET`, and a client built from
//! either must reach it.
//!
//! **Tenants.** The proto also binds every path under a `/{tenant}` prefix.
//! This server serves no tenant and its card names none, so a
//! tenant-prefixed path — like a `tenant` field in a body — is recognized
//! and refused by name, never served as if the prefix were not there: the
//! caller meant some other deployment's tenant.
//!
//! Errors are AIP-193 `Status` objects carrying a `google.rpc.ErrorInfo`
//! whose reason names the A2A error (spec §11.6), under the HTTP status the
//! spec's §5.4 table assigns — see [`error`].

mod error;

use a2a::{
    A2AError, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, GetTaskRequest, ListTaskPushNotificationConfigsRequest,
    StreamResponse, SubscribeToTaskRequest,
};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::stream::BoxStream;
use serde_json::{Map, Value};

use crate::{
    config::Principal,
    http::{self, AppState},
    ops::{self, Operation, Outcome, parse, parse_or_default, refuse_tenant},
};

/// Where the HTTP+JSON interface is mounted, below the JSON-RPC endpoint's
/// own root — the sibling layout the spec's sample card uses
/// (`…/a2a/v1` for JSON-RPC, `…/a2a/json` for HTTP+JSON).
pub const HTTP_JSON_PATH: &str = "/json";

/// The interface URL the card advertises for HTTP+JSON, given the JSON-RPC
/// one: the same root, with [`HTTP_JSON_PATH`] under it.
pub(crate) fn interface_url(json_rpc_endpoint: &str) -> String {
    format!(
        "{}{HTTP_JSON_PATH}",
        json_rpc_endpoint.trim_end_matches('/')
    )
}

/// Answers every request under [`HTTP_JSON_PATH`].
pub(crate) async fn handle(
    State(state): State<AppState>,
    Extension(caller): Extension<Principal>,
    method: Method,
    Path(path): Path<String>,
    Query(query): Query<Vec<(String, String)>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(error) = ops::check_version(http::version(&headers, None).as_deref()) {
        return error::response(error);
    }
    let operation = match operation(&method, &path, query, &body) {
        Ok(operation) => operation,
        Err(Refusal::Method) => return method_not_allowed(),
        Err(Refusal::Error(error)) => return error::response(error),
    };
    match ops::perform(&state.service, &state.cards, &caller, operation).await {
        Ok(Outcome::Value(value)) => Json(value).into_response(),
        Ok(Outcome::Stream(stream)) => sse(stream),
        Err(error) => error::response(error),
    }
}

/// The resource a path names, before the verb is considered.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Send,
    Stream,
    Tasks,
    Task(&'a str),
    Cancel(&'a str),
    Subscribe(&'a str),
    PushConfigs(&'a str),
    PushConfig(&'a str, &'a str),
    ExtendedCard,
}

fn route<'a>(segments: &[&'a str]) -> Option<Route<'a>> {
    let id = |id: &str| !id.is_empty();
    Some(match segments {
        ["message:send"] => Route::Send,
        ["message:stream"] => Route::Stream,
        ["tasks"] => Route::Tasks,
        ["extendedAgentCard"] => Route::ExtendedCard,
        ["tasks", task] => match task.rsplit_once(':') {
            Some((task, "cancel")) if id(task) => Route::Cancel(task),
            Some((task, "subscribe")) if id(task) => Route::Subscribe(task),
            Some(_) => return None,
            None if id(task) => Route::Task(task),
            None => return None,
        },
        ["tasks", task, "pushNotificationConfigs"] if id(task) => Route::PushConfigs(task),
        ["tasks", task, "pushNotificationConfigs", config] if id(task) && id(config) => {
            Route::PushConfig(task, config)
        }
        _ => return None,
    })
}

enum Refusal {
    /// The path names a resource, but not under this verb.
    Method,
    Error(A2AError),
}

impl From<A2AError> for Refusal {
    fn from(error: A2AError) -> Self {
        Self::Error(error)
    }
}

/// The operation `method` on `path` asks for. `path` is relative to
/// [`HTTP_JSON_PATH`], with no leading slash.
fn operation(
    method: &Method,
    path: &str,
    query: Vec<(String, String)>,
    body: &[u8],
) -> Result<Operation, Refusal> {
    let segments: Vec<&str> = path.split('/').collect();
    let route = match route(&segments) {
        Some(route) => route,
        None => match segments.split_first() {
            Some((tenant, rest)) if route(rest).is_some() => {
                return Err(refuse_tenant(Some(tenant))
                    .err()
                    .unwrap_or_else(not_found)
                    .into());
            }
            _ => return Err(not_found().into()),
        },
    };
    let query = query_object(query)?;
    Ok(match (route, method.clone()) {
        (Route::Send, Method::POST) => Operation::SendMessage(parse(Some(body_of(body)?))?),
        (Route::Stream, Method::POST) => {
            Operation::SendStreamingMessage(parse(Some(body_of(body)?))?)
        }
        (Route::Tasks, Method::GET) => Operation::ListTasks(parse_or_default(Some(query))?),
        (Route::Task(id), Method::GET) => Operation::GetTask(GetTaskRequest {
            id: id.to_string(),
            history_length: integer(&query, "historyLength")?,
            tenant: None,
        }),
        (Route::Cancel(id), Method::POST) => {
            let body = optional_body(body)?;
            Operation::CancelTask(CancelTaskRequest {
                id: id.to_string(),
                metadata: body
                    .get("metadata")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| A2AError::invalid_params(format!("metadata: {error}")))?,
                tenant: None,
            })
        }
        (Route::Subscribe(id), Method::GET | Method::POST) => {
            Operation::SubscribeToTask(SubscribeToTaskRequest {
                id: id.to_string(),
                tenant: None,
            })
        }
        (Route::PushConfigs(task), Method::POST) => {
            let mut config = body_of(body)?;
            match config.get("taskId").and_then(Value::as_str) {
                Some(named) if !named.is_empty() && named != task => {
                    return Err(A2AError::invalid_params(
                        "taskId in the body names a different task than the path",
                    )
                    .into());
                }
                _ => config["taskId"] = Value::String(task.to_string()),
            }
            Operation::CreatePushConfig(parse(Some(config))?)
        }
        (Route::PushConfigs(task), Method::GET) => {
            Operation::ListPushConfigs(ListTaskPushNotificationConfigsRequest {
                task_id: task.to_string(),
                page_size: integer(&query, "pageSize")?,
                page_token: query
                    .get("pageToken")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                tenant: None,
            })
        }
        (Route::PushConfig(task, id), Method::GET) => {
            Operation::GetPushConfig(GetTaskPushNotificationConfigRequest {
                task_id: task.to_string(),
                id: id.to_string(),
                tenant: None,
            })
        }
        (Route::PushConfig(task, id), Method::DELETE) => {
            Operation::DeletePushConfig(DeleteTaskPushNotificationConfigRequest {
                task_id: task.to_string(),
                id: id.to_string(),
                tenant: None,
            })
        }
        (Route::ExtendedCard, Method::GET) => Operation::GetExtendedAgentCard,
        _ => return Err(Refusal::Method),
    })
}

fn not_found() -> A2AError {
    A2AError::method_not_found("no HTTP+JSON operation is served at this path")
}

/// A request body, which must be a JSON object naming no tenant.
fn body_of(body: &[u8]) -> Result<Value, A2AError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| A2AError::parse_error(format!("the body is not JSON: {error}")))?;
    if !value.is_object() {
        return Err(A2AError::invalid_params("the body must be a JSON object"));
    }
    refuse_tenant(value.get("tenant").and_then(Value::as_str))?;
    Ok(value)
}

/// A body that may be absent, as `:cancel`'s is.
fn optional_body(body: &[u8]) -> Result<Value, A2AError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        Ok(Value::Object(Map::new()))
    } else {
        body_of(body)
    }
}

/// The query string as the JSON object the request type it stands for
/// would be parsed from (spec §11.5): camelCase names, numbers and booleans
/// as their decimal and lowercase spellings, and a state either as the
/// enum's own name or the short form the spec's example uses
/// (`status=working`).
fn query_object(query: Vec<(String, String)>) -> Result<Value, A2AError> {
    let mut object = Map::new();
    for (key, value) in query {
        let value = match key.as_str() {
            "pageSize" | "historyLength" => Value::from(
                value
                    .parse::<i64>()
                    .map_err(|_| A2AError::invalid_params(format!("{key} must be an integer")))?,
            ),
            "includeArtifacts" => Value::Bool(match value.as_str() {
                "true" => true,
                "false" => false,
                _ => {
                    return Err(A2AError::invalid_params(
                        "includeArtifacts must be true or false",
                    ));
                }
            }),
            "status" => Value::String(task_state(&value)),
            "tenant" => {
                refuse_tenant(Some(&value))?;
                continue;
            }
            _ => Value::String(value),
        };
        object.insert(key, value);
    }
    Ok(Value::Object(object))
}

fn task_state(value: &str) -> String {
    let upper = value.trim().to_ascii_uppercase();
    if upper.starts_with("TASK_STATE_") {
        upper
    } else {
        format!("TASK_STATE_{upper}")
    }
}

fn integer(query: &Value, key: &str) -> Result<Option<i32>, A2AError> {
    query
        .get(key)
        .and_then(Value::as_i64)
        .map(|value| {
            i32::try_from(value)
                .map_err(|_| A2AError::invalid_params(format!("{key} is out of range")))
        })
        .transpose()
}

/// Spec §11.7: each event's `data` is one `StreamResponse`, bare — no
/// JSON-RPC envelope.
fn sse(stream: BoxStream<'static, StreamResponse>) -> Response {
    http::sse(stream, |update| {
        serde_json::to_value(update).unwrap_or_default()
    })
}

fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::CONTENT_TYPE, "application/json")],
        Json(serde_json::json!({"error": {
            "code": 405,
            "status": "UNIMPLEMENTED",
            "message": "this path does not take that method",
        }})),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
