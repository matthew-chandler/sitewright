//! Listeners for standalone mode: HTTPS with per-hostname certificates, and the plain
//! HTTP side that answers ACME challenges and redirects everything else.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::acme::{CHALLENGE_PREFIX, Challenges};
use crate::store::CertStore;

/// The TLS configuration for the HTTPS listener: certificates come from the store, by
/// server name; HTTP/2 and HTTP/1.1 are offered.
pub fn server_config(store: CertStore) -> Arc<rustls::ServerConfig> {
    sw_core::crypto::install_default_provider();
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(store));
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

/// Serve `app` over TLS until `cancel` fires. Each connection's peer address is made
/// available to handlers as `ConnectInfo<SocketAddr>`, as on the plain listener.
pub async fn serve_https(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    app: Router,
    cancel: CancellationToken,
) {
    let acceptor = TlsAcceptor::from(config);
    loop {
        let (stream, peer) = tokio::select! {
            _ = cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::warn!(%error, "accepting a TLS connection failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let (acceptor, app, cancel) = (acceptor.clone(), app.clone(), cancel.clone());
        tokio::spawn(async move {
            // A handshake that never finishes must not hold a task forever.
            let Ok(Ok(tls)) =
                tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await
            else {
                return; // unknown hostname, no SNI, or a client that went away
            };
            let service = hyper::service::service_fn(
                move |mut request: hyper::Request<hyper::body::Incoming>| {
                    request.extensions_mut().insert(ConnectInfo(peer));
                    app.clone().oneshot(request.map(axum::body::Body::new))
                },
            );
            let builder = Builder::new(TokioExecutor::new());
            let connection = builder.serve_connection_with_upgrades(TokioIo::new(tls), service);
            tokio::pin!(connection);
            tokio::select! {
                _ = connection.as_mut() => {}
                _ = cancel.cancelled() => {
                    connection.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(Duration::from_secs(5), connection).await;
                }
            }
        });
    }
}

#[derive(Clone)]
struct HttpSide {
    challenges: Challenges,
    /// Where to send everything else: the HTTPS port, or `None` to serve `inner` over
    /// plain HTTP as well.
    redirect_to_port: Option<u16>,
    inner: Router,
}

/// The plain-HTTP application for standalone mode. ACME challenges are always answered.
/// With `redirect_to_port`, every other request is redirected to HTTPS, except paths
/// under `/.well-known/` (domain checks must work before a certificate exists).
/// Without it, other requests are served by `inner`.
pub fn http_app(challenges: Challenges, redirect_to_port: Option<u16>, inner: Router) -> Router {
    Router::new().fallback(http_side).with_state(HttpSide {
        challenges,
        redirect_to_port,
        inner,
    })
}

async fn http_side(State(side): State<HttpSide>, request: Request) -> Response {
    let path = request.uri().path();
    if let Some(token) = path.strip_prefix(CHALLENGE_PREFIX) {
        return match side.challenges.answer(token) {
            Some(body) => {
                ([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if let Some(port) = side.redirect_to_port
        && !path.starts_with("/.well-known/")
    {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .and_then(sw_core::names::normalize_hostname);
        let Some(host) = host else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let authority = if port == 443 {
            host
        } else {
            format!("{host}:{port}")
        };
        let target = request.uri().path_and_query().map_or("/", |pq| pq.as_str());
        return Redirect::permanent(&format!("https://{authority}{target}")).into_response();
    }
    side.inner
        .oneshot(request)
        .await
        .unwrap_or_else(|never| match never {})
}
