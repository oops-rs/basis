//! An [`A2AError`] as the HTTP+JSON binding reports it: an AIP-193 `Status`
//! under the HTTP status spec §5.4 assigns, with a `google.rpc.ErrorInfo`
//! naming the A2A error (spec §11.6).
//!
//! The mapping is written out here rather than taken from `a2a-lf`'s
//! `http_status_code`, because that one answers `400` for
//! `TaskNotCancelableError` and `ContentTypeNotSupportedError`, where the
//! spec's table says `409 Conflict` and `415 Unsupported Media Type`.

use a2a::{
    A2AError, ERROR_INFO_TYPE as ERROR_INFO, PROTOCOL_DOMAIN as DOMAIN, error_code, error_reason,
};
use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

/// The HTTP status and the `google.rpc.Code` name for `code`.
pub(crate) fn status(code: i32) -> (StatusCode, &'static str) {
    match code {
        error_code::TASK_NOT_FOUND => (StatusCode::NOT_FOUND, "NOT_FOUND"),
        error_code::TASK_NOT_CANCELABLE => (StatusCode::CONFLICT, "FAILED_PRECONDITION"),
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
        | error_code::UNSUPPORTED_OPERATION
        | error_code::VERSION_NOT_SUPPORTED => (StatusCode::BAD_REQUEST, "UNIMPLEMENTED"),
        error_code::CONTENT_TYPE_NOT_SUPPORTED => {
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "INVALID_ARGUMENT")
        }
        error_code::INVALID_AGENT_RESPONSE => (StatusCode::BAD_GATEWAY, "INTERNAL"),
        error_code::EXTENDED_CARD_NOT_CONFIGURED | error_code::EXTENSION_SUPPORT_REQUIRED => {
            (StatusCode::BAD_REQUEST, "FAILED_PRECONDITION")
        }
        error_code::PARSE_ERROR | error_code::INVALID_REQUEST | error_code::INVALID_PARAMS => {
            (StatusCode::BAD_REQUEST, "INVALID_ARGUMENT")
        }
        // In this binding a method is a path; one nothing is served at is
        // simply not there.
        error_code::METHOD_NOT_FOUND => (StatusCode::NOT_FOUND, "NOT_FOUND"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL"),
    }
}

pub(crate) fn body(error: &A2AError) -> Value {
    let (status, name) = status(error.code);
    json!({"error": {
        "code": status.as_u16(),
        "status": name,
        "message": error.message,
        "details": [{
            "@type": ERROR_INFO,
            "reason": error_reason(error.code),
            "domain": DOMAIN,
        }],
    }})
}

pub(crate) fn response(error: A2AError) -> Response {
    let (status, _) = status(error.code);
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Json(body(&error)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_follow_the_specs_table() {
        for (error, expected) in [
            (A2AError::task_not_found("x"), 404),
            (A2AError::task_not_cancelable("x"), 409),
            (A2AError::push_notification_not_supported(), 400),
            (A2AError::unsupported_operation("x"), 400),
            (A2AError::content_type_not_supported(), 415),
            (A2AError::invalid_agent_response(), 502),
            (A2AError::extended_card_not_configured(), 400),
            (A2AError::version_not_supported("0.3"), 400),
            (A2AError::invalid_params("x"), 400),
            (A2AError::internal("x"), 500),
        ] {
            assert_eq!(status(error.code).0.as_u16(), expected, "{error:?}");
        }
    }

    #[test]
    fn the_body_names_the_a2a_error_in_its_error_info() {
        let body = body(&A2AError::task_not_found("t-1"));
        assert_eq!(body["error"]["code"], 404);
        assert_eq!(body["error"]["status"], "NOT_FOUND");
        let info = &body["error"]["details"][0];
        assert_eq!(info["@type"], ERROR_INFO);
        assert_eq!(info["reason"], "TASK_NOT_FOUND");
        assert_eq!(info["domain"], DOMAIN);
    }
}
