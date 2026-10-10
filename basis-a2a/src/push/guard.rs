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
//!   link-local, unspecified, multicast, documentation, benchmarking or
//!   reserved, and no IPv6 address carrying such an IPv4 one (mapped,
//!   IPv4-compatible, NAT64, 6to4) — unless the operator allows private
//!   webhooks;
//! - the delivery connects to exactly the addresses that were checked
//!   ([`Target::addresses`] pinned into the client), so a DNS answer that
//!   changes between the check and the connect — DNS rebinding — cannot
//!   move the request somewhere the check never saw.
//!
//! A host that resolves to a mix of public and private addresses is refused
//! outright rather than filtered: there is no reading of such an answer that
//! makes it a webhook the caller should be allowed to aim this server at.
//!
//! A host that does not resolve and one that resolves somewhere refused get
//! the same answer, so the refusal is no oracle for this server's own DNS —
//! which names exist on its network. The lookup is bounded by
//! [`LOOKUP_TIMEOUT`].

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

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
    /// Unresolvable, or resolving to an address refused — one answer for
    /// both, by design (see the module docs).
    #[error(
        "the webhook host does not resolve to a public address; this server does not send to \
         loopback, private, link-local or other reserved addresses"
    )]
    Unreachable,
}

/// How long resolving a webhook's host may take.
pub(crate) const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

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
            let resolved: Vec<SocketAddr> =
                tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((domain, port)))
                    .await
                    .map_err(|_| Refused::Unreachable)?
                    .map_err(|_| Refused::Unreachable)?
                    .collect();
            (Some(domain.to_string()), resolved)
        }
    };
    if addresses.is_empty() {
        return Err(Refused::Unreachable);
    }
    if !allow_private && addresses.iter().any(|address| !is_public(address.ip())) {
        return Err(Refused::Unreachable);
    }
    Ok(Target {
        url,
        host,
        addresses,
    })
}

/// Whether `ip` is an address on the public internet, as far as a webhook
/// is concerned. An IPv6 address that carries an IPv4 one — mapped,
/// IPv4-compatible, NAT64 or 6to4 — is judged as the IPv4 address it
/// reaches.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match embedded_v4(ip) {
            Embedded::V4(inner) => is_public_v4(inner),
            Embedded::Refused => false,
            Embedded::None => is_public_v6(ip),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    let within = |a: u8, b: u8, c: u8| first == a && second == b && third == c;
    let cgnat = first == 100 && (64..128).contains(&second);
    // 0.0.0.0/8 is "this network": a destination only in the sense that
    // some stacks connect it to the local host.
    let this_network = first == 0;
    // 192.0.0.0/24: IETF protocol assignments, some of them local.
    let protocol_assignments = within(192, 0, 0);
    // TEST-NET-1, -2 and -3: documentation, never anyone's webhook.
    let documentation = within(192, 0, 2) || within(198, 51, 100) || within(203, 0, 113);
    // 198.18.0.0/15: benchmarking networks, routed inside labs.
    let benchmarking = first == 198 && (second & 0xfe) == 18;
    // 240.0.0.0/4: reserved, and 255.255.255.255 with it.
    let reserved = first >= 240;
    !(ip.is_loopback()
        || ip.is_private()
        || cgnat
        || ip.is_link_local()
        || ip.is_unspecified()
        || this_network
        || protocol_assignments
        || documentation
        || benchmarking
        || ip.is_multicast()
        || reserved)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let unique_local = (segments[0] & 0xfe00) == 0xfc00;
    let link_local = (segments[0] & 0xffc0) == 0xfe80;
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || unique_local
        || link_local
        || documentation)
}

/// What an IPv6 address says about an IPv4 one inside it.
enum Embedded {
    V4(Ipv4Addr),
    /// A prefix whose IPv4 address cannot be read off portably, and that is
    /// never a public destination: local-use NAT64.
    Refused,
    None,
}

fn embedded_v4(ip: Ipv6Addr) -> Embedded {
    let segments = ip.segments();
    let last = |segments: [u16; 8]| {
        let [.., high, low] = segments;
        Ipv4Addr::from((u32::from(high) << 16) | u32::from(low))
    };
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return Embedded::V4(mapped);
    }
    match segments {
        // IPv4-compatible `::a.b.c.d` (deprecated, still routable by some
        // stacks); `::` and `::1` are themselves, not IPv4 addresses.
        [0, 0, 0, 0, 0, 0, ..] if !ip.is_unspecified() && !ip.is_loopback() => {
            Embedded::V4(last(segments))
        }
        // NAT64's well-known prefix, 64:ff9b::/96.
        [0x0064, 0xff9b, 0, 0, 0, 0, ..] => Embedded::V4(last(segments)),
        // Local-use NAT64, 64:ff9b:1::/48 (RFC 8215).
        [0x0064, 0xff9b, 0x0001, ..] => Embedded::Refused,
        // 6to4, 2002:a.b.c.d::/48.
        [0x2002, high, low, ..] => {
            Embedded::V4(Ipv4Addr::from((u32::from(high) << 16) | u32::from(low)))
        }
        _ => Embedded::None,
    }
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
            "192.0.0.8",
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
            "198.18.0.1",
            "198.19.255.255",
            "240.0.0.1",
            "2001:db8::1",
            "::127.0.0.1",
            "::10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::5db8:d822",
            "2002:7f00:1::",
            "2002:c0a8:101::1",
        ] {
            assert!(!is_public(ip(private)), "{private}");
        }
        for embedded_public in ["64:ff9b::5db8:d822", "2002:5db8:d822::1", "::93.184.216.34"] {
            assert!(is_public(ip(embedded_public)), "{embedded_public}");
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
                Refused::Unreachable,
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
            Refused::Unreachable
        );
        let allowed = check("http://localhost:9/hook", true).await.unwrap();
        assert_eq!(allowed.host.as_deref(), Some("localhost"));
        assert!(allowed.addresses.iter().all(|address| address.port() == 9));
    }

    #[tokio::test]
    async fn an_unresolvable_host_and_a_refused_one_read_the_same() {
        let unresolved = check("http://no-such-host.invalid/hook", false)
            .await
            .unwrap_err();
        let refused = check("http://127.0.0.1/hook", false).await.unwrap_err();
        assert_eq!(unresolved.to_string(), refused.to_string());
    }

    #[tokio::test]
    async fn a_public_literal_passes() {
        let target = check("https://93.184.216.34/hook", false).await.unwrap();
        assert_eq!(target.addresses[0].port(), 443);
    }
}
