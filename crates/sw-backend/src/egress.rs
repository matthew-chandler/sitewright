//! Outbound HTTP requests on behalf of a site's backend.
//!
//! A backend cannot open a socket. `fetch` in a backend is a message to the daemon,
//! which makes the request itself, under these rules:
//!
//! - **Approved hosts only.** Each site has its own list; the owner approves entries.
//! - **HTTPS on port 443 only**, to a host name, never an IP address.
//! - **Public addresses only.** The name is resolved here and the connection is pinned
//!   to the address that was checked, so a name cannot point at the loopback interface,
//!   a private network or a cloud metadata service (server-side request forgery), and
//!   cannot change between the check and the connection (DNS rebinding).
//! - **Redirects are not followed.** The backend gets the 3xx and decides; following it
//!   means another `fetch`, which is checked like the first.
//! - **Limits**: a time limit and a size limit, text bodies only.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Largest response body handed to a backend.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Largest request body a backend may send.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Longest one outbound request may take, connection included.
pub const TIMEOUT: Duration = Duration::from_secs(8);

/// A request as the backend describes it.
#[derive(Debug, Clone, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<String>,
}

fn default_method() -> String {
    "GET".into()
}

/// The response handed back to the backend.
#[derive(Debug, Clone, Serialize)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Normalize a host name for the allow-list: lowercase, no trailing dot. `None` for
/// anything that is not a plain DNS name (IP addresses, ports, paths, wildcards).
pub fn normalize_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let plain = !host.is_empty()
        && host.len() <= 253
        && host.contains('.')
        && host.parse::<IpAddr>().is_err()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        // A name whose last label is a number is an IP address in disguise.
        && host
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.bytes().any(|b| b.is_ascii_alphabetic()));
    plain.then_some(host)
}

/// Whether an address is on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1])) // carrier-grade NAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // protocol assignments
                || (o[0] == 198 && (18..20).contains(&o[1])) // benchmarking
                || o[0] >= 240) // reserved
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64
                || s[0] == 0) // v4-compatible and other reserved ranges
        }
    }
}

/// What a URL must look like, and the host it names.
pub fn parse_url(url: &str) -> Result<(reqwest::Url, String), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| format!("{url:?} is not a valid URL"))?;
    if parsed.scheme() != "https" {
        return Err("only https:// URLs can be requested".into());
    }
    if parsed.port().is_some_and(|p| p != 443) {
        return Err("only the standard HTTPS port can be used".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("put credentials in a header, not in the URL".into());
    }
    let host = parsed
        .host_str()
        .and_then(normalize_host)
        .ok_or("the URL must name a host, not an IP address")?;
    Ok((parsed, host))
}

/// Makes the requests. `allow_local` exists for tests, which have only loopback servers.
#[derive(Debug, Clone, Default)]
pub struct Egress {
    allow_local: bool,
}

/// Request headers a backend may not set: they belong to the connection.
fn forbidden_request_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "connection"
            | "transfer-encoding"
            | "upgrade"
            | "te"
            | "expect"
    ) || name.starts_with("proxy-")
}

impl Egress {
    pub fn new() -> Self {
        Self::default()
    }

    /// For tests: plain HTTP to any port and to local addresses.
    #[doc(hidden)]
    pub fn for_tests() -> Self {
        Self { allow_local: true }
    }

    /// The host a request would go to, after checking the URL's shape.
    pub fn target_host(&self, url: &str) -> Result<String, String> {
        if self.allow_local {
            let parsed =
                reqwest::Url::parse(url).map_err(|_| format!("{url:?} is not a valid URL"))?;
            return parsed
                .host_str()
                .map(str::to_ascii_lowercase)
                .ok_or_else(|| "the URL must name a host".into());
        }
        parse_url(url).map(|(_, host)| host)
    }

