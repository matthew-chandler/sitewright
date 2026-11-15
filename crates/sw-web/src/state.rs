//! Shared state for all request handlers.

use std::sync::Arc;

use sw_agent::Agent;
use sw_core::Config;
use sw_metrics::{RequestLog, VisitorHasher};
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
    pub agent: Agent,
    pub domains: DomainChecker,
    pub providers: Providers,
    pub auth: Arc<Auth>,
    /// The admin UI's hostname, normalized.
    pub admin_host: Arc<str>,
    /// The admin UI's origin, for example `https://admin.example.com`.
    pub admin_origin: Arc<str>,
}
