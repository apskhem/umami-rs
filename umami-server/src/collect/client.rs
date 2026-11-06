//! Client IP handling: ports of upstream's `src/lib/ip.ts` and the IP parts of
//! `src/lib/detect.ts`.

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;

use crate::config::IgnoredIp;

/// Headers checked for the client IP, in upstream's order.
const IP_HEADERS: &[&str] = &[
    "true-client-ip",
    "cf-connecting-ip",
    "fastly-client-ip",
    "x-nf-client-connection-ip",
    "do-connecting-ip",
    "x-real-ip",
    "x-appengine-user-ip",
    "x-forwarded-for",
    "forwarded",
    "x-client-ip",
    "x-cluster-client-ip",
    "x-forwarded",
];

/// A header value as Node.js reads it: each byte becomes one character (Latin-1).
/// Upstream hashes these strings, so non-ASCII bytes must decode the same way.
pub fn header_string(
    headers: &HeaderMap,
    name: impl axum::http::header::AsHeaderName,
) -> Option<String> {
    headers.get(name).map(|value| {
        value
            .as_bytes()
            .iter()
            .map(|&byte| char::from(byte))
            .collect()
    })
}

/// Upstream's `getIpAddress(headers)`.
///
/// Upstream runs behind Next.js, which fills in `x-forwarded-for` from the socket
/// when a request arrives without one; `peer` plays that role here.
pub fn client_ip(
    headers: &HeaderMap,
    custom_header: Option<&str>,
    peer: Option<SocketAddr>,
) -> Option<String> {
    let header = |name: &str| header_string(headers, name).filter(|value| !value.is_empty());

    if let Some(name) = custom_header
        && let Some(value) = header(name)
    {
        return parse_header_value(name, &value);
    }

    for name in IP_HEADERS {
        if let Some(value) = header(name) {
            return parse_header_value(name, &value);
        }

        if *name == "x-forwarded-for"
            && let Some(peer) = peer
        {
            return Some(normalize(peer.ip()).to_string());
        }
    }

    None
}

fn parse_header_value(name: &str, value: &str) -> Option<String> {
    match name {
        "x-forwarded-for" => value.split(',').next().map(|first| resolve(first.trim())),
        "forwarded" => forwarded_for(value).map(resolve),
        _ => Some(resolve(value)),
    }
}

/// The first match of upstream's `/for=(\[?[0-9a-fA-F:.]+]?)/` (which never matches
/// quoted values such as `for="[::1]"`).
fn forwarded_for(value: &str) -> Option<&str> {
    value.match_indices("for=").find_map(|(at, _)| {
        let rest = &value[at + 4..];
        let open = usize::from(rest.starts_with('['));
        let len = rest[open..]
            .find(|c: char| !(c.is_ascii_hexdigit() || c == ':' || c == '.'))
            .unwrap_or(rest.len() - open);

        if len == 0 {
            return None;
        }

        let close = usize::from(rest[open + len..].starts_with(']'));

        Some(&rest[..open + len + close])
    })
}

/// Upstream's `resolveIp`: normalize a valid address, else retry without a port,
/// else return the input unchanged.
fn resolve(ip: &str) -> String {
    if let Ok(addr) = ip.parse::<IpAddr>() {
        return normalize(addr).to_string();
    }

    let stripped = strip_port(ip);
    if stripped != ip {
        return stripped
            .parse::<IpAddr>()
            .map_or_else(|_| stripped.to_owned(), |addr| normalize(addr).to_string());
    }

    ip.to_owned()
}

/// IPv4-mapped IPv6 addresses become plain IPv4.
fn normalize(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        v4 => v4,
    }
}

/// Upstream's `stripPort`.
pub fn strip_port(ip: &str) -> &str {
    if ip.starts_with('[')
        && let Some(end) = ip.find(']')
    {
        return &ip[..=end];
    }

    if let Some(colon) = ip.rfind(':') {
        let host = &ip[..colon];
        if ip.contains('.')
            || host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return host;
        }
    }

    ip
}

/// Upstream's `hasBlockedIp`.
pub fn is_ignored(ip: Option<&str>, ignored: &[IgnoredIp]) -> bool {
    let Some(ip) = ip.filter(|ip| !ip.is_empty()) else {
        return false;
    };
    let addr = ip.parse::<IpAddr>().ok();

    ignored.iter().any(|entry| {
        entry.entry == ip
            || matches!((addr, entry.range), (Some(addr), Some(range)) if range.contains(&addr))
    })
}

