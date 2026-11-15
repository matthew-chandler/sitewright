//! Sitewright: turn a prompt into a live website on your own server.
//!
//! This crate wires the daemon and CLI together. The interesting parts live in the
//! `sw-*` crates; see `docs/architecture.md`.

use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;

/// Build the daemon's HTTP router.
///
/// M0 placeholder: every host and path answers `hello`. M1 replaces this with
/// host-based routing to published sites and the admin UI.
pub fn router() -> Router {
    Router::new().fallback(|| async { "hello\n" })
}

/// Serve `router()` on an already-bound listener until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr: SocketAddr = listener.local_addr()?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
