//! Keeps the certificate store in step with the hostnames the daemon serves: issues
//! what is missing, renews what is ageing, removes what is no longer wanted.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::acme::Acme;
use crate::store::{CertMeta, CertStore};

/// Where the set of hostnames that should have certificates comes from.
pub type Hostnames = Arc<dyn Fn() -> futures_boxed::BoxFuture<Vec<String>> + Send + Sync>;

/// A check run before asking for a certificate: does this hostname reach us over HTTP?
/// Returns the reason when it does not.
pub type Precheck =
    Arc<dyn Fn(String) -> futures_boxed::BoxFuture<Result<(), String>> + Send + Sync>;

/// Minimal boxed-future alias, to keep this crate free of extra dependencies.
pub mod futures_boxed {
    pub type BoxFuture<T> = std::pin::Pin<Box<dyn Future<Output = T> + Send>>;
}

#[derive(Clone)]
pub struct ManagerOptions {
    /// How often to look for hostnames that need a certificate.
    pub scan_interval: Duration,
    /// How often to ask the certificate authority whether to renew early (ARI).
    pub ari_interval: Duration,
    pub precheck: Option<Precheck>,
}

impl std::fmt::Debug for ManagerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerOptions")
            .field("scan_interval", &self.scan_interval)
            .field("ari_interval", &self.ari_interval)
            .field("precheck", &self.precheck.is_some())
            .finish()
    }
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            scan_interval: Duration::from_secs(30),
            ari_interval: Duration::from_secs(12 * 3600),
            precheck: None,
        }
    }
}

/// The certificate state of one hostname, for the settings page and `doctor`.
#[derive(Debug, Clone, Serialize)]
pub struct HostStatus {
    pub hostname: String,
    pub certificate: Option<CertMeta>,
    /// Why the last attempt failed, if it did.
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Attempt {
    failures: u32,
    next_try: Option<Instant>,
    last_error: Option<String>,
}

/// Issues and renews certificates. Cheap to clone.
#[derive(Clone)]
pub struct Manager {
    acme: Arc<Acme>,
    store: CertStore,
    options: ManagerOptions,
    attempts: Arc<Mutex<HashMap<String, Attempt>>>,
    /// Hostnames the certificate authority has asked us to renew early.
    renew_early: Arc<Mutex<BTreeSet<String>>>,
    wanted: Arc<Mutex<Vec<String>>>,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

fn now_secs() -> i64 {
    sw_core::time::now_ms() / 1000
}

/// How long to wait after the n-th consecutive failure: 1, 2, 4 ... minutes, at most 6
/// hours. Certificate authorities rate-limit failed validations, so do not hammer them.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(60u64.saturating_mul(1u64 << failures.saturating_sub(1).min(16)))
        .min(Duration::from_secs(6 * 3600))
}

impl Manager {
    pub fn new(acme: Acme, store: CertStore, options: ManagerOptions) -> Self {
        Self {
            acme: Arc::new(acme),
            store,
            options,
            attempts: Arc::default(),
            renew_early: Arc::default(),
            wanted: Arc::default(),
        }
    }

    pub fn store(&self) -> &CertStore {
        &self.store
    }

    /// Bring the store in line with `hostnames`: issue missing certificates, renew those
    /// that are due, and delete certificates for names that are no longer wanted.
    /// Returns the hostnames that got a new certificate.
    pub async fn ensure(&self, hostnames: &[String]) -> Vec<String> {
        *self.wanted.lock().unwrap_or_else(|e| e.into_inner()) = hostnames.to_vec();
        for stale in self
            .store
            .hostnames()
            .into_iter()
            .filter(|h| !hostnames.contains(h))
        {
            tracing::info!(hostname = %stale, "removing the certificate of a hostname that is no longer served");
            self.store.remove(&stale);
            self.attempts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&stale);
        }

        let mut issued = Vec::new();
        for hostname in hostnames {
            let early = self
                .renew_early
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(hostname);
            let needs = match self.store.meta(hostname) {
                None => true,
                Some(meta) => {
                    early
                        || meta.due_for_renewal(now_secs())
                        || meta.issuer_directory != self.acme.directory()
                }
            };
            if !needs || !self.may_try(hostname) {
                continue;
            }
            match self.issue(hostname).await {
                Ok(meta) => {
                    tracing::info!(%hostname, not_after = meta.not_after, "certificate issued");
                    self.attempts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(hostname);
                    self.renew_early
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(hostname);
                    issued.push(hostname.clone());
                }
                Err(reason) => {
                    let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
                    let attempt = attempts.entry(hostname.clone()).or_default();
                    attempt.failures += 1;
                    let wait = backoff(attempt.failures);
                    attempt.next_try = Some(Instant::now() + wait);
                    tracing::warn!(%hostname, %reason, retry_in_secs = wait.as_secs(), "could not get a certificate");
                    attempt.last_error = Some(reason);
                }
            }
        }
        issued
    }

    fn may_try(&self, hostname: &str) -> bool {
        let attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts
            .get(hostname)
            .and_then(|a| a.next_try)
            .is_none_or(|at| Instant::now() >= at)
    }

    async fn issue(&self, hostname: &str) -> Result<CertMeta, String> {
        if let Some(precheck) = &self.options.precheck {
            precheck(hostname.to_owned())
                .await
                .map_err(|reason| format!("not asking for a certificate yet: {reason}"))?;
        }
        let (chain, key) = self.acme.issue(hostname).await.map_err(|e| e.to_string())?;
        self.store
            .insert(hostname, &chain, &key, self.acme.directory())
            .map_err(|e| e.to_string())
    }

    /// Ask the certificate authority, for each certificate, whether it wants it renewed
    /// early. Names it flags are renewed on the next `ensure`.
    pub async fn check_renewal_information(&self) {
        for hostname in self.store.hostnames() {
            let Some(leaf) = self.store.leaf(&hostname) else {
                continue;
            };
            if self.acme.renewal_suggested(&leaf).await == Some(true) {
                tracing::info!(%hostname, "the certificate authority suggests renewing now");
                self.renew_early
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(hostname);
            }
        }
    }

    /// The state of every wanted hostname.
    pub fn status(&self) -> Vec<HostStatus> {
        let attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let mut names = self
            .wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        names.sort();
        names
            .into_iter()
            .map(|hostname| HostStatus {
                certificate: self.store.meta(&hostname),
                last_error: attempts.get(&hostname).and_then(|a| a.last_error.clone()),
                hostname,
            })
            .collect()
    }

    /// Run until cancelled: scan for work regularly, and check renewal information
    /// every `ari_interval`.
    pub fn spawn(
        &self,
        hostnames: Hostnames,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut scan = tokio::time::interval(manager.options.scan_interval);
            scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_ari = Instant::now();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = scan.tick() => {
                        if last_ari.elapsed() >= manager.options.ari_interval {
                            last_ari = Instant::now();
                            manager.check_renewal_information().await;
                        }
                        manager.ensure(&hostnames().await).await;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_back_off_exponentially_up_to_six_hours() {
        assert_eq!(backoff(1), Duration::from_secs(60));
        assert_eq!(backoff(2), Duration::from_secs(120));
        assert_eq!(backoff(5), Duration::from_secs(960));
        assert_eq!(backoff(12), Duration::from_secs(6 * 3600));
        assert_eq!(backoff(200), Duration::from_secs(6 * 3600));
    }
}