/// Whether geolocation should be skipped: upstream ignores invalid addresses and
/// those `is-localhost-ip` considers local (private, loopback, link-local and
/// unique-local ranges, plus addresses the machine could bind to).
pub fn is_local(addr: IpAddr) -> bool {
    match normalize(addr) {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_unspecified()
                || (a == 169 && b == 254 && (1..=254).contains(&c))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| (name.parse().unwrap(), HeaderValue::from_str(value).unwrap()))
            .collect()
    }

    fn ip(pairs: &[(&'static str, &str)]) -> Option<String> {
        client_ip(&headers(pairs), None, None)
    }

    #[test]
    fn reads_headers_in_upstream_order() {
        assert_eq!(ip(&[]), None);
        assert_eq!(
            ip(&[
                ("x-forwarded-for", "1.1.1.1"),
                ("cf-connecting-ip", "2.2.2.2")
            ]),
            Some("2.2.2.2".into())
        );
        assert_eq!(
            ip(&[("x-forwarded-for", " 203.0.113.7 , 10.0.0.1")]),
            Some("203.0.113.7".into())
        );
        assert_eq!(
            ip(&[("forwarded", "for=[2001:db8::1]:4711;proto=https")]),
            Some("[2001:db8::1]".into())
        );
        assert_eq!(ip(&[("forwarded", "for=\"[2001:db8::1]:4711\"")]), None);
        assert_eq!(
            ip(&[("forwarded", "for=unknown, for=198.51.100.2")]),
            Some("198.51.100.2".into())
        );
        assert_eq!(
            ip(&[("forwarded", "for=192.0.2.60;proto=http")]),
            Some("192.0.2.60".into())
        );
    }

    #[test]
    fn normalizes_addresses() {
        assert_eq!(
            ip(&[("x-real-ip", "::ffff:203.0.113.7")]),
            Some("203.0.113.7".into())
        );
        assert_eq!(
            ip(&[("x-real-ip", "203.0.113.7:8080")]),
            Some("203.0.113.7".into())
        );
        assert_eq!(
            ip(&[("x-real-ip", "2001:DB8:0:0::1")]),
            Some("2001:db8::1".into())
        );
        assert_eq!(ip(&[("x-real-ip", "not-an-ip")]), Some("not-an-ip".into()));
    }

    #[test]
    fn custom_header_wins() {
        let headers = headers(&[("x-client", "198.51.100.1"), ("x-real-ip", "203.0.113.7")]);

        assert_eq!(
            client_ip(&headers, Some("x-client"), None),
            Some("198.51.100.1".into())
        );
        assert_eq!(
            client_ip(&headers, Some("x-missing"), None),
            Some("203.0.113.7".into())
        );
    }

    #[test]
    fn peer_address_stands_in_for_x_forwarded_for() {
        let peer = Some("[::ffff:198.51.100.9]:5000".parse().unwrap());

        assert_eq!(
            client_ip(&HeaderMap::new(), None, peer),
            Some("198.51.100.9".into())
        );
        assert_eq!(
            client_ip(&headers(&[("x-real-ip", "203.0.113.7")]), None, peer),
            Some("203.0.113.7".into())
        );
        // Headers after x-forwarded-for lose to it, as they do behind Next.js.
        assert_eq!(
            client_ip(&headers(&[("x-client-ip", "203.0.113.7")]), None, peer),
            Some("198.51.100.9".into())
        );
    }

    #[test]
    fn decodes_headers_as_latin1_like_node() {
        let headers: HeaderMap = [(
            "user-agent".parse().unwrap(),
            HeaderValue::from_bytes("Ù".as_bytes()).unwrap(),
        )]
        .into_iter()
        .collect();

        // UTF-8 bytes C3 99 read as two Latin-1 characters.
        assert_eq!(
            header_string(&headers, "user-agent").as_deref(),
            Some("\u{c3}\u{99}")
        );
    }

    #[test]
    fn strips_ports() {
        assert_eq!(strip_port("1.2.3.4:80"), "1.2.3.4");
        assert_eq!(strip_port("[::1]:80"), "[::1]");
        assert_eq!(strip_port("::1"), "::1");
        assert_eq!(strip_port("localhost:3000"), "localhost");
    }

    #[test]
    fn ignored_ips() {
        let ignored = [
            IgnoredIp {
                entry: "203.0.113.7".into(),
                range: None,
            },
            IgnoredIp {
                entry: "10.0.0.0/8".into(),
                range: Some("10.0.0.0/8".parse().unwrap()),
            },
        ];

        assert!(is_ignored(Some("203.0.113.7"), &ignored));
        assert!(is_ignored(Some("10.1.2.3"), &ignored));
        assert!(!is_ignored(Some("11.0.0.1"), &ignored));
        assert!(!is_ignored(Some("::ffff:10.0.0.1"), &ignored));
        assert!(!is_ignored(None, &ignored));
    }

    #[test]
    fn local_addresses() {
        for local in [
            "127.0.0.1",
            "10.1.1.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "0.0.0.0",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(is_local(local.parse().unwrap()), "{local}");
        }
        for public in ["8.8.8.8", "169.254.0.1", "2001:db8::1", "172.32.0.1"] {
            assert!(!is_local(public.parse().unwrap()), "{public}");
        }
    }
}
