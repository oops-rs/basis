//! Where a webhook may point (ADR-0030 §4; spec §13.2).
//!
//! A push config is a URL a caller names and this server then POSTs to, from
//! inside whatever network it runs in. Unchecked, that is a request forgery
//! primitive: a caller names `http://169.254.169.254/` or an admin port on
//! the operator's LAN and this server reaches it for them. So a webhook is
//! checked before anything is sent and again at every delivery:
//!
//! - `http` or `https`, nothing else;
//! - the host is resolved, and every address it resolves to must be public —
//!   not loopback, private (RFC 1918, CGNAT `100.64/10`, IPv6 ULA),
//!   link-local, unspecified, multicast or broadcast — unless the operator
//!   allows private webhooks;
//! - the delivery connects to exactly the addresses that were checked
//!   ([`Target::addresses`] pinned into the client), so a DNS answer that
//!   changes between the check and the connect — DNS rebinding — cannot
//!   move the request somewhere the check never saw.
//!
//! A host that resolves to a mix of public and private addresses is refused
//! outright rather than filtered: there is no reading of such an answer that
//! makes it a webhook the caller should be allowed to aim this server at.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::Url;

/// A webhook that passed the check: its URL, and the addresses it was
/// checked at.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub(crate) url: Url,
    /// The host to pin, when the URL names one rather than an IP literal.
    pub(crate) host: Option<String>,
    pub(crate) addresses: Vec<SocketAddr>,
}

/// Why a webhook was refused. The message is the caller's to read; it names
/// the rule, never an address the server resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refused {
    #[error("the webhook URL does not parse: {0}")]
    Unparsable(String),
    #[error("the webhook URL must be http or https")]
    Scheme,
    #[error("the webhook URL has no host")]
    NoHost,
    #[error("the webhook host does not resolve")]
    Unresolved,
    #[error(
        "the webhook host resolves to a loopback, private, link-local or otherwise non-public \
         address, which this server does not send to"
    )]
    NotPublic,
}

/// Checks `url` as a webhook: its scheme, then every address its host
/// resolves to, unless `allow_private`.
pub(crate) async fn check(url: &str, allow_private: bool) -> Result<Target, Refused> {
    let url = Url::parse(url).map_err(|error| Refused::Unparsable(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Refused::Scheme);
    }
    let port = url.port_or_known_default().ok_or(Refused::Scheme)?;
    let (host, addresses) = match url.host().ok_or(Refused::NoHost)? {
        url::Host::Ipv4(ip) => (None, vec![SocketAddr::new(IpAddr::V4(ip), port)]),
        url::Host::Ipv6(ip) => (None, vec![SocketAddr::new(IpAddr::V6(ip), port)]),
        url::Host::Domain(domain) => {
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
                .await
                .map_err(|_| Refused::Unresolved)?
                .collect();
            (Some(domain.to_string()), resolved)
        }
    };
    if addresses.is_empty() {
        return Err(Refused::Unresolved);
    }
    if !allow_private && addresses.iter().any(|address| !is_public(address.ip())) {
        return Err(Refused::NotPublic);
    }
    Ok(Target {
        url,
        host,
        addresses,
    })
}

/// Whether `ip` is an address on the public internet, as far as a webhook
/// is concerned. An IPv4-mapped IPv6 address is judged as the IPv4 address
/// it is.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => is_public_v4(mapped),
            None => is_public_v6(ip),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    let cgnat = first == 100 && (64..128).contains(&second);
    // 0.0.0.0/8 is "this network": a destination only in the sense that
    // some stacks connect it to the local host.
    let this_network = first == 0;
    !(ip.is_loopback()
        || ip.is_private()
        || cgnat
        || ip.is_link_local()
        || ip.is_unspecified()
        || this_network
        || ip.is_multicast()
        || ip.is_broadcast())
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    let unique_local = (first & 0xfe00) == 0xfc00;
    let link_local = (first & 0xffc0) == 0xfe80;
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || unique_local || link_local)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn only_public_addresses_are_public() {
        for public in [
            "93.184.216.34",
            "8.8.8.8",
            "2606:2800:220:1::1",
            "100.128.0.1",
        ] {
            assert!(is_public(ip(public)), "{public}");
        }
        for private in [
            "127.0.0.1",
            "127.255.255.254",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.169.254",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip(private)), "{private}");
        }
    }

    #[tokio::test]
    async fn the_scheme_must_be_http_or_https() {
        for url in ["ftp://example.com/", "file:///etc/passwd", "gopher://x/"] {
            assert_eq!(
                check(url, true).await.unwrap_err(),
                Refused::Scheme,
                "{url}"
            );
        }
        assert!(matches!(
            check("not a url", true).await.unwrap_err(),
            Refused::Unparsable(_)
        ));
    }

    #[tokio::test]
    async fn a_private_literal_is_refused_unless_allowed() {
        for url in [
            "http://127.0.0.1:8080/hook",
            "http://[::1]/hook",
            "https://169.254.169.254/latest",
            "http://10.0.0.5/",
        ] {
            assert_eq!(
                check(url, false).await.unwrap_err(),
                Refused::NotPublic,
                "{url}"
            );
            let allowed = check(url, true).await.unwrap();
            assert!(allowed.host.is_none(), "a literal needs no pinning");
        }
    }

    #[tokio::test]
    async fn a_name_resolving_to_loopback_is_refused_and_pinned_when_allowed() {
        assert_eq!(
            check("http://localhost:9/hook", false).await.unwrap_err(),
            Refused::NotPublic
        );
        let allowed = check("http://localhost:9/hook", true).await.unwrap();
        assert_eq!(allowed.host.as_deref(), Some("localhost"));
        assert!(allowed.addresses.iter().all(|address| address.port() == 9));
    }

    #[tokio::test]
    async fn a_public_literal_passes() {
        let target = check("https://93.184.216.34/hook", false).await.unwrap();
        assert_eq!(target.addresses[0].port(), 443);
    }
}
