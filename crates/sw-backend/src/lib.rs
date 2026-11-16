//! Site backends: agent-written JavaScript that answers a site's `/api/*` requests.
//!
//! The code runs in a separate worker process per site ([`worker`]), jailed with seccomp
//! ([`sandbox`]) so that it can do nothing but exchange messages ([`protocol`]) with the
//! daemon. See ADR-0006.

pub mod manager;
pub mod protocol;
pub mod sandbox;
pub mod sitedb;
pub mod sites;
pub mod worker;

pub use manager::self_test;
pub use manager::{
    BackendError, BackendRequest, BackendResponse, Backends, Host, Limits, Target, WorkerCommand,
};
pub use sitedb::{DbLimits, Migration, RunResult, SiteDb, SiteDbs};
pub use sites::{BackendSource, DraftOutcome, LogLine, SiteBackends};
pub use worker::{ENTRY, WORKER_ARG, worker_main};
