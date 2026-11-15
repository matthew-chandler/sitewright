//! Sitewright metrics: request log, event beacon, uptime checks, rollups and dashboards.
//!
//! Privacy rules that hold everywhere in this crate: raw IP addresses are never stored, and
//! visitors are told apart only by a hash salted with a key that rotates every day.

pub mod beacon;
pub mod dashboard;
pub mod request_log;
pub mod rollup;
pub mod sandbox;
pub mod uptime;
pub mod visitor;

pub use request_log::{RequestLog, RequestRecord};
pub use sandbox::{QueryResult, Sandbox};
pub use visitor::{UaClass, VisitorHasher, classify_user_agent};

/// Handles to everything metrics-related that request handlers and agent tools need.
/// Cheap to clone.
#[derive(Debug, Clone)]
pub struct Metrics {
    pub db: sqlx::SqlitePool,
    pub sandbox: std::sync::Arc<Sandbox>,
    pub beacon_limiter: std::sync::Arc<beacon::BeaconLimiter>,
    /// A check slower than this marks a site as degraded.
    pub slow_ms: u64,
}

impl Metrics {
    /// Open the metrics services on the database file at `db_path`.
    pub async fn open(
        db: sqlx::SqlitePool,
        db_path: &std::path::Path,
        slow_ms: u64,
    ) -> Result<Self, sqlx::Error> {
        Ok(Self {
            db,
            sandbox: std::sync::Arc::new(Sandbox::open(db_path).await?),
            beacon_limiter: std::sync::Arc::default(),
            slow_ms,
        })
    }
}
