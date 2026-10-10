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
//!    [`BODY_TIMEOUT`], and only as JSON. The handler is then given the body
//!    already read.
//!
//! The caller's [`Principal`] travels to the handler as a request extension.
//!
//! # Web pages
//!
//! A loopback server with no token trusts whoever can reach it, and a web
//! page in the operator's browser can reach it too: by a "simple" cross-site
//! request (a `text/plain` POST, which needs no CORS preflight), or by DNS
//! rebinding (a page whose own host name now resolves to `127.0.0.1`). Two
//! rules close both, before anything is parsed:
//!
//! - a request body must say it is JSON (`application/json`, or another
//!   `application/*+json`) — a type no cross-site request can send without
//!   a preflight, which this server never answers;
//! - when the server is bound to loopback, the `Host` header must be a
//!   loopback name or the host the server advertises itself at. A rebound
//!   page sends its own name.
//!
//! [`Principal`]: crate::Principal

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
    /// The host names a request may carry, when the server is on loopback;
    /// `None` when any is accepted.
    hosts: Option<Arc<Vec<String>>>,
}

impl Gate {
    /// `advertised` are the hosts this server's card names; on a loopback
    /// server they, and loopback names, are the only `Host`s accepted.
    pub(crate) fn new(loopback: bool, advertised: &[&str]) -> Self {
        let hosts = loopback.then(|| {
            Arc::new(
                advertised
                    .iter()
                    .filter_map(|url| reqwest::Url::parse(url).ok())
                    .filter_map(|url| url.host_str().map(normalize))
                    .collect(),
            )
        });
        Self {
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            hosts,
        }
    }

    fn admits_host(&self, host: Option<&str>) -> bool {
        let Some(hosts) = &self.hosts else {
            return true;
        };
        let Some(host) = host.and_then(host_name) else {
            return false;
        };
        is_loopback_name(&host) || hosts.contains(&host)
    }
}

/// A `Host` header's name, without its port, normalized.
fn host_name(header: &str) -> Option<String> {
    let header = header.trim();
    let name = if let Some(rest) = header.strip_prefix('[') {
        format!("[{}]", rest.split_once(']')?.0)
    } else {
        match header.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => {
                name.to_string()
            }
            _ => header.to_string(),
        }
    };
    (!name.is_empty()).then(|| normalize(&name))
}

fn normalize(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn is_loopback_name(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Whether `content_type` says the body is JSON.
fn is_json(content_type: Option<&str>) -> bool {
    let Some(essence) = content_type
        .and_then(|value| value.split(';').next())
        .map(|essence| essence.trim().to_ascii_lowercase())
    else {
        return false;
    };
    essence == "application/json"
        || essence
            .strip_prefix("application/")
            .is_some_and(|subtype| subtype.len() > "+json".len() && subtype.ends_with("+json"))
}

pub(crate) async fn gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Ok(_place) = Arc::clone(&state.gate.in_flight).try_acquire_owned() else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "this agent is handling as many requests as it takes at once; retry shortly",
        );
    };
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    if !state.gate.admits_host(host) {
        return refused(
            StatusCode::MISDIRECTED_REQUEST,
            "this agent answers on its loopback address and the host it advertises, not this one",
        );
    }
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
    let json = is_json(
        request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    );
    if declared.is_some_and(|length| length > 0) && !json {
        return not_json();
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
    if !body.is_empty() && !json {
        return not_json();
    }
    parts.extensions.insert(caller);
    next.run(Request::from_parts(parts, Body::from(body))).await
}

fn not_json() -> Response {
    refused(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "a request body must be JSON, sent as application/json",
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_json_is_json() {
        for json in [
            "application/json",
            "Application/JSON; charset=utf-8",
            "application/a2a+json",
        ] {
            assert!(is_json(Some(json)), "{json}");
        }
        for other in [
            "text/plain",
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "application/+json",
            "",
        ] {
            assert!(!is_json(Some(other)), "{other}");
        }
        assert!(!is_json(None));
    }

    #[test]
    fn a_loopback_server_admits_loopback_names_and_its_advertised_host_only() {
        let gate = Gate::new(true, &["https://agent.example.com/a2a/"]);
        for host in [
            "localhost",
            "localhost:8080",
            "127.0.0.1:9",
            "127.1.2.3",
            "[::1]:8080",
            "api.localhost",
            "Agent.Example.com:443",
            "agent.example.com.",
        ] {
            assert!(gate.admits_host(Some(host)), "{host}");
        }
        for host in ["evil.example", "evil.example:8080", "10.0.0.1", "[::2]", ""] {
            assert!(!gate.admits_host(Some(host)), "{host}");
        }
        assert!(!gate.admits_host(None), "a request must name its host");
        assert!(Gate::new(false, &[]).admits_host(Some("anything")));
    }
}
