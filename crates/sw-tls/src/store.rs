//! Certificates on disk and in memory, and the resolver that picks one by hostname.
//!
//! Layout: `<data>/certs/<hostname>/{cert.pem, key.pem, meta.json}`, key files 0600.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};

use crate::TlsError;

/// What is known about a stored certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertMeta {
    pub hostname: String,
    /// Validity, Unix seconds.
    pub not_before: i64,
    pub not_after: i64,
    /// Serial number, hex. Changes with every renewal.
    pub serial: String,
    /// The ACME directory that issued it.
    pub issuer_directory: String,
}

impl CertMeta {
    /// Seconds of validity left at `now` (negative once expired).
    pub fn remaining(&self, now: i64) -> i64 {
        self.not_after - now
    }

    /// Whether it is time to renew: a third or less of the lifetime remains. Derived
    /// from the certificate itself, so nothing assumes 90-day certificates.
    pub fn due_for_renewal(&self, now: i64) -> bool {
        let lifetime = (self.not_after - self.not_before).max(1);
        self.remaining(now) * 3 <= lifetime
    }
}

struct Entry {
    key: Arc<CertifiedKey>,
    meta: CertMeta,
}

/// The certificate store. Cheap to clone.
#[derive(Clone)]
pub struct CertStore {
    dir: PathBuf,
    entries: Arc<ArcSwap<HashMap<String, Arc<Entry>>>>,
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertStore")
            .field("dir", &self.dir)
            .field("hostnames", &self.hostnames())
            .finish()
    }
}

impl CertStore {
    /// Open the store and load every certificate found on disk. Unreadable entries are
    /// skipped with a warning: they will simply be issued again.
    pub fn open(dir: &Path) -> Result<Self, TlsError> {
        std::fs::create_dir_all(dir)?;
        let mut entries = HashMap::new();
        for item in std::fs::read_dir(dir)?.filter_map(Result::ok) {
            let Ok(hostname) = item.file_name().into_string() else {
                continue;
            };
            let path = item.path();
            let loaded = (|| -> Result<Entry, TlsError> {
                let cert_pem = std::fs::read_to_string(path.join("cert.pem"))?;
                let key_pem = std::fs::read_to_string(path.join("key.pem"))?;
                let meta: CertMeta =
                    serde_json::from_str(&std::fs::read_to_string(path.join("meta.json"))?)
                        .map_err(|e| TlsError::Certificate(e.to_string()))?;
                Ok(Entry {
                    key: Arc::new(certified_key(&cert_pem, &key_pem)?),
                    meta,
                })
            })();
            match loaded {
                Ok(entry) if entry.meta.hostname == hostname => {
                    entries.insert(hostname, Arc::new(entry));
                }
                Ok(_) => {
                    tracing::warn!(%hostname, "ignoring a certificate stored under the wrong name")
                }
                Err(error) => {
                    tracing::warn!(%hostname, %error, "ignoring an unreadable certificate")
                }
            }
        }
        Ok(Self {
            dir: dir.to_owned(),
            entries: Arc::new(ArcSwap::from_pointee(entries)),
        })
    }

    /// Save a freshly issued certificate and start serving it.
    pub fn insert(
        &self,
        hostname: &str,
        cert_pem: &str,
        key_pem: &str,
        issuer_directory: &str,
    ) -> Result<CertMeta, TlsError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let key = certified_key(cert_pem, key_pem)?;
        let meta = describe(hostname, cert_pem, issuer_directory)?;
        let dir = self.dir.join(hostname);
        std::fs::create_dir_all(&dir)?;
        // The key first, with owner-only permissions from the moment it exists.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dir.join("key.pem.tmp"))?;
        file.write_all(key_pem.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(dir.join("key.pem.tmp"), dir.join("key.pem"))?;
        std::fs::write(dir.join("cert.pem"), cert_pem)?;
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta)
                .map_err(|e| TlsError::Certificate(e.to_string()))?,
        )?;

        let mut entries = HashMap::clone(&self.entries.load());
        entries.insert(
            hostname.to_owned(),
            Arc::new(Entry {
                key: Arc::new(key),
                meta: meta.clone(),
            }),
        );
        self.entries.store(Arc::new(entries));
        Ok(meta)
    }

    /// Stop serving and delete the certificate of a hostname that no longer belongs to
    /// any site.
    pub fn remove(&self, hostname: &str) {
        let mut entries = HashMap::clone(&self.entries.load());
        if entries.remove(hostname).is_some() {
            self.entries.store(Arc::new(entries));
            let _ = std::fs::remove_dir_all(self.dir.join(hostname));
        }
    }

    pub fn meta(&self, hostname: &str) -> Option<CertMeta> {
        self.entries.load().get(hostname).map(|e| e.meta.clone())
    }

    /// The leaf certificate of a hostname, DER-encoded.
    pub fn leaf(&self, hostname: &str) -> Option<CertificateDer<'static>> {
        self.entries
            .load()
            .get(hostname)
            .and_then(|e| e.key.cert.first().cloned())
    }

    pub fn hostnames(&self) -> Vec<String> {
        let mut names: Vec<String> = self.entries.load().keys().cloned().collect();
        names.sort();
        names
    }
}

