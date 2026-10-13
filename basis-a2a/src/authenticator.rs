//! Who a request is from, decided by whichever authenticator the operator
//! chose (ADR-0036).
//!
//! The server's own — named bearer tokens, and the anonymous caller of a
//! tokenless loopback server — is one [`Authenticator`] among any. A server
//! behind a gateway that signs a short-lived token per request plugs in one
//! that checks those instead, with
//! [`ServeConfig::with_authenticator`](crate::ServeConfig::with_authenticator).

use std::collections::HashMap;

use a2a::{SecurityRequirement, SecurityScheme};
use futures::future::BoxFuture;

use crate::config::Principal;

/// What an authenticator is shown of a request: its method, URI, version and
/// headers — never its body, which is not read until a caller is known.
pub use axum::http::request::Parts as RequestHead;

/// Turns a request's credentials into the [`Principal`] it acts as.
///
/// Called once per request, before a byte of its body is read, while the
/// request holds one of the server's in-flight places — so an authenticator
/// that awaits I/O (refreshing a key set, say) must bound it itself, or a
/// caller can hold places as long as that I/O hangs. The public card is the
/// one path never passed to it.
pub trait Authenticator: Send + Sync + 'static {
    /// Who sent the request with `head`, or why it is refused.
    ///
    /// A principal built with [`Principal::new`] is an authenticated caller:
    /// it owns its tasks apart from every other (ADR-0030 §8) and may be
    /// served the extended card. [`Principal::anonymous`] is the one caller
    /// of a server that authenticates nobody.
    fn authenticate<'a>(
        &'a self,
        head: &'a RequestHead,
    ) -> BoxFuture<'a, Result<Principal, Refusal>>;

    /// What the card declares a caller must present: `None` for a server
    /// that authenticates nobody. Read once, when the service opens.
    ///
    /// `None` is held to what a tokenless server is: it binds only to
    /// loopback, serves no extended card, and serves push notifications or
    /// asks its caller only when anonymous callers are trusted.
    fn security(&self) -> Option<Security>;

    /// The `WWW-Authenticate` value a `401` carries. Defaults to `Bearer`.
    fn challenge(&self) -> &str {
        "Bearer"
    }
}

/// Why a request was refused before it reached a handler. Neither carries a
/// body: what was wrong with a credential is not told to whoever sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `401`: no credential, or one that does not prove who the caller is.
    Unauthenticated,
    /// `403`: a caller proved who it is, and is not allowed here.
    Forbidden,
}

/// The security schemes a card declares, and which of them a caller must
/// present. Built so that every requirement names a declared scheme.
#[derive(Debug, Clone, PartialEq)]
pub struct Security {
    schemes: HashMap<String, SecurityScheme>,
    requirements: Vec<SecurityRequirement>,
}

impl Security {
    /// `scheme`, under `name`, required on every call.
    pub fn required(name: impl Into<String>, scheme: SecurityScheme) -> Self {
        Self {
            schemes: HashMap::new(),
            requirements: Vec::new(),
        }
        .or(name, scheme)
    }

    /// Accepts `scheme`, under `name`, as an alternative to every scheme
    /// already declared: a caller presents any one of them.
    #[must_use]
    pub fn or(mut self, name: impl Into<String>, scheme: SecurityScheme) -> Self {
        let name = name.into();
        self.requirements
            .retain(|requirement| !(requirement.len() == 1 && requirement.contains_key(&name)));
        self.requirements
            .push(HashMap::from([(name.clone(), Vec::new())]));
        self.schemes.insert(name, scheme);
        self
    }

    pub fn schemes(&self) -> &HashMap<String, SecurityScheme> {
        &self.schemes
    }

    pub fn requirements(&self) -> &[SecurityRequirement] {
        &self.requirements
    }
}

#[cfg(test)]
mod tests {
    use a2a::{ApiKeySecurityScheme, HttpAuthSecurityScheme};

    use super::*;

    fn bearer() -> SecurityScheme {
        SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "Bearer".into(),
            description: None,
            bearer_format: Some("JWT".into()),
        })
    }

    #[test]
    fn every_requirement_names_a_declared_scheme_and_each_is_an_alternative() {
        let key = SecurityScheme::ApiKey(ApiKeySecurityScheme {
            location: "header".into(),
            name: "X-Key".into(),
            description: None,
        });
        let security = Security::required("jwt", bearer()).or("key", key);
        assert_eq!(security.schemes().len(), 2);
        assert_eq!(
            security.requirements(),
            [
                HashMap::from([("jwt".to_string(), Vec::new())]),
                HashMap::from([("key".to_string(), Vec::new())]),
            ]
        );
        let again = security.or("jwt", bearer());
        assert_eq!(
            again.schemes().len(),
            2,
            "a name declared twice is replaced"
        );
        assert_eq!(again.requirements().len(), 2);
    }
}
