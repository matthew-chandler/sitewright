//! Sitewright site manager: site store, git history, releases, validation and export.
//!
//! The [`SiteStore`] is the only code that changes sites. Everything else (the CLI, the web
//! UI and the agent's tools) goes through it, so its guardrails apply to all of them.

pub mod confine;
pub mod domains;
pub mod error;
pub mod export;
mod git;
pub mod import;
pub mod routes;
pub mod store;
pub mod validate;

pub use domains::{DomainCheck, DomainChecker};
pub use error::{Result, SiteError};
pub use export::{Export, ExportFormat};
pub use import::ImportFile;
pub use routes::{Route, RouteTable};
pub use store::{
    Author, FileEntry, Hostname, ImportReport, PublishOutcome, Release, ReleaseHooks, ReleasePlan,
    Site, SiteInfo, SiteStatus, SiteStore, Upload, UploadOutcome,
};
