//! URL and host admission for `web_fetch`: syntax, scheme, embedded
//! credentials, and the address classes the gateway never contacts.
//!
//! The address policy runs on resolved addresses, not on host names, so a name
//! that resolves to a loopback, private, link-local, or cloud-metadata address
//! is refused the same way a literal address is. The HTTP backend pins the
//! connection to the addresses checked here, so a name cannot re-resolve to a
//! different address between the check and the connection.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use url::{Host, Url};

use super::WebFetchErrorCode;
use super::backend::FetchFailure;

/// Anthropic's documented maximum URL length for web fetch, in characters.
pub(crate) const MAX_URL_CHARS: usize = 250;

/// Why a URL was refused before any network activity.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UrlRejection {
    #[error("url is empty")]
    Empty,
    #[error("url exceeds {MAX_URL_CHARS} characters")]
    TooLong,
    #[error("url is not valid: {0}")]
    Invalid(#[source] url::ParseError),
    #[error("url scheme {0:?} is not http or https")]
    Scheme(String),
    /// Well formed, but the gateway never sends credentials to a third party.
    #[error("url carries credentials")]
    Credentials,
}

impl UrlRejection {
    /// The documented error code for this rejection.
    pub(crate) const fn code(&self) -> WebFetchErrorCode {
        match self {
            Self::Empty | Self::Invalid(_) | Self::Scheme(_) => WebFetchErrorCode::InvalidToolInput,
            Self::TooLong => WebFetchErrorCode::UrlTooLong,
            Self::Credentials => WebFetchErrorCode::UrlNotAllowed,
        }
    }
}

/// Parse and admit a URL the model asked to fetch.
///
/// Only absolute `http` and `https` URLs are fetched (the parser guarantees
/// those carry a host). A URL that carries credentials is refused rather than
/// sent to a third party.
pub(crate) fn validate_url(raw: &str) -> Result<Url, UrlRejection> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(UrlRejection::Empty);
    }
    if raw.chars().count() > MAX_URL_CHARS {
        return Err(UrlRejection::TooLong);
    }
    let url = Url::parse(raw).map_err(UrlRejection::Invalid)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UrlRejection::Scheme(url.scheme().to_owned()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlRejection::Credentials);
    }
    Ok(url)
}

/// Whether an address is one the gateway may contact on behalf of a request.
///
/// Refused: loopback, unspecified, private (RFC 1918), carrier-grade NAT
/// (100.64/10), link-local (which covers the cloud metadata address
/// 169.254.169.254), multicast, broadcast, benchmarking, documentation, IETF
/// protocol assignments (192.0.0/24), and reserved ranges; for IPv6 also
/// unique-local, site-local, local-use NAT64, and every transition form that
/// embeds a refused IPv4 address.
#[must_use]
pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || first == 0
        || (first == 100 && (64..=127).contains(&second))
        || (first == 192 && second == 0 && third == 0)
        || (first == 198 && (18..=19).contains(&second))
        || first >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    let segments = ip.segments();
    let first = segments[0];
    let embedded_v4 = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) embed an IPv4 address that
    // must pass the IPv4 policy; local-use NAT64 (64:ff9b:1::/48) and the
    // deprecated IPv4-compatible form (::a.b.c.d) are refused outright.
    let nat64 = segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] && !is_public_v4(embedded_v4(segments[6], segments[7]));
    let local_nat64 = segments[..3] == [0x64, 0xff9b, 1];
    let six_to_four = first == 0x2002 && !is_public_v4(embedded_v4(segments[1], segments[2]));
    !(nat64
        || local_nat64
        || six_to_four
        || ip.to_ipv4().is_some()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first & 0xffc0) == 0xfec0
        || (first == 0x2001 && segments[1] == 0x0db8))
}

