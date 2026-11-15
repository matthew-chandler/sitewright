//! Host-based dispatch: the entry point for every request on the public listener.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use sw_core::names::normalize_hostname;
use sw_sites::domains::WELL_KNOWN_PREFIX;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;

use crate::state::AppState;
use crate::{admin, site};

#[derive(Clone)]
struct Dispatch {
    state: AppState,
    admin: Router,
}

/// The application served on the public listener. Serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()`.
pub fn public_router(state: AppState) -> Router {
    let admin = admin::router(state.clone());
    Router::new()
        .fallback(dispatch)
        .with_state(Dispatch { state, admin })
        .layer(CompressionLayer::new())
}

async fn dispatch(
    State(d): State<Dispatch>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let Some(host) = request_host(&request) else {
        return (StatusCode::BAD_REQUEST, "missing or invalid Host header\n").into_response();
    };
    // Domain checks: answered on every hostname, so a name can be verified before (and
    // regardless of whether) it belongs to a site.
    if let Some(token) = request.uri().path().strip_prefix(WELL_KNOWN_PREFIX) {
        return match d.state.domains.answer(token) {
            Some(proof) => ([(header::CACHE_CONTROL, "no-store")], proof).into_response(),
            None => unknown_host(),
        };
    }
    if host == *d.state.admin_host {
        return match d.admin.oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
    }
    let routes = d.state.store.routes();
    match routes.get(&host) {
        Some(route) => site::serve(&d.state, route.clone(), peer, request).await,
        None => unknown_host(),
    }
}

/// The hostname the client asked for: the `Host` header, or the request target's
/// authority for HTTP/2.
fn request_host(request: &Request) -> Option<String> {
    let raw = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()))?;
    normalize_hostname(raw)
}

/// The response for a hostname that is neither a site nor the admin host.
fn unknown_host() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "404: there is no site at this address\n",
    )
        .into_response()
}