impl ResolvesServerCert for CertStore {
    /// Pick the certificate for the requested hostname. No name, or a name without a
    /// certificate, gets none, and the handshake fails: nothing is ever issued on demand.
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        self.entries.load().get(&name).map(|e| e.key.clone())
    }
}

fn certified_key(cert_pem: &str, key_pem: &str) -> Result<CertifiedKey, TlsError> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| TlsError::Certificate(format!("unreadable certificate chain: {e}")))?;
    if chain.is_empty() {
        return Err(TlsError::Certificate(
            "the certificate chain is empty".into(),
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| TlsError::Certificate(format!("unreadable private key: {e}")))?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| TlsError::Certificate(format!("unsupported private key: {e}")))?;
    Ok(CertifiedKey::new(chain, signer))
}

/// Read validity and serial from the leaf certificate, and check it covers `hostname`.
fn describe(hostname: &str, cert_pem: &str, issuer_directory: &str) -> Result<CertMeta, TlsError> {
    let leaf = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .next()
        .and_then(Result::ok)
        .ok_or_else(|| TlsError::Certificate("the certificate chain is empty".into()))?;
    let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|e| TlsError::Certificate(format!("unreadable certificate: {e}")))?;
    let covers = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .is_some_and(|san| san.value.general_names.iter().any(|n| matches!(n, x509_parser::extensions::GeneralName::DNSName(d) if d.eq_ignore_ascii_case(hostname))));
    if !covers {
        return Err(TlsError::Certificate(format!(
            "the certificate does not cover {hostname}"
        )));
    }
    Ok(CertMeta {
        hostname: hostname.to_owned(),
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        serial: cert.raw_serial_as_string().replace(':', ""),
        issuer_directory: issuer_directory.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed(name: &str) -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
        (cert.cert.pem(), cert.signing_key.serialize_pem())
    }

    #[test]
    fn certificates_persist_with_private_keys_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = CertStore::open(dir.path()).unwrap();
        assert!(store.hostnames().is_empty());

        let (cert, key) = self_signed("hikes.example.com");
        let meta = store
            .insert("hikes.example.com", &cert, &key, "https://acme.test/dir")
            .unwrap();
        assert!(meta.not_after > meta.not_before && !meta.serial.is_empty());
        let mode = std::fs::metadata(dir.path().join("hikes.example.com/key.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        // A certificate for another name is refused.
        assert!(store.insert("other.example.com", &cert, &key, "x").is_err());
        assert!(
            store
                .insert("hikes.example.com", "not pem", &key, "x")
                .is_err()
        );

        // A restart finds it again; a corrupt entry is skipped, not fatal.
        std::fs::create_dir_all(dir.path().join("broken.example.com")).unwrap();
        std::fs::write(dir.path().join("broken.example.com/cert.pem"), "junk").unwrap();
        let reopened = CertStore::open(dir.path()).unwrap();
        assert_eq!(reopened.hostnames(), ["hikes.example.com"]);
        assert_eq!(reopened.meta("hikes.example.com"), Some(meta));

        reopened.remove("hikes.example.com");
        assert!(reopened.hostnames().is_empty());
        assert!(!dir.path().join("hikes.example.com").exists());
    }

    #[test]
    fn renewal_is_due_when_a_third_of_the_lifetime_remains() {
        let meta = |not_before, not_after| CertMeta {
            hostname: "h".into(),
            not_before,
            not_after,
            serial: "1".into(),
            issuer_directory: "d".into(),
        };
        let ninety_days = meta(0, 90 * 86_400);
        assert!(!ninety_days.due_for_renewal(59 * 86_400));
        assert!(ninety_days.due_for_renewal(60 * 86_400));
        assert!(ninety_days.due_for_renewal(100 * 86_400), "expired");
        // The rule scales: a 6-day certificate renews with 2 days left, not 30.
        let six_days = meta(0, 6 * 86_400);
        assert!(!six_days.due_for_renewal(3 * 86_400));
        assert!(six_days.due_for_renewal(4 * 86_400));
    }
}
