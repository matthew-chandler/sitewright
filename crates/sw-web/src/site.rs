//! Serving a published site from its immutable release directory.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use sw_core::names::normalize_hostname;
use sw_core::time::now_ms;
use sw_metrics::{RequestRecord, beacon, classify_user_agent};
use sw_sites::Route;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::client_ip::{client_ip, header_str};
use crate::state::AppState;

/// Serve one request for a site and record it in the request log.
pub async fn serve(
    state: &AppState,
    route: Arc<Route>,
    peer: SocketAddr,
    request: Request,
) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    // The analytics beacon is served by the daemon on every site, and is not itself
    // counted as traffic.
    if path == beacon::SCRIPT_PATH {
        return beacon_script(&method);
    }
    if path == beacon::EVENT_PATH {
        return beacon_event(state, &route, peer, request).await;
    }
    let user_agent = header_str(request.headers(), "user-agent")
        .unwrap_or("")
        .to_owned();
    let referrer_host = header_str(request.headers(), "referer").and_then(referrer_host);
    let ip = client_ip(&state.config.server, peer, request.headers());

    let visitor_hash = state.hasher.hash(ip, &user_agent, route.site_id);

    let release = Release {
        site_id: route.site_id,
        site: &route.site,
        sha: &route.sha,
        root: &route.root,
        backend: route.backend,
    };
    let response = respond(&state.backend, &release, request, &visitor_hash).await;

    let bytes = header_str(response.headers(), "content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    state.request_log.record(RequestRecord {
        ts: now_ms(),
        site_id: route.site_id,
        method: method.as_str().to_owned(),
        path,
        status: response.status().as_u16(),
        bytes,
        duration_ms: started.elapsed().as_millis() as u64,
        referrer_host,
        ua_class: classify_user_agent(&user_agent),
        visitor_hash,
    });
    response
}

/// One release of one site, as the responder needs it.
#[derive(Debug, Clone, Copy)]
pub struct Release<'a> {
    pub site_id: i64,
    pub site: &'a sw_core::SiteName,
    /// The release's version: its commit sha.
    pub sha: &'a str,
    /// The directory holding the release's files.
    pub root: &'a Path,
    /// Whether the release has a backend.
    pub backend: bool,
}

/// Answer one request for a site: a static file, or its backend for `/api`. This is the
/// whole of a site's public behaviour; the daemon adds logging and the beacon around
/// it, and a containerized site runs exactly this.
pub async fn respond(
    backends: &sw_backend::SiteBackends,
    release: &Release<'_>,
    request: Request,
    visitor: &str,
) -> Response {
    let path = request.uri().path();
    let method = request.method().clone();
    let is_api = path == "/api" || path.starts_with("/api/");
    let mut response = if sw_sites::confine::is_backend_path(path.trim_start_matches('/')) {
        // Backend source and migrations are part of the release but never served.
        StatusCode::NOT_FOUND.into_response()
    } else if is_api && release.backend && backends.enabled() {
        backend_response(backends, release, request, visitor).await
    } else if method == Method::GET || method == Method::HEAD {
        serve_file(release.root, request).await
    } else {
        (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
            "method not allowed\n",
        )
            .into_response()
    };
    apply_headers(response.headers_mut());
    response
}

/// Request headers a backend never sees: connection management, and anything that
/// identifies the visitor's address. Backends get the same daily-rotating visitor hash
/// the metrics use instead.
fn hidden_from_backend(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "upgrade"
            | "content-length"
            | "forwarded"
            | "x-real-ip"
            | "true-client-ip"
            | "cf-connecting-ip"
            | "cf-connecting-ipv6"
            | "x-sitewright-visitor"
    ) || name.starts_with("x-forwarded-")
        || name.starts_with("proxy-")
}

fn backend_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!("{{\"error\":\"{message}\"}}"),
    )
        .into_response()
}

/// Hand an `/api/*` request to the site's backend.
async fn backend_response(
    backends: &sw_backend::SiteBackends,
    release: &Release<'_>,
    request: Request,
    visitor: &str,
) -> Response {
    use sw_backend::{BackendError, BackendRequest};

    let (parts, body) = request.into_parts();
    let limit = backends.limits().max_request_body;
    let Ok(bytes) = axum::body::to_bytes(body, limit).await else {
        return backend_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "the request body is too large",
        );
    };
    let Ok(body) = String::from_utf8(bytes.to_vec()) else {
        return backend_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the request body must be UTF-8 text",
        );
    };
    let mut headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter(|(name, _)| !hidden_from_backend(name.as_str()))
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
    headers.push(("x-sitewright-visitor".into(), visitor.to_owned()));
    let head = parts.method == Method::HEAD;
    let backend_request = BackendRequest {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or("").to_owned(),
        headers,
        body,
    };
    let answer = backends
        .serve(
            release.site_id,
            release.site,
            release.sha,
            release.root,
            backend_request,
        )
        .await;
    let answer = match answer {
        Ok(answer) => answer,
        Err(BackendError::Timeout) => {
            return backend_error(StatusCode::GATEWAY_TIMEOUT, "the backend took too long");
        }
        Err(BackendError::Busy) => {
            let mut response =
                backend_error(StatusCode::SERVICE_UNAVAILABLE, "the backend is busy");
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            return response;
        }
        Err(BackendError::TooLarge) => {
            return backend_error(StatusCode::BAD_GATEWAY, "the backend's answer is too large");
        }
        Err(error) => {
            tracing::warn!(site = %release.site, %error, "backend request failed");
            return backend_error(StatusCode::INTERNAL_SERVER_ERROR, "backend error");
        }
    };
    let mut response = Response::new(Body::from(if head { String::new() } else { answer.body }));
    *response.status_mut() =
        StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (name, value) in answer.headers {
        if matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "connection" | "keep-alive" | "upgrade"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            response.headers_mut().append(name, value);
        }
    }
    // Answers are per request unless the backend says otherwise.
    if !response.headers().contains_key(header::CACHE_CONTROL) {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

fn beacon_script(method: &Method) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
        ],
        beacon::SCRIPT,
    )
        .into_response()
}

