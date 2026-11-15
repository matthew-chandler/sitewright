//! Working out the real client address behind trusted proxies.

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;
use sw_core::config::ServerConfig;

/// The address of the client that made the request.
///
/// Forwarding headers are believed only when the TCP peer is a trusted proxy. Then
/// `CF-Connecting-IP` wins if present; otherwise `X-Forwarded-For` is read from the right,
/// skipping trusted proxies, so a client cannot spoof its address by sending its own
/// `X-Forwarded-For`.
pub fn client_ip(config: &ServerConfig, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let peer_ip = peer.ip();
    if !config.is_trusted_proxy(peer_ip) {
        return peer_ip;
    }
    if let Some(ip) = header_str(headers, "cf-connecting-ip").and_then(|v| v.trim().parse().ok()) {
        return ip;
    }
    let forwarded = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|part| part.trim().parse::<IpAddr>().ok())
        .collect::<Vec<_>>();
    forwarded
        .iter()
        .rev()
        .find(|ip| !config.is_trusted_proxy(**ip))
        .or(forwarded.first())
        .copied()
        .unwrap_or(peer_ip)
}

/// Whether the visitor's connection was HTTPS, as reported by a trusted proxy.
pub fn forwarded_https(config: &ServerConfig, peer: SocketAddr, headers: &HeaderMap) -> bool {
    config.is_trusted_proxy(peer.ip())
        && header_str(headers, "x-forwarded-proto")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"))
}

pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(proxies: &[&str]) -> ServerConfig {
        ServerConfig {
            trusted_proxies: proxies.iter().map(|p| p.parse().unwrap()).collect(),
            ..ServerConfig::default()
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(*k, v.parse().unwrap());
        }
        map
    }

    #[test]
    fn untrusted_peers_cannot_spoof() {
        let cfg = config(&[]);
        let peer: SocketAddr = "198.51.100.9:1234".parse().unwrap();
        let h = headers(&[
            ("x-forwarded-for", "1.2.3.4"),
            ("cf-connecting-ip", "5.6.7.8"),
        ]);
        assert_eq!(client_ip(&cfg, peer, &h), peer.ip());
        assert!(!forwarded_https(
            &cfg,
            peer,
            &headers(&[("x-forwarded-proto", "https")])
        ));
    }

    #[test]
    fn trusted_proxy_headers_are_used() {
        let cfg = config(&["127.0.0.1/32", "10.0.0.0/8"]);
        let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        // Rightmost untrusted address wins; the client-supplied leftmost one is ignored.
        let h = headers(&[("x-forwarded-for", "6.6.6.6, 203.0.113.5, 10.0.0.2")]);
        assert_eq!(
            client_ip(&cfg, peer, &h),
            "203.0.113.5".parse::<IpAddr>().unwrap()
        );
        let h = headers(&[
            ("x-forwarded-for", "203.0.113.5"),
            ("cf-connecting-ip", "2001:db8::1"),
        ]);
        assert_eq!(
            client_ip(&cfg, peer, &h),
            "2001:db8::1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(client_ip(&cfg, peer, &HeaderMap::new()), peer.ip());
        assert_eq!(
            client_ip(&cfg, peer, &headers(&[("x-forwarded-for", "garbage")])),
            peer.ip()
        );
        assert!(forwarded_https(
            &cfg,
            peer,
            &headers(&[("x-forwarded-proto", "HTTPS")])
        ));
    }
}
