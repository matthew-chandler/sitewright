//! Shared state for all request handlers.

use std::sync::Arc;

use sw_agent::Agent;
use sw_core::Config;
use sw_metrics::{Metrics, RequestLog, VisitorHasher};
use sw_sites::{DomainChecker, SiteStore};

use crate::auth::Auth;
use crate::providers::Providers;

/// Everything a handler needs. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: SiteStore,
    pub request_log: RequestLog,
    pub hasher: Arc<VisitorHasher>,
    pub metrics: Metrics,
    pub agent: Agent,
    /// Site backends: the workers and databases behind `/api/*`.
    pub backend: sw_backend::SiteBackends,
    pub domains: DomainChecker,
    pub providers: Providers,
    pub auth: Arc<Auth>,
    /// The certificate manager, once it is running (standalone mode only).
    pub tls: Arc<std::sync::OnceLock<sw_tls::Manager>>,
    /// Notified to make the daemon exit cleanly so its supervisor restarts it (after an
    /// update or a rollback).
    pub restart: Arc<tokio::sync::Notify>,
    /// The newest released version, when it is newer than the running one.
    pub update_available: Arc<std::sync::Mutex<Option<String>>>,
    /// The admin UI's hostname, normalized.
    pub admin_host: Arc<str>,
    /// The admin UI's origin, for example `https://admin.example.com`.
    pub admin_origin: Arc<str>,
}
