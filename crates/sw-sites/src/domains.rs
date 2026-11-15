//! Checking that a hostname really reaches this server.
//!
//! DNS alone cannot prove that: behind Cloudflare or any other proxy a name resolves to
//! the proxy's addresses. So the check has two parts: resolve the name, then fetch
//! `http://<hostname>/.well-known/sitewright/<token>` and see whether this very daemon
//! answers with the token it just made up.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::Rng;
use serde::Serialize;
use sw_core::names::normalize_hostname;

/// URL path prefix the daemon answers on every hostname.
pub const WELL_KNOWN_PREFIX: &str = "/.well-known/sitewright/";
const TOKEN_LIFETIME: Duration = Duration::from_secs(60);

/// The outcome of a domain check.
#[derive(Debug, Clone, Serialize)]
pub struct DomainCheck {
    pub hostname: String,
    /// Addresses the name resolves to. Empty when it does not resolve.
    pub addresses: Vec<IpAddr>,
    /// Whether a request to the hostname reached this server.
    pub reaches_this_server: bool,
    /// What was observed, in words, for people and for the agent.
    pub detail: String,
}

/// Issues short-lived tokens and answers for them. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct DomainChecker {
    tokens: Arc<Mutex<HashMap<String, Instant>>>,
}

impl DomainChecker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The response body for `/.well-known/sitewright/<token>`, if the token is live.
    pub fn answer(&self, token: &str) -> Option<String> {
        let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        tokens.retain(|_, issued| issued.elapsed() < TOKEN_LIFETIME);
        tokens.contains_key(token).then(|| proof(token))
    }

    fn issue(&self) -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        let token = hex::encode(bytes);
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token.clone(), Instant::now());
        token
    }

    /// Resolve `hostname` and test whether plain-HTTP requests for it arrive here.
    ///
    /// `port` is the public HTTP port (80 in production). The request is a single GET of a
    /// fixed path with a random token, without following redirects.
    pub async fn check(&self, hostname: &str, port: u16) -> DomainCheck {
        let Some(host) = normalize_hostname(hostname) else {
            return DomainCheck {
                hostname: hostname.to_owned(),
                addresses: Vec::new(),
                reaches_this_server: false,
                detail: "not a valid hostname".into(),
            };
        };
        let mut addresses: Vec<IpAddr> = match tokio::net::lookup_host((host.as_str(), port)).await
        {
            Ok(found) => found.map(|a| a.ip()).collect(),
            Err(_) => Vec::new(),
        };
        addresses.sort();
        addresses.dedup();
        if addresses.is_empty() {
            return DomainCheck {
                hostname: host,
                addresses,
                reaches_this_server: false,
                detail: "the name does not resolve: there is no DNS record for it yet (or it has not propagated)".into(),
            };
        }

        let token = self.issue();
        let url = format!("http://{host}:{port}{WELL_KNOWN_PREFIX}{token}");
        sw_core::crypto::install_default_provider();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build();
        let outcome = match client {
            Ok(client) => client.get(&url).send().await,
            Err(e) => Err(e),
        };
        let (reaches, detail) = match outcome {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if status.is_success() && body.trim() == proof(&token) {
                    (true, "requests for this name reach this server".to_owned())
                } else if status.is_redirection() {
                    (
                        false,
                        format!(
                            "the name answers with a redirect ({status}); something in front redirects HTTP before it reaches this server"
                        ),
                    )
                } else {
                    (
                        false,
                        format!(
                            "the name resolves, but another server answered ({status}); the DNS record or the proxy in front points somewhere else"
                        ),
                    )
                }
            }
            Err(e) if e.is_timeout() => (
                false,
                "the name resolves, but nothing answered on HTTP within 8 seconds".to_owned(),
            ),
            Err(_) => (
                false,
                "the name resolves, but the HTTP connection failed".to_owned(),
            ),
        };
        DomainCheck {
            hostname: host,
            addresses,
            reaches_this_server: reaches,
            detail,
        }
    }
}

fn proof(token: &str) -> String {
    format!("sitewright-domain-check {token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_issued_tokens_are_answered() {
        let checker = DomainChecker::new();
        assert_eq!(checker.answer("nope"), None);
        let token = checker.issue();
        assert_eq!(
            checker.answer(&token),
            Some(format!("sitewright-domain-check {token}"))
        );
    }

    #[tokio::test]
    async fn invalid_and_unresolvable_names_fail_with_a_reason() {
        let checker = DomainChecker::new();
        let bad = checker.check("not a host", 80).await;
        assert!(!bad.reaches_this_server);
        let missing = checker.check("does-not-exist.invalid", 80).await;
        assert!(!missing.reaches_this_server);
        assert!(missing.addresses.is_empty());
        assert!(missing.detail.contains("does not resolve"));
    }
}