/// Accept one analytics event. Always answers quickly and says little: this endpoint is
/// open to the internet.
async fn beacon_event(
    state: &AppState,
    route: &Route,
    peer: SocketAddr,
    request: Request,
) -> Response {
    if request.method() != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let user_agent = header_str(request.headers(), "user-agent")
        .unwrap_or("")
        .to_owned();
    let ip = client_ip(&state.config.server, peer, request.headers());
    let Ok(body) = axum::body::to_bytes(request.into_body(), beacon::MAX_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let Ok(event) = beacon::parse_event(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Bots do not run the script; anything claiming to be one here is noise.
    if classify_user_agent(&user_agent) == sw_metrics::UaClass::Bot {
        return StatusCode::NO_CONTENT.into_response();
    }
    let visitor = state.hasher.hash(ip, &user_agent, route.site_id);
    if !state.metrics.beacon_limiter.allow(&visitor) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    if let Err(error) = beacon::record(&state.metrics.db, route.site_id, &visitor, &event).await {
        tracing::error!(%error, "could not store a beacon event");
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Resolve the request path inside `root`: the file itself, a directory's `index.html`,
/// or `<path>.html` for extension-less paths. Misses get the site's `404.html` if it has
/// one.
async fn serve_file(root: &Path, request: Request) -> Response {
    let (parts, _body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let sub_request = |path: &str| {
        let mut builder = Request::builder().method(parts.method.clone()).uri(path);
        if let Some(headers) = builder.headers_mut() {
            // Conditional and range headers matter to the file service; nothing else does.
            for name in [
                header::IF_MODIFIED_SINCE,
                header::IF_UNMODIFIED_SINCE,
                header::RANGE,
                header::IF_RANGE,
            ] {
                if let Some(value) = parts.headers.get(&name) {
                    headers.insert(name, value.clone());
                }
            }
        }
        builder.body(Body::empty())
    };
    let dir = || ServeDir::new(root).append_index_html_on_directories(true);

    let Ok(first) = sub_request(&path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let response = dir()
        .oneshot(first)
        .await
        .unwrap_or_else(|never| match never {});
    if response.status() != StatusCode::NOT_FOUND {
        return response.map(Body::new);
    }

    // Clean URLs: /about serves about.html.
    let last = path.rsplit('/').next().unwrap_or("");
    if !last.is_empty()
        && !last.contains('.')
        && let Ok(retry) = sub_request(&format!("{path}.html"))
    {
        let response = dir()
            .oneshot(retry)
            .await
            .unwrap_or_else(|never| match never {});
        if response.status() != StatusCode::NOT_FOUND {
            return response.map(Body::new);
        }
    }

    let custom = root.join("404.html");
    if tokio::fs::metadata(&custom)
        .await
        .is_ok_and(|m| m.is_file())
    {
        let request = Request::builder()
            .method(parts.method.clone())
            .uri("/404.html")
            .body(Body::empty());
        if let Ok(request) = request {
            let mut response = ServeFile::new(custom)
                .oneshot(request)
                .await
                .unwrap_or_else(|never| match never {});
            if response.status() == StatusCode::OK {
                *response.status_mut() = StatusCode::NOT_FOUND;
                return response.map(Body::new);
            }
        }
    }
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Not found</title><h1>Not found</h1><p>There is nothing at this address.</p>\n",
    )
        .into_response()
}

/// Security and caching headers for every site response.
fn apply_headers(headers: &mut HeaderMap) {
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("frame-ancestors 'self'"),
    );
    // Releases change the files behind a URL, so browsers must revalidate. HTML is
    // revalidated on every use; other assets may be reused briefly.
    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    let cache = if is_html {
        "no-cache"
    } else {
        "public, max-age=300, must-revalidate"
    };
    // A backend's own choice (or its "no-store" default) stands.
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    }
}

/// The hostname of a `Referer` URL, if it has one.
fn referrer_host(referer: &str) -> Option<String> {
    let uri: Uri = referer.parse().ok()?;
    normalize_hostname(uri.host()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_referrer_host() {
        assert_eq!(
            referrer_host("https://News.Example.org/a?b=c").as_deref(),
            Some("news.example.org")
        );
        assert_eq!(
            referrer_host("http://example.org:8080/").as_deref(),
            Some("example.org")
        );
        assert_eq!(referrer_host("/relative"), None);
        assert_eq!(referrer_host("not a url"), None);
    }
}
