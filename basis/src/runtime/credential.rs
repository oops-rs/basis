//! The credential a custom endpoint is spoken to with — or none.

use std::sync::Arc;

use async_trait::async_trait;
use mentra::provider_core::{CredentialSource, ProviderCredentials, ProviderError};

/// What a custom endpoint's request carries for authorization: the key
/// resolution found, or nothing.
///
/// One type for both answers [`resolve`](crate::provider::resolve) can give,
/// so a base URL builds one provider type rather than two chosen by whether a
/// key exists. mentra has both halves — `StaticCredentialSource` for a key
/// and a no-credentials source for none — but the second is private to its
/// runtime crate, and the first cannot say "none". With nothing here the
/// definition's auth scheme is set to `None` as well, so the request carries
/// no `Authorization` header at all rather than an empty bearer.
///
/// Deliberately not `Debug`: the one field is the secret.
#[derive(Clone)]
pub(super) struct Credential(Option<Arc<str>>);

impl Credential {
    pub(super) fn new(api_key: Option<&str>) -> Self {
        Self(api_key.map(Arc::from))
    }

    /// Whether a request made with this carries a key.
    pub(super) const fn is_some(&self) -> bool {
        self.0.is_some()
    }
}

#[async_trait]
impl CredentialSource for Credential {
    async fn credentials(&self) -> Result<ProviderCredentials, ProviderError> {
        Ok(ProviderCredentials {
            bearer_token: self.0.as_deref().map(str::to_string),
            ..ProviderCredentials::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_key_becomes_the_bearer_token() {
        let credentials = Credential::new(Some("k"))
            .credentials()
            .await
            .expect("resolves");

        assert_eq!(credentials.bearer_token.as_deref(), Some("k"));
    }

    #[tokio::test]
    async fn no_key_means_no_token_rather_than_an_empty_one() {
        let credentials = Credential::new(None).credentials().await.expect("resolves");

        assert_eq!(credentials.bearer_token, None);
        assert!(!Credential::new(None).is_some());
    }
}
