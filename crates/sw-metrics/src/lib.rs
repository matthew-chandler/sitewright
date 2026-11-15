//! Sitewright metrics: request log, event beacon, uptime checks, rollups and dashboards.
//!
//! Privacy rules that hold everywhere in this crate: raw IP addresses are never stored, and
//! visitors are told apart only by a hash salted with a key that rotates every day.

pub mod request_log;
pub mod visitor;

pub use request_log::{RequestLog, RequestRecord};
pub use visitor::{UaClass, VisitorHasher, classify_user_agent};
