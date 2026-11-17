//! Sitewright web layer: site serving, admin UI, HTTP API, SSE and authentication.
//!
//! One listener serves everything. [`dispatch`] looks at the `Host` header and hands the
//! request to the admin application, to a published site, or to a plain 404.

pub mod admin;
pub mod api;
pub mod auth;
pub mod client_ip;
pub mod container;
pub mod dispatch;
pub mod error;
pub mod markdown;
pub mod providers;
pub mod site;
pub mod socket;
pub mod state;
pub mod ui;
pub mod upload;

pub use dispatch::public_router;
pub use state::AppState;
