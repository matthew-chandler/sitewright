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
use sw_metrics::{RequestRecord, classify_user_agent};
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
    let user_agent = header_str(request.headers(), "user-agent")
        .unwrap_or("")
        .to_owned();
    let referrer_host = header_str(request.headers(), "referer").and_then(referrer_host);
    let ip = client_ip(&state.config.server, peer, request.headers());

    let mut response = if method == Method::GET || method == Method::HEAD {
        serve_file(&route.root, request).await
    } else {
        (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
            "method not allowed\n",
        )
            .into_response()
    };
    apply_headers(response.headers_mut());

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
        visitor_hash: state.hasher.hash(ip, &user_agent, route.site_id),
    });
    response
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
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
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
