//! Sitewright TLS: ACME issuance, certificate store and per-hostname resolver.
//!
//! Used in standalone mode, where the daemon terminates TLS itself. Certificates are
//! requested (HTTP-01) only for hostnames the daemon already knows: a TLS handshake for
//! an unknown name never triggers issuance, it simply fails.

pub mod acme;
pub mod manager;
pub mod serve;
pub mod store;

pub use acme::{Acme, Challenges};
pub use manager::{HostStatus, Manager, ManagerOptions};
pub use store::CertStore;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("ACME error: {0}")]
    Acme(String),
    #[error("certificate error: {0}")]
    Certificate(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
