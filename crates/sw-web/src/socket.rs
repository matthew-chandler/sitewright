//! The admin socket: a Unix socket in the data directory that the CLI talks to.
//!
//! Anyone who can open the socket can administer the daemon, so it is created with
//! owner-only permissions inside the owner-only data directory. Shell access to the
//! server is the root of trust.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use axum::Router;
use tokio::net::{UnixListener, UnixStream};

/// Bind the admin socket at `path`, replacing a stale socket file left by a crashed
/// daemon. Fails if another daemon is still listening there.
pub async fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "another sitewright daemon is already using {}",
                    path.display()
                ),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve `router` on the admin socket until `shutdown` resolves.
pub async fn serve(
    listener: UnixListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}
