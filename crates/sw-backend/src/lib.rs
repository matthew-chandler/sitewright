//! Site backends: agent-written JavaScript that answers a site's `/api/*` requests.
//!
//! The code runs in a separate worker process per site ([`worker`]), jailed with seccomp
//! ([`sandbox`]) so that it can do nothing but exchange messages ([`protocol`]) with the
//! daemon. See ADR-0006.

pub mod auth;
pub mod egress;
pub mod jobs;
pub mod llm;
pub mod manager;
pub mod protocol;
pub mod sandbox;
pub mod secrets;
pub mod sitedb;
pub mod sites;
pub mod standalone;
pub mod worker;

pub use egress::Egress;
pub use jobs::{Cron, JobRecord, ScheduleInfo};
pub use llm::{LlmSettings, LlmUsage, ModelInfo, ModelSource};
pub use manager::self_test;
pub use manager::{
    BackendError, BackendRequest, BackendResponse, Backends, Host, Limits, Target, WorkerCommand,
};
pub use protocol::{Declared, DeclaredSchedule};
pub use sitedb::{DbLimits, Migration, ReadRows, RunResult, SiteDb, SiteDbs};
pub use sites::{
    BackendSource, DraftOutcome, DraftTask, LiveBackend, LogLine, SiteBackends, SiteHostEntry,
};
pub use standalone::{MANIFEST_FILE, ManifestLlm, ManifestModel, SiteManifest, model_env_prefix};
pub use worker::{ENTRY, WORKER_ARG, worker_main};
