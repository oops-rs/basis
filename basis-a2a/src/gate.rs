//! What every request passes before a handler sees it.
//!
//! A handler that extracts its body reads that body before it runs a line of
//! its own, so authentication inside a handler comes too late: an
//! unauthenticated caller could already make the server buffer half a
//! megabyte, as slowly as it liked. This layer runs first, and in order:
//!
//! 1. **A concurrency limit.** At most [`MAX_IN_FLIGHT`] requests are handled
//!    at once; one past it is answered `503` at once rather than queued.
//! 2. **Authentication**, before a byte of the body is read. The public card
//!    is the one path that needs none.
//! 3. **A bounded body read**: at most the body limit, within
//!    [`BODY_TIMEOUT`]. The handler is then given the body already read.
//!
//! The caller's [`Principal`] travels to the handler as a request extension.

use std::{sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tokio::sync::Semaphore;

use crate::{
    card::AGENT_CARD_PATH,
    http::{self, AppState, MAX_BODY},
};

/// The most requests handled at once. A blocking `SendMessage` holds its
/// place for as long as its task runs; a stream holds it until its headers
/// are sent.
pub(crate) const MAX_IN_FLIGHT: usize = 256;

/// How long a caller has to deliver its whole body once its headers are in.
pub(crate) const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// The shared limits one router enforces.
#[derive(Clone)]
pub(crate) struct Gate {
    in_flight: Arc<Semaphore>,
}

impl Gate {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        }
    }
}

pub(crate) async fn gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Ok(_place) = Arc::clone(&state.gate.in_flight).try_acquire_owned() else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "this agent is handling as many requests as it takes at once; retry shortly",
        );
    };
    if request.uri().path() == AGENT_CARD_PATH {
        return next.run(request).await;
    }
    let Some(caller) = http::authenticate(&state.service, request.headers()) else {
        return http::unauthorized();
    };
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok())
        .and_then(|length| length.parse::<usize>().ok());
    if declared.is_some_and(|length| length > MAX_BODY) {
        return too_large();
    }
    let (mut parts, body) = request.into_parts();
    let body = match tokio::time::timeout(BODY_TIMEOUT, to_bytes(body, MAX_BODY)).await {
        Ok(Ok(body)) => body,
        // `to_bytes` fails past the limit as well as on a broken read; a
        // declared length was checked above, so this is mostly the former.
        Ok(Err(_)) => return too_large(),
        Err(_) => {
            return refused(
                StatusCode::REQUEST_TIMEOUT,
                "the request body did not arrive in time",
            );
        }
    };
    parts.extensions.insert(caller);
    next.run(Request::from_parts(parts, Body::from(body))).await
}

fn too_large() -> Response {
    refused(
        StatusCode::PAYLOAD_TOO_LARGE,
        "the request body is larger than this agent accepts",
    )
}

/// A refusal at the HTTP layer, before either binding has parsed anything —
/// like the `401` beside it, it carries no protocol body.
pub(crate) fn refused(status: StatusCode, reason: &'static str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain")], reason).into_response()
}
