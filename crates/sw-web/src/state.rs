//! Shared state for all request handlers.

use std::sync::Arc;

use sw_core::Config;
use sw_metrics::{RequestLog, VisitorHasher};
use sw_sites::SiteStore;

/// Everything a handler needs. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: SiteStore,
    pub request_log: RequestLog,
    pub hasher: Arc<VisitorHasher>,
    /// The admin UI's hostname, normalized.
    pub admin_host: Arc<str>,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        store: SiteStore,
        request_log: RequestLog,
        hasher: Arc<VisitorHasher>,
    ) -> Self {
        let admin_host = config.domains.admin_host().into();
        Self {
            config,
            store,
            request_log,
            hasher,
            admin_host,
        }
    }
}
