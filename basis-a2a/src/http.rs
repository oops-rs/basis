//! The HTTP surface: the public card, and the two bindings over one
//! service — JSON-RPC at the endpoint's root, HTTP+JSON under
//! [`HTTP_JSON_PATH`](crate::HTTP_JSON_PATH) — each answering a JSON body or
//! an SSE stream, behind the same authentication and the same version check.
//!
//! TLS is not terminated here (ADR-0028 §6). A deployment that leaves the
//! machine puts a proxy in front and advertises its `https://` URL with
//! [`ServeConfig::with_public_url`](crate::ServeConfig::with_public_url).

use std::{
    collections::HashMap, convert::Infallible, future::Future, io, net::SocketAddr, sync::Arc,
};

use a2a::{AgentCard, JsonRpcResponse, SVC_PARAM_VERSION, StreamResponse};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{any, get, post},
};
use futures::{StreamExt, stream::BoxStream};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::{
    card::{self, AGENT_CARD_PATH},
    config::Principal,
    rest::{self, HTTP_JSON_PATH},
    rpc::{self, Reply},
    service::A2aService,
};

/// The largest request body accepted. A prompt is bounded at 256 KiB by
/// `basis-tasks`; this leaves room for the envelope and nothing more.
const MAX_BODY: usize = 512 * 1024;

/// Why a server could not be bound.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// ADR-0028 §6: an address other machines can reach needs a token.
    #[error(
        "{0} is reachable from other machines, and this server has no token; configure one, or \
         bind a loopback address"
    )]
    Unauthenticated(SocketAddr),
    #[error("bind {0}: {1}")]
    Io(SocketAddr, #[source] io::Error),
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) service: A2aService,
    card: Arc<AgentCard>,
}

/// A bound, not yet serving, A2A server.
pub struct A2aServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    endpoint: String,
    router: Router,
}

impl A2aServer {
    /// Binds `address` for `service`. A non-loopback address is refused when
    /// the service has no token, before anything listens.
    pub async fn bind(address: SocketAddr, service: A2aService) -> Result<Self, BindError> {
        if !address.ip().is_loopback() && !service.principals().any() {
            return Err(BindError::Unauthenticated(address));
        }
        let listener = TcpListener::bind(address)
            .await
            .map_err(|error| BindError::Io(address, error))?;
        let local_addr = listener
            .local_addr()
            .map_err(|error| BindError::Io(address, error))?;
        let endpoint = service
            .public_url()
            .map_or_else(|| format!("http://{local_addr}/"), str::to_owned);
        let router = router(service, &endpoint);
        Ok(Self {
            listener,
            local_addr,
            endpoint,
            router,
        })
    }

    /// The address actually bound — the port, when `0` was asked for.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The JSON-RPC endpoint the card advertises.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The HTTP+JSON interface URL the card advertises, beside the JSON-RPC
    /// endpoint.
    pub fn http_json_url(&self) -> String {
        rest::interface_url(&self.endpoint)
    }

    /// Serves until the process ends.
    pub async fn serve(self) -> io::Result<()> {
        axum::serve(self.listener, self.router).await
    }

    /// Serves until `signal` resolves, then stops accepting and lets
    /// in-flight requests finish.
    pub async fn serve_until(
        self,
        signal: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<()> {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(signal)
            .await
    }
}

/// The routes, for a host that serves them on a listener of its own.
/// `endpoint` is the JSON-RPC URL the card advertises; the HTTP+JSON
/// interface is advertised, and served, under it at
/// [`HTTP_JSON_PATH`](crate::HTTP_JSON_PATH).
pub fn router(service: A2aService, endpoint: &str) -> Router {
    let card = Arc::new(card::build(
        service.identity(),
        endpoint,
        service.principals().any(),
    ));
    Router::new()
        .route(AGENT_CARD_PATH, get(agent_card))
        .route("/", post(json_rpc))
        .route(&format!("{HTTP_JSON_PATH}/{{*path}}"), any(rest::handle))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(AppState { service, card })
}

async fn agent_card(State(state): State<AppState>) -> Json<AgentCard> {
    Json(AgentCard::clone(&state.card))
}

async fn json_rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let Some(caller) = authenticate(&state.service, &headers) else {
        return unauthorized();
    };
    let version = version(&headers, Some(&query));
    match rpc::dispatch(&state.service, &caller, version.as_deref(), &body).await {
        Reply::Json(response) => Json(response).into_response(),
        Reply::Stream(id, updates) => sse(updates, move |update| {
            let result = serde_json::to_value(update).unwrap_or_default();
            serde_json::to_value(JsonRpcResponse::success(id.clone(), result)).unwrap_or_default()
        }),
    }
}

/// The caller's `A2A-Version`: its header, or — for the JSON-RPC binding,
/// which has always taken it there too — a query parameter.
pub(crate) fn version(
    headers: &HeaderMap,
    query: Option<&HashMap<String, String>>,
) -> Option<String> {
    headers
        .get(SVC_PARAM_VERSION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            query?
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(SVC_PARAM_VERSION))
                .map(|(_, value)| value.clone())
        })
}

/// An SSE response carrying each update as `frame` dresses it.
pub(crate) fn sse(
    updates: BoxStream<'static, StreamResponse>,
    frame: impl Fn(StreamResponse) -> Value + Send + 'static,
) -> Response {
    let events = updates
        .map(move |update| Ok::<_, Infallible>(Event::default().data(frame(update).to_string())));
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Who sent `headers`, if anyone this server accepts. Checked before any
/// body is parsed, so an unauthenticated caller learns nothing of what the
/// server would have said.
pub(crate) fn authenticate(service: &A2aService, headers: &HeaderMap) -> Option<Principal> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer);
    service.principals().authenticate(presented)
}

pub(crate) fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
    )
        .into_response()
}

/// The token in an `Authorization: Bearer <token>` value; the scheme is
/// case-insensitive, as RFC 7235 has it.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_value_yields_its_token_whatever_the_schemes_case() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
    }
}