    /// Make a request to a host the caller has already found on the site's allow-list.
    pub async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, String> {
        let method = reqwest::Method::from_bytes(request.method.to_ascii_uppercase().as_bytes())
            .ok()
            .filter(|m| ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"].contains(&m.as_str()))
            .ok_or("unsupported HTTP method")?;
        if request
            .body
            .as_ref()
            .is_some_and(|b| b.len() > MAX_REQUEST_BYTES)
        {
            return Err("the request body is too large".into());
        }

        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(4))
            .no_proxy()
            .user_agent(concat!("sitewright-backend/", env!("CARGO_PKG_VERSION")));
        let url = if self.allow_local {
            reqwest::Url::parse(&request.url).map_err(|_| "not a valid URL".to_owned())?
        } else {
            let (url, host) = parse_url(&request.url)?;
            // Resolve here, keep only public addresses, and pin the connection to one.
            let resolved: Vec<SocketAddr> = tokio::time::timeout(
                Duration::from_secs(4),
                tokio::net::lookup_host((host.as_str(), 443)),
            )
            .await
            .map_err(|_| format!("looking up {host} took too long"))?
            .map_err(|_| format!("{host} could not be found"))?
            .collect();
            let address = resolved
                .into_iter()
                .find(|a| is_public(a.ip()))
                .ok_or_else(|| format!("{host} does not resolve to a public address"))?;
            builder = builder.resolve(&host, address).https_only(true);
            url
        };
        let client = builder
            .build()
            .map_err(|_| "could not set up the request".to_owned())?;

        let mut outgoing = client.request(method, url);
        for (name, value) in &request.headers {
            let name = name.to_ascii_lowercase();
            if forbidden_request_header(&name) {
                continue;
            }
            outgoing = outgoing.header(name, value);
        }
        if let Some(body) = request.body {
            outgoing = outgoing.body(body);
        }
        // Errors are described without the URL: it may carry a key in its query string.
        let mut response = outgoing.send().await.map_err(|e| {
            if e.is_timeout() {
                "the request timed out".to_owned()
            } else if e.is_connect() {
                "could not connect to the host".to_owned()
            } else {
                "the request failed".to_owned()
            }
        })?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect();
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                        return Err(format!(
                            "the response is larger than {} KB",
                            MAX_RESPONSE_BYTES / 1024
                        ));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) if e.is_timeout() => return Err("the request timed out".into()),
                Err(_) => return Err("the response could not be read".into()),
            }
        }
        let body =
            String::from_utf8(bytes).map_err(|_| "the response is not UTF-8 text".to_owned())?;
        Ok(FetchResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_plain_public_https_urls_pass() {
        assert_eq!(
            parse_url("https://API.Example.com./v1/x?q=1").unwrap().1,
            "api.example.com"
        );
        assert!(parse_url("https://api.example.com:443/").is_ok());
        for bad in [
            "http://api.example.com/",
            "https://api.example.com:8443/",
            "https://user:pw@api.example.com/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://2130706433/",
            "https://0x7f.0.0.1/",
            "https://localhost/",
            "https://169.254.169.254/latest/meta-data/",
            "ftp://example.com/",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
        assert_eq!(normalize_host("Example.COM."), Some("example.com".into()));
        for bad in [
            "",
            "localhost",
            "*.example.com",
            "a b.com",
            "example.com:443",
            "10.0.0.1",
            "1.2.3.4.5",
            "-a.com",
            "exa_mple.com",
            "example.com/path",
        ] {
            assert_eq!(normalize_host(bad), None, "{bad}");
        }
    }

    #[test]
    fn private_and_special_addresses_are_not_public() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "240.0.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
            "64:ff9b::7f00:1",
            "ff02::1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }

    #[tokio::test]
    async fn names_that_resolve_to_private_addresses_are_refused() {
        // "localhost.localdomain"-style names vary by system; nip-style names need the
        // network. What can be checked everywhere: the shape rules stop IP literals, and
        // a resolvable local name is refused by the address check.
        let egress = Egress::new();
        let err = egress
            .fetch(FetchRequest {
                url: "https://localhost.localdomain/".into(),
                method: "GET".into(),
                headers: vec![],
                body: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.contains("public address")
                || err.contains("could not be found")
                || err.contains("too long"),
            "{err}"
        );
    }
}
