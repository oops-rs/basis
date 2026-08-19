//! Who is allowed to open a bridge connection.
//!
//! # Why loopback is not enough
//!
//! A listening socket that drives an agent with write access to a workspace is
//! only as safe as the set of things that can reach it, and on a developer's
//! machine that set is larger than it looks. A WebSocket handshake is exempt
//! from the same-origin policy: any page open in the user's browser can dial
//! `ws://127.0.0.1:<port>` with no preflight and no cooperation from the
//! server. Binding to loopback keeps other machines out. It does not keep web
//! pages out, and a page is the more likely attacker — the user need only
//! visit it.
//!
//! # What is enough
//!
//! The `Origin` header. A browser sets it on every WebSocket handshake and
//! script cannot forge it, which splits the callers cleanly:
//!
//! - **No `Origin`** — not a browser. Something already running as this user,
//!   which could have run `basis` itself; refusing it protects nothing.
//! - **An `Origin`** — a page. Pages are admitted by name only.
//!
//! The allowlist starts empty, so out of the box the bridge serves native
//! clients and turns away every page, including one served from localhost.
//! Running acp-ui means naming where it is served from, which is a decision
//! someone makes once and on purpose.
//!
//! This settles DNS rebinding at the same time: rebinding is the same attack
//! wearing a hostname the user's resolver was tricked into pointing at
//! 127.0.0.1, and the browser still sends the attacker's origin with it.

use std::sync::Arc;

use tokio_tungstenite::tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, Response as HttpResponse, StatusCode, header},
};

/// What the `Origin` header on a handshake settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    /// No `Origin`: not a browser, so not the caller this guard is about.
    NotABrowser,
    /// A page, named on the allowlist.
    AllowedOrigin,
    /// A page that was not named.
    Refused,
}

/// Decides one handshake. Split from the callback so the rule can be tested
/// without a socket — the rule is the security property, not the plumbing.
pub(super) fn admit(origin: Option<&HeaderValue>, allowed: &[String]) -> Admission {
    let Some(origin) = origin else {
        return Admission::NotABrowser;
    };

    match origin.to_str() {
        Ok(named) if allowed.iter().any(|entry| entry == named) => Admission::AllowedOrigin,
        // An origin that is not text cannot match an allowlist entry, and
        // nothing that speaks HTTP correctly sends one.
        Ok(_) | Err(_) => Admission::Refused,
    }
}

/// The handshake callback `accept_hdr_async` takes.
///
/// Takes the allowlist by [`Arc`] because there is one guard per connection
/// and one list per bridge; the list is read, never changed.
///
/// The large `Err` variant is tungstenite's `Callback` signature, not a choice
/// basis can make: the error *is* the HTTP response the client gets, and it is
/// built at most once per refused connection.
#[allow(clippy::result_large_err)]
pub(super) fn origin_guard(
    allowed: Arc<Vec<String>>,
) -> impl FnOnce(&Request, Response) -> Result<Response, ErrorResponse> + Unpin {
    move |request, response| {
        match admit(request.headers().get(header::ORIGIN), &allowed) {
            Admission::NotABrowser | Admission::AllowedOrigin => Ok(response),
            // Refusing at the handshake means the page is told why, in a
            // status its own error handler surfaces, before any ACP message
            // is exchanged.
            Admission::Refused => Err(refused()),
        }
    }
}

fn refused() -> ErrorResponse {
    HttpResponse::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Some(
            "basis: this bridge does not serve web pages unless their origin is \
             allowed explicitly. A page can reach a loopback socket without \
             asking anyone, so the allowlist is the only thing standing between \
             a visited site and this workspace."
                .to_string(),
        ))
        .expect("a status and a body always build a response")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<String> {
        vec!["http://localhost:5173".to_string()]
    }

    #[test]
    fn a_caller_without_an_origin_is_not_a_browser() {
        assert_eq!(admit(None, &[]), Admission::NotABrowser);
        assert_eq!(
            admit(None, &allowed()),
            Admission::NotABrowser,
            "a native client is served whether or not any page is"
        );
    }

    #[test]
    fn a_named_origin_is_admitted() {
        assert_eq!(
            admit(
                Some(&HeaderValue::from_static("http://localhost:5173")),
                &allowed()
            ),
            Admission::AllowedOrigin
        );
    }

    #[test]
    fn an_unnamed_page_is_refused() {
        assert_eq!(
            admit(
                Some(&HeaderValue::from_static("https://evil.example")),
                &allowed()
            ),
            Admission::Refused
        );
    }

    #[test]
    fn the_empty_allowlist_refuses_every_page() {
        for origin in [
            "http://localhost:5173",
            "http://127.0.0.1:5173",
            "null",
            "https://evil.example",
        ] {
            assert_eq!(
                admit(Some(&HeaderValue::from_static(origin)), &[]),
                Admission::Refused,
                "{origin} must not be served by default: a page reaches loopback unasked"
            );
        }
    }

    #[test]
    fn an_origin_that_is_not_text_is_refused() {
        let malformed = HeaderValue::from_bytes(&[0xff, 0xfe]).expect("a header of raw bytes");

        assert_eq!(admit(Some(&malformed), &allowed()), Admission::Refused);
    }

    #[test]
    fn an_origin_must_match_exactly() {
        for near_miss in [
            "http://localhost:5174",
            "https://localhost:5173",
            "http://localhost:5173/",
            "http://localhost:5173.evil.example",
        ] {
            assert_eq!(
                admit(Some(&HeaderValue::from_static(near_miss)), &allowed()),
                Admission::Refused,
                "{near_miss} is a different origin from the one that was allowed"
            );
        }
    }
}