/// Resolve a URL host to the socket addresses the backend may connect to.
///
/// A literal address is checked directly; a name is looked up and every
/// returned address must pass `admit` — the backend's address policy,
/// [`is_public_ip`] unless the operator allowed private networks — so a split
/// answer cannot smuggle in a non-public address.
pub(crate) async fn resolve_addrs(
    host: &Host<&str>,
    port: u16,
    admit: impl Fn(IpAddr) -> bool,
) -> Result<Vec<SocketAddr>, FetchFailure> {
    let addrs: Vec<SocketAddr> = match host {
        Host::Ipv4(ip) => vec![SocketAddr::new(IpAddr::V4(*ip), port)],
        Host::Ipv6(ip) => vec![SocketAddr::new(IpAddr::V6(*ip), port)],
        Host::Domain(name) => tokio::net::lookup_host((*name, port))
            .await
            .map_err(|source| FetchFailure::Dns {
                host: (*name).to_owned(),
                source,
            })?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(FetchFailure::NoAddress { host: host.to_string() });
    }
    if addrs.iter().any(|addr| !admit(addr.ip())) {
        return Err(FetchFailure::NotPublic { host: host.to_string() });
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_accepts_absolute_http_urls_only() {
        assert_eq!(
            validate_url(" https://example.com/page?q=1#x ").unwrap().as_str(),
            "https://example.com/page?q=1#x"
        );
        assert_eq!(
            validate_url("http://example.com").unwrap().as_str(),
            "http://example.com/"
        );
        for (raw, expected) in [
            ("", "url is empty"),
            ("example.com/page", "url is not valid"),
            ("ftp://example.com/file", "url scheme \"ftp\" is not http or https"),
            ("file:///etc/passwd", "url scheme \"file\" is not http or https"),
            ("javascript:alert(1)", "url scheme \"javascript\" is not http or https"),
            ("http://", "url is not valid"),
        ] {
            let rejection = validate_url(raw).unwrap_err();
            assert_eq!(rejection.code(), WebFetchErrorCode::InvalidToolInput, "{raw}");
            assert!(rejection.to_string().starts_with(expected), "{raw}: {rejection}");
        }
    }

    #[test]
    fn validate_url_refuses_long_urls_and_credentials() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        let rejection = validate_url(&long).unwrap_err();
        assert!(matches!(rejection, UrlRejection::TooLong));
        assert_eq!(rejection.code(), WebFetchErrorCode::UrlTooLong);
        assert_eq!(rejection.to_string(), "url exceeds 250 characters");
        let exact = format!(
            "https://example.com/{}",
            "a".repeat(MAX_URL_CHARS - "https://example.com/".len())
        );
        assert!(
            validate_url(&exact).is_ok(),
            "exactly {MAX_URL_CHARS} characters is allowed"
        );
        for raw in ["https://user:secret@example.com/", "https://token@example.com/"] {
            let rejection = validate_url(raw).unwrap_err();
            assert!(matches!(rejection, UrlRejection::Credentials), "{raw}");
            assert_eq!(rejection.code(), WebFetchErrorCode::UrlNotAllowed);
            assert_eq!(rejection.to_string(), "url carries credentials");
        }
    }

    #[test]
    fn public_ipv4_policy_refuses_every_internal_range() {
        for refused in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "192.0.0.1",
            "192.0.0.170",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
        ] {
            let ip: IpAddr = refused.parse().unwrap();
            assert!(!is_public_ip(ip), "{refused} must be refused");
        }
        for allowed in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "192.0.1.1",
            "198.20.0.1",
        ] {
            let ip: IpAddr = allowed.parse().unwrap();
            assert!(is_public_ip(ip), "{allowed} must be allowed");
        }
    }

    #[test]
    fn public_ipv6_policy_refuses_internal_and_embedded_ranges() {
        for refused in [
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::10.0.0.1",
            "::8.8.8.8",
            "64:ff9b::10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::808:808",
            "2002:0a00:0001::1",
            "2002:c0a8:0101::1",
        ] {
            let ip: IpAddr = refused.parse().unwrap();
            assert!(!is_public_ip(ip), "{refused} must be refused");
        }
        for allowed in [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:0808:0808::1",
            "2001:db9::1",
            "fb00::1",
        ] {
            let ip: IpAddr = allowed.parse().unwrap();
            assert!(is_public_ip(ip), "{allowed} must be allowed");
        }
    }

    #[tokio::test]
    async fn resolve_addrs_checks_literals_without_a_lookup() {
        let loopback = Host::Ipv4(Ipv4Addr::LOCALHOST);
        let error = resolve_addrs(&loopback, 80, is_public_ip).await.unwrap_err();
        assert!(
            matches!(&error, FetchFailure::NotPublic { host } if host == "127.0.0.1"),
            "{error:?}"
        );
        let allowed = resolve_addrs(&loopback, 8080, |_| true).await.unwrap();
        assert_eq!(allowed, vec!["127.0.0.1:8080".parse::<SocketAddr>().unwrap()]);

        let public = Host::Ipv6("2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap());
        let addrs = resolve_addrs(&public, 443, is_public_ip).await.unwrap();
        assert_eq!(addrs[0].port(), 443);
        assert!(is_public_ip(addrs[0].ip()));
    }

    #[tokio::test]
    async fn resolve_addrs_refuses_names_that_resolve_to_loopback() {
        // `localhost` resolves without the network and always to loopback.
        let error = resolve_addrs(&Host::Domain("localhost"), 80, is_public_ip)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, FetchFailure::NotPublic { host } if host == "localhost"),
            "{error:?}"
        );
        let addrs = resolve_addrs(&Host::Domain("localhost"), 80, |_| true).await.unwrap();
        assert!(addrs.iter().all(|addr| addr.ip().is_loopback()));
    }
}
