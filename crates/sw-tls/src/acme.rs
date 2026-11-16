//! The ACME client: one account, HTTP-01 challenges, one certificate per hostname.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, CertificateIdentifier, ChallengeType,
    Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use rustls::pki_types::CertificateDer;

use crate::TlsError;

/// Path prefix of HTTP-01 challenges.
pub const CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

/// The challenge responses currently being served. Cheap to clone; shared between the
/// ACME client (which adds them) and the HTTP listener (which answers them).
#[derive(Debug, Clone, Default)]
pub struct Challenges {
    tokens: Arc<Mutex<HashMap<String, String>>>,
}

impl Challenges {
    /// The body to answer `/.well-known/acme-challenge/<token>` with, if we are expecting it.
    pub fn answer(&self, token: &str) -> Option<String> {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(token)
            .cloned()
    }

    fn set(&self, token: &str, key_authorization: &str) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token.to_owned(), key_authorization.to_owned());
    }

    fn clear(&self, token: &str) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token);
    }
}

/// An ACME account on one directory.
pub struct Acme {
    account: Account,
    directory: String,
    challenges: Challenges,
}

impl std::fmt::Debug for Acme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acme")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

/// What is stored in `<data>/acme/account.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredAccount {
    directory: String,
    credentials: AccountCredentials,
}

fn err(e: impl std::fmt::Display) -> TlsError {
    TlsError::Acme(e.to_string())
}

impl Acme {
    /// Load the account stored in `account_file`, or register a new one with the
    /// directory and store it. An account made for another directory is not reused.
    pub async fn open(
        account_file: &Path,
        directory: &str,
        contact: Option<&str>,
        root_cert: Option<&PathBuf>,
        challenges: Challenges,
    ) -> Result<Self, TlsError> {
        sw_core::crypto::install_default_provider();
        let builder = || match root_cert {
            Some(path) => Account::builder_with_root(path).map_err(err),
            None => Account::builder().map_err(err),
        };
        let stored: Option<StoredAccount> = std::fs::read_to_string(account_file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        if let Some(stored) = stored
            && stored.directory == directory
        {
            let account = builder()?
                .from_credentials(stored.credentials)
                .await
                .map_err(err)?;
            return Ok(Self {
                account,
                directory: directory.to_owned(),
                challenges,
            });
        }

        let contact_uri = contact.map(|c| {
            if c.contains(':') {
                c.to_owned()
            } else {
                format!("mailto:{c}")
            }
        });
        let contacts: Vec<&str> = contact_uri.iter().map(String::as_str).collect();
        let new_account = NewAccount {
            contact: &contacts,
            terms_of_service_agreed: true,
            only_return_existing: false,
        };
        let (account, credentials) = builder()?
            .create(&new_account, directory.to_owned(), None)
            .await
            .map_err(err)?;

        if let Some(parent) = account_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let stored = StoredAccount {
            directory: directory.to_owned(),
            credentials,
        };
        write_private(
            account_file,
            &serde_json::to_string_pretty(&stored).map_err(err)?,
        )?;
        tracing::info!(%directory, "registered an ACME account");
        Ok(Self {
            account,
            directory: directory.to_owned(),
            challenges,
        })
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    /// Get a certificate for one hostname with an HTTP-01 challenge. Returns the
    /// certificate chain and the private key, both PEM.
    pub async fn issue(&self, hostname: &str) -> Result<(String, String), TlsError> {
        let identifiers = [Identifier::Dns(hostname.to_owned())];
        let mut order = self
            .account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(err)?;

        let mut tokens = Vec::new();
        let outcome: Result<(String, String), TlsError> = async {
            let mut authorizations = order.authorizations();
            while let Some(result) = authorizations.next().await {
                let mut authorization = result.map_err(err)?;
                match authorization.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    other => return Err(TlsError::Acme(format!("the authorization for {hostname} is {other:?}"))),
                }
                let mut challenge = authorization
                    .challenge(ChallengeType::Http01)
                    .ok_or_else(|| TlsError::Acme("the certificate authority did not offer an HTTP-01 challenge".into()))?;
                self.challenges.set(&challenge.token, challenge.key_authorization().as_str());
                tokens.push(challenge.token.clone());
                challenge.set_ready().await.map_err(err)?;
            }

            let retries = RetryPolicy::new().initial_delay(Duration::from_millis(250)).timeout(Duration::from_secs(90));
            let status = order.poll_ready(&retries).await.map_err(err)?;
            if status != OrderStatus::Ready {
                return Err(TlsError::Acme(format!(
                    "validation of {hostname} failed (order is {status:?}): the certificate authority could not fetch the challenge over HTTP on port 80"
                )));
            }
            let key_pem = order.finalize().await.map_err(err)?;
            let chain_pem = order.poll_certificate(&retries).await.map_err(err)?;
            Ok((chain_pem, key_pem))
        }
        .await;
        for token in tokens {
            self.challenges.clear(&token);
        }
        outcome
    }

    /// Whether the certificate authority suggests renewing this certificate now (ACME
    /// Renewal Information, RFC 9773). `None` when the server does not support ARI or
    /// cannot be asked: the caller then falls back to the lifetime rule.
    pub async fn renewal_suggested(&self, leaf: &CertificateDer<'_>) -> Option<bool> {
        let id = CertificateIdentifier::try_from(leaf).ok()?;
        let (info, _retry_after) = self.account.renewal_info(&id).await.ok()?;
        Some(info.suggested_window.start <= time::OffsetDateTime::now_utc())
    }
}

/// Write a file that only its owner can read.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}
