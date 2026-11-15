//! Process-wide TLS crypto provider.
//!
//! Sitewright uses `rustls` with the `ring` provider everywhere (see ADR-0001). rustls
//! needs the provider installed once per process before any TLS client or server is built.

/// Install the `ring` crypto provider as the process default. Safe to call many times.
pub fn install_default_provider() {
    // An error only means a provider is already installed, which is what we want.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
