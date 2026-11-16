//! Secrets a site's backend may use, such as API keys for other services.
//!
//! The owner enters them in the portal. They are stored like provider keys: in a file
//! only the daemon's user can read (`site-secrets.toml`, mode 0600), never in the
//! database, the site's repository or a log. A backend reads one by name at run time.
//! The agent only ever learns the names: every channel that carries backend output back
//! to the model is passed through [`SiteSecrets::redact`].

use std::path::PathBuf;
use std::sync::Mutex;

use sw_core::secrets::Secrets;

/// Longest secret value, in bytes.
pub const MAX_VALUE_BYTES: usize = 8 * 1024;
/// Most secrets per site.
pub const MAX_PER_SITE: usize = 50;
/// Values shorter than this are not redacted: replacing every "1" or "on" in a log
/// would destroy it and hide nothing.
const MIN_REDACT_BYTES: usize = 6;

/// The secrets file. Cheap operations; every change is written through at once.
#[derive(Debug)]
pub struct SiteSecrets {
    path: PathBuf,
    /// Serializes read-modify-write cycles on the file.
    lock: Mutex<()>,
}

/// Whether `name` can be a secret's name: `UPPER_SNAKE_CASE`, at most 64 characters.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn key(site_id: i64, name: &str) -> String {
    format!("{site_id}/{name}")
}

impl SiteSecrets {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    fn load(&self) -> Result<Secrets, String> {
        Secrets::load(&self.path).map_err(|e| e.to_string())
    }

    /// The names of a site's secrets, sorted.
    pub fn names(&self, site_id: i64) -> Result<Vec<String>, String> {
        Ok(self
            .all(site_id)?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    /// A site's secrets as `(name, value)` pairs.
    fn all(&self, site_id: i64) -> Result<Vec<(String, String)>, String> {
        let prefix = key(site_id, "");
        Ok(self
            .load()?
            .keys
            .into_iter()
            .filter_map(|(k, v)| Some((k.strip_prefix(&prefix)?.to_owned(), v)))
            .collect())
    }

    pub fn get(&self, site_id: i64, name: &str) -> Result<Option<String>, String> {
        Ok(self.load()?.keys.remove(&key(site_id, name)))
    }

    /// Store a secret, replacing one of the same name.
    pub fn set(&self, site_id: i64, name: &str, value: &str) -> Result<(), String> {
        if !valid_name(name) {
            return Err(
                "secret names are UPPER_SNAKE_CASE: capital letters, digits and underscores, \
                 starting with a letter (for example WEATHER_API_KEY)"
                    .into(),
            );
        }
        if value.is_empty() || value.len() > MAX_VALUE_BYTES {
            return Err(format!(
                "a secret's value must be between 1 and {MAX_VALUE_BYTES} bytes"
            ));
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut secrets = self.load()?;
        let prefix = key(site_id, "");
        let existing = secrets
            .keys
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .count();
        if existing >= MAX_PER_SITE && !secrets.keys.contains_key(&key(site_id, name)) {
            return Err(format!("a site can have at most {MAX_PER_SITE} secrets"));
        }
        secrets.keys.insert(key(site_id, name), value.to_owned());
        secrets.save(&self.path).map_err(|e| e.to_string())
    }

    /// Remove a secret. Returns whether it existed.
    pub fn remove(&self, site_id: i64, name: &str) -> Result<bool, String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut secrets = self.load()?;
        let existed = secrets.keys.remove(&key(site_id, name)).is_some();
        if existed {
            secrets.save(&self.path).map_err(|e| e.to_string())?;
        }
        Ok(existed)
    }

    /// Drop the secrets of sites that no longer exist. Site ids can be reused once a
    /// site is purged, and a new site must never inherit an old one's secrets.
    pub fn retain_sites(&self, existing: &[i64]) -> Result<usize, String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut secrets = self.load()?;
        let before = secrets.keys.len();
        secrets.keys.retain(|k, _| {
            k.split_once('/')
                .and_then(|(id, _)| id.parse::<i64>().ok())
                .is_some_and(|id| existing.contains(&id))
        });
        let removed = before - secrets.keys.len();
        if removed > 0 {
            secrets.save(&self.path).map_err(|e| e.to_string())?;
        }
        Ok(removed)
    }

    /// Replace every occurrence of one of the site's secret values in `text` with
    /// `[secret NAME]`. If the secrets cannot be read, nothing is let through.
    pub fn redact(&self, site_id: i64, text: &str) -> String {
        let Ok(mut all) = self.all(site_id) else {
            return "[withheld: the site's secrets could not be read to check this text]".into();
        };
        // Longest first, so a secret that contains another is replaced whole.
        all.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
        let mut out = text.to_owned();
        for (name, value) in all {
            if value.len() >= MIN_REDACT_BYTES && out.contains(&value) {
                out = out.replace(&value, &format!("[secret {name}]"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn secrets_are_per_site_write_through_and_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("site-secrets.toml");
        let secrets = SiteSecrets::new(path.clone());
        assert!(secrets.names(1).unwrap().is_empty());
        secrets.set(1, "WEATHER_API_KEY", "abc123def456").unwrap();
        secrets.set(1, "A2", "second-value").unwrap();
        secrets.set(2, "WEATHER_API_KEY", "other-site-key").unwrap();
        secrets.set(1, "A2", "replaced-value").unwrap();
        assert_eq!(secrets.names(1).unwrap(), ["A2", "WEATHER_API_KEY"]);
        assert_eq!(
            secrets.get(1, "A2").unwrap().as_deref(),
            Some("replaced-value")
        );
        assert_eq!(secrets.get(2, "A2").unwrap(), None);
        assert_eq!(
            secrets.get(12, "WEATHER_API_KEY").unwrap(),
            None,
            "1/ is not a prefix of 12/"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // A fresh handle reads the same file.
        let again = SiteSecrets::new(path);
        assert!(again.remove(1, "A2").unwrap());
        assert!(!again.remove(1, "A2").unwrap());
        assert_eq!(secrets.names(1).unwrap(), ["WEATHER_API_KEY"]);

        for bad in [
            "",
            "lower",
            "1ST",
            "WITH-DASH",
            "WITH SPACE",
            &"X".repeat(65),
        ] {
            assert!(secrets.set(1, bad, "value").is_err(), "{bad}");
        }
        assert!(secrets.set(1, "EMPTY", "").is_err());
        assert!(
            secrets
                .set(1, "BIG", &"x".repeat(MAX_VALUE_BYTES + 1))
                .is_err()
        );
    }

    #[test]
    fn redaction_hides_values_but_not_names_or_short_values() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SiteSecrets::new(dir.path().join("s.toml"));
        secrets.set(1, "TOKEN", "sk-live-0123456789").unwrap();
        secrets.set(1, "TOKEN_PREFIX", "sk-live").unwrap();
        secrets.set(1, "FLAG", "on").unwrap();
        secrets.set(2, "OTHER", "site-two-secret").unwrap();
        let text = "auth sk-live-0123456789 and sk-live alone, flag on, TOKEN, site-two-secret";
        assert_eq!(
            secrets.redact(1, text),
            "auth [secret TOKEN] and [secret TOKEN_PREFIX] alone, flag on, TOKEN, site-two-secret"
        );
        assert_eq!(secrets.redact(3, text), text);

        assert_eq!(secrets.retain_sites(&[2, 7]).unwrap(), 3);
        assert!(secrets.names(1).unwrap().is_empty());
        assert_eq!(secrets.names(2).unwrap(), ["OTHER"]);
        assert_eq!(secrets.retain_sites(&[2]).unwrap(), 0);
    }
}
