//! Who may call a served agent, and who each caller is.
//!
//! ADR-0030 §8: a server holds named bearer tokens, and the name a token was
//! given is the [`Principal`] every task it creates is recorded under. One
//! token that sees every task is a single-tenant server; several names make
//! one served agent shareable by parties that cannot see each other's work.

use std::fmt;

/// The principal every task belongs to when the server has exactly one
/// caller: the single [`ServeConfig::with_token`](crate::ServeConfig::with_token)
/// holder, or anybody reaching a loopback server with no token at all.
///
/// One name for both, and the name every record written before principals
/// existed is read back under — so upgrading a single-token or tokenless
/// server keeps every task its one caller could already see.
pub const DEFAULT_PRINCIPAL: &str = "default";

/// The bearer token every call but the public card must carry.
///
/// Held privately and never printed: `Debug` redacts it, and nothing in this
/// crate formats it into an error.
#[derive(Clone)]
pub struct BearerToken(String);

impl BearerToken {
    /// `None` for an empty or all-whitespace token, which would authenticate
    /// a request carrying `Authorization: Bearer ` and nothing else.
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        (!token.trim().is_empty()).then_some(Self(token))
    }

    /// Whether `presented` is this token, compared in time that depends only
    /// on the lengths — the bytes of a wrong guess do not change how long it
    /// takes to refuse.
    pub(crate) fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();
        if expected.len() != presented.len() {
            return false;
        }
        expected
            .iter()
            .zip(presented)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
    }

    fn same_as(&self, other: &Self) -> bool {
        self.matches(&other.0)
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

/// Who a request came from: the name of the token it carried, or the
/// anonymous caller of a loopback server with no token.
///
/// Tasks are owned by name. Whether the caller proved it with a token is
/// kept beside the name because one operation — the extended card — is for
/// authenticated callers only (ADR-0030 §5).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Principal {
    name: String,
    authenticated: bool,
}

impl Principal {
    /// A principal a host has authenticated by means of its own, for calling
    /// [`A2aService`](crate::A2aService) directly. `None` for a name that is
    /// not 1–64 ASCII letters, digits, `-`, `_` or `.` — names are written
    /// into the task index and printed in logs, so they are kept plain.
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        let plain = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        plain.then_some(Self {
            name,
            authenticated: true,
        })
    }

    /// The one caller of a server with no token: anybody who can reach its
    /// loopback address.
    pub fn anonymous() -> Self {
        Self {
            name: DEFAULT_PRINCIPAL.to_string(),
            authenticated: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this caller presented a credential, rather than merely
    /// reaching a tokenless loopback server.
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Whether this caller owns what was recorded under `owner`.
    pub(crate) fn owns(&self, owner: &str) -> bool {
        self.name == owner
    }
}

/// Why a set of principals cannot be served.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrincipalError {
    #[error("principal {0} is configured twice")]
    DuplicateName(String),
    /// Two names for one token would make whichever is checked first the
    /// owner of the other's tasks.
    #[error("principals {0} and {1} have the same token")]
    SharedToken(String, String),
}

/// Every token a server accepts, each under its principal's name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Principals {
    tokens: Vec<(Principal, BearerToken)>,
}

impl Principals {
    pub(crate) fn new(tokens: Vec<(Principal, BearerToken)>) -> Result<Self, PrincipalError> {
        for (index, (principal, token)) in tokens.iter().enumerate() {
            for (other, other_token) in &tokens[..index] {
                if other.name == principal.name {
                    return Err(PrincipalError::DuplicateName(principal.name.clone()));
                }
                if other_token.same_as(token) {
                    return Err(PrincipalError::SharedToken(
                        other.name.clone(),
                        principal.name.clone(),
                    ));
                }
            }
        }
        Ok(Self { tokens })
    }

    /// Whether any token is configured — what a non-loopback bind requires
    /// and what the card's security scheme declares.
    pub(crate) fn any(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// Who `presented` (the token of an `Authorization: Bearer` header, if
    /// any) is. With no tokens configured everyone is the anonymous
    /// principal. Every token is compared, whichever matches, so how long a
    /// refusal takes does not say how many names were tried before it.
    pub(crate) fn authenticate(&self, presented: Option<&str>) -> Option<Principal> {
        if self.tokens.is_empty() {
            return Some(Principal::anonymous());
        }
        let presented = presented?;
        let mut found = None;
        for (principal, token) in &self.tokens {
            let matched = token.matches(presented);
            if matched && found.is_none() {
                found = Some(principal.clone());
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(value: &str) -> BearerToken {
        BearerToken::new(value).unwrap()
    }

    fn principal(name: &str) -> Principal {
        Principal::new(name).unwrap()
    }

    #[test]
    fn an_empty_token_is_no_token() {
        assert!(BearerToken::new("").is_none());
        assert!(BearerToken::new("  \t").is_none());
        assert!(BearerToken::new("s3cret").is_some());
    }

    #[test]
    fn a_token_matches_only_itself() {
        let token = token("s3cret");
        assert!(token.matches("s3cret"));
        assert!(!token.matches("s3cre"));
        assert!(!token.matches("s3crex"));
        assert!(!token.matches(""));
    }

    #[test]
    fn a_token_never_prints() {
        assert!(!format!("{:?}", token("s3cret")).contains("s3cret"));
        let principals = Principals::new(vec![(principal("a"), token("s3cret"))]).unwrap();
        assert!(!format!("{principals:?}").contains("s3cret"));
    }

    #[test]
    fn a_principal_name_is_plain() {
        assert!(Principal::new("team-a_1.x").is_some());
        for bad in ["", "a b", "a/b", "ü", &"x".repeat(65)] {
            assert!(Principal::new(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn each_token_authenticates_its_own_principal() {
        let principals = Principals::new(vec![
            (principal("alice"), token("token-a")),
            (principal("bob"), token("token-b")),
        ])
        .unwrap();
        assert_eq!(
            principals.authenticate(Some("token-b")).unwrap().name(),
            "bob"
        );
        assert!(
            principals
                .authenticate(Some("token-a"))
                .unwrap()
                .is_authenticated()
        );
        assert!(principals.authenticate(Some("token-c")).is_none());
        assert!(principals.authenticate(None).is_none());
    }

    #[test]
    fn no_tokens_is_one_anonymous_principal() {
        let anyone = Principals::default().authenticate(None).unwrap();
        assert_eq!(anyone.name(), DEFAULT_PRINCIPAL);
        assert!(!anyone.is_authenticated());
    }

    #[test]
    fn a_name_or_a_token_given_twice_is_refused() {
        assert_eq!(
            Principals::new(vec![
                (principal("a"), token("one")),
                (principal("a"), token("two")),
            ])
            .unwrap_err(),
            PrincipalError::DuplicateName("a".into())
        );
        assert_eq!(
            Principals::new(vec![
                (principal("a"), token("one")),
                (principal("b"), token("one")),
            ])
            .unwrap_err(),
            PrincipalError::SharedToken("a".into(), "b".into())
        );
    }
}
