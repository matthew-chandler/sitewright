//! Validated site names and hostnames.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Names that can never be used for a site, whatever the configured admin host is.
pub const RESERVED_NAMES: &[&str] = &["admin", "www", "api", "mail"];

/// A site name: a single DNS label, `^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SiteName(String);

impl SiteName {
    /// Parse and validate a site name. Does not check reserved names; see
    /// [`SiteName::is_reserved`], which needs the configured admin host.
    pub fn parse(name: &str) -> Result<Self> {
        if is_dns_label(name) {
            Ok(Self(name.to_owned()))
        } else {
            Err(Error::InvalidName(format!(
                "invalid site name {name:?}: use 1 to 63 lowercase letters, digits or hyphens, \
                 starting and ending with a letter or digit"
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this name is reserved: one of [`RESERVED_NAMES`], or the first label of the
    /// admin hostname.
    pub fn is_reserved(&self, admin_host: &str) -> bool {
        RESERVED_NAMES.contains(&self.0.as_str())
            || admin_host.split('.').next() == Some(self.0.as_str())
    }
}

impl fmt::Display for SiteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SiteName {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<SiteName> for String {
    fn from(value: SiteName) -> Self {
        value.0
    }
}

fn is_dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    let alnum = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty()
        && b.len() <= 63
        && b.first().is_some_and(alnum)
        && b.last().is_some_and(alnum)
        && b.iter().all(|c| alnum(c) || *c == b'-')
}

/// Normalize a hostname from a `Host` header or user input: lowercase, no port, no
/// trailing dot. Returns `None` if the result is not a plausible hostname.
pub fn normalize_hostname(raw: &str) -> Option<String> {
    let raw = raw.trim();
    // Bracketed IPv6 literals are never site hostnames.
    if raw.starts_with('[') {
        return None;
    }
    let host = match raw.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) => h,
        _ => raw,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let valid = !host.is_empty() && host.len() <= 253 && host.split('.').all(is_dns_label);
    valid.then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for name in ["a", "hikes", "my-site", "a1", "1a", &"a".repeat(63)] {
            assert!(SiteName::parse(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "",
            "-a",
            "a-",
            "A",
            "a_b",
            "a.b",
            "a b",
            "é",
            "../x",
            &"a".repeat(64),
        ] {
            assert!(SiteName::parse(name).is_err(), "{name}");
        }
    }

    #[test]
    fn reserved_names_include_admin_label() {
        let admin = "sitewright-dev.example.com";
        assert!(SiteName::parse("admin").unwrap().is_reserved(admin));
        assert!(SiteName::parse("www").unwrap().is_reserved(admin));
        assert!(
            SiteName::parse("sitewright-dev")
                .unwrap()
                .is_reserved(admin)
        );
        assert!(!SiteName::parse("hikes").unwrap().is_reserved(admin));
    }

    #[test]
    fn normalizes_hostnames() {
        assert_eq!(
            normalize_hostname("Hikes.Example.COM:8080").as_deref(),
            Some("hikes.example.com")
        );
        assert_eq!(
            normalize_hostname("example.com.").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            normalize_hostname("localhost").as_deref(),
            Some("localhost")
        );
        assert_eq!(normalize_hostname("[::1]:8080"), None);
        assert_eq!(normalize_hostname("bad host"), None);
        assert_eq!(normalize_hostname(""), None);
        assert_eq!(normalize_hostname("a..b"), None);
    }
}
