//! One webhook POST: the spec's payload and headers, sent to the address the
//! guard checked and nowhere else.

use std::time::Duration;

use a2a::{StreamResponse, TaskPushNotificationConfig};
use reqwest::{Client, header, redirect};

use super::guard::{self, Refused};

/// How long one delivery may take before it counts as failed — inside the
/// 10–30 seconds spec §4.3.3 recommends.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The headers a config's `token` travels in. Both, because the two
/// authorities disagree: spec §6.6's example names the first, and the
/// official SDKs read the second. A receiver of either kind finds its token,
/// and the token is the receiver's own, so saying it twice tells no one
/// anything new.
pub(crate) const TOKEN_HEADERS: [&str; 2] = ["X-A2A-Notification-Token", "A2A-Notification-Token"];

/// Why one delivery did not land.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Failed {
    #[error("{0}")]
    Refused(#[from] Refused),
    #[error("the webhook answered {0}")]
    Status(reqwest::StatusCode),
    #[error("the webhook could not be reached: {0}")]
    Unreachable(String),
}

/// POSTs `update` to `config`'s webhook. The URL is checked again here —
/// a name that resolved to a public address when the config was created may
/// not now — and the request is pinned to what this check resolved.
///
/// Redirects are not followed and no proxy is used: either would send the
/// request to an address the guard never saw.
pub(crate) async fn deliver(
    config: &TaskPushNotificationConfig,
    update: &StreamResponse,
    allow_private: bool,
) -> Result<(), Failed> {
    let target = guard::check(&config.url, allow_private).await?;
    let mut client = Client::builder()
        .no_proxy()
        .redirect(redirect::Policy::none())
        .timeout(TIMEOUT);
    if let Some(host) = &target.host {
        client = client.resolve_to_addrs(host, &target.addresses);
    }
    let client = client
        .build()
        .map_err(|error| Failed::Unreachable(error.to_string()))?;
    let mut request = client.post(target.url).json(update);
    if let Some(authentication) = &config.authentication
        && let Some(credentials) = authentication
            .credentials
            .as_deref()
            .filter(|credentials| !credentials.is_empty())
    {
        request = request.header(
            header::AUTHORIZATION,
            format!("{} {credentials}", authentication.scheme.trim()),
        );
    }
    if let Some(token) = config.token.as_deref().filter(|token| !token.is_empty()) {
        for header in TOKEN_HEADERS {
            request = request.header(header, token);
        }
    }
    let response = request
        .send()
        .await
        // Without the URL reqwest would append: a webhook URL can carry a
        // credential of the caller's in its query.
        .map_err(|error| Failed::Unreachable(error.without_url().to_string()))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(Failed::Status(response.status()))
    }
}
