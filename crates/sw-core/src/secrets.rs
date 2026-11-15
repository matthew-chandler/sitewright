//! API keys and other secrets, kept apart from the main configuration.
//!
//! `secrets.toml` lives in the data directory with mode 0600. Its values are never
//! logged, never shown in the UI and never sent back to a browser.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The contents of `secrets.toml`.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Secrets {
    /// API keys by name; providers refer to them with `key_ref`.
    pub keys: BTreeMap<String, String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names only: values must never reach a log.
        f.debug_struct("Secrets")
            .field("keys", &self.keys.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Secrets {
    /// Load `path`. A missing file is an empty set of secrets. A file readable by group
    /// or others is refused, because that almost always means a leaked key.
    pub fn load(path: &Path) -> Result<Self> {
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(Error::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(Error::Config(format!(
                "{} is readable by other users (mode {mode:o}); run: chmod 600 {}",
                path.display(),
                path.display()
            )));
        }
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        // A parse error may quote the offending line, so report only its position.
        toml::from_str(&text).map_err(|e| {
            let at = e
                .span()
                .map(|s| format!(" at byte {}", s.start))
                .unwrap_or_default();
            Error::Config(format!("{} is not valid{at}", path.display()))
        })
    }

    /// Write the secrets to `path` with mode 0600.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let text = toml::to_string(self).map_err(|e| Error::Config(e.to_string()))?;
        let tmp = path.with_extension("toml.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn key(&self, name: &str) -> Option<&str> {
        self.keys.get(name).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        assert!(Secrets::load(&path).unwrap().keys.is_empty());

        let mut secrets = Secrets::default();
        secrets.keys.insert("azure".into(), "sk-very-secret".into());
        secrets.save(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            Secrets::load(&path).unwrap().key("azure"),
            Some("sk-very-secret")
        );
        assert!(!format!("{secrets:?}").contains("sk-very-secret"));
    }

    #[test]
    fn refuses_world_readable_files_and_hides_values_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "[keys]\nazure = \"sk-very-secret\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Secrets::load(&path).is_err());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, "[keys]\nazure = sk-very-secret-unquoted\n").unwrap();
        let err = Secrets::load(&path).unwrap_err().to_string();
        assert!(!err.contains("sk-very-secret"), "{err}");
    }
}
