//! The admin application, served on the admin hostname: authentication endpoints, the
//! guard that protects everything else, static assets and the PWA files.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use webauthn_rs::prelude::{PublicKeyCredential, RegisterPublicKeyCredential};

use crate::auth::{Auth, AuthError, Session};
use crate::client_ip::{client_ip, header_str};
use crate::error::ApiError;
use crate::state::AppState;
use crate::{api, ui};

#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/static"]
struct Assets;

pub fn router(state: AppState) -> Router {
    Router::new()
        // Open to everyone.
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/login", get(ui::login_page))
        .route("/enroll/{token}", get(ui::enroll_page))
        .route("/auth/login/start", post(login_start))
        .route("/auth/login/finish", post(login_finish))
        .route("/auth/register/start", post(register_start))
        .route("/auth/register/finish", post(register_finish))
        .route("/static/{*path}", get(static_file))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        // Everything below needs a session (or, under /api, an API token).
        .route("/logout", post(logout))
        .merge(ui::routes())
        .nest_service("/api", api::router(state.clone()))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Paths that do not need a session.
fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/healthz" | "/login" | "/manifest.webmanifest" | "/sw.js"
    ) || path.starts_with("/auth/")
        || path.starts_with("/enroll/")
        || path.starts_with("/static/")
}

async fn current_session(auth: &Auth, headers: &HeaderMap) -> Option<Session> {
    let token = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(Auth::token_from_cookie_header)?;
    auth.session(token).await.ok().flatten()
}

/// The gate in front of the whole admin application.
///
/// - Requests that change state must come from the admin origin (when the browser says
///   where they came from) and, if they carry a session, must carry its CSRF token.
/// - Anything not public needs a session; under `/api`, an API token also works.
async fn guard(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let safe = matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    let path = request.uri().path().to_owned();
    let is_api = path.starts_with("/api/");

    if !safe
        && let Some(origin) = header_str(request.headers(), "origin")
        && origin != &*state.admin_origin
    {
        return deny(
            StatusCode::FORBIDDEN,
            "requests from other origins are not allowed",
        );
    }

    let session = current_session(&state.auth, request.headers()).await;
    let response = match session {
        Some(session) => {
            if !safe
                && header_str(request.headers(), "x-csrf-token")
                    != Some(session.csrf_token.as_str())
            {
                return deny(
                    StatusCode::FORBIDDEN,
                    "missing or wrong CSRF token; reload the page and try again",
                );
            }
            request.extensions_mut().insert(session);
            next.run(request).await
        }
        None if is_public(&path) => next.run(request).await,
        None if is_api => {
            let bearer = header_str(request.headers(), "authorization")
                .and_then(|v| v.strip_prefix("Bearer "));
            match bearer {
                Some(token)
                    if state
                        .auth
                        .verify_api_token(token.trim())
                        .await
                        .unwrap_or(false) =>
                {
                    next.run(request).await
                }
                _ => deny(
                    StatusCode::UNAUTHORIZED,
                    "sign in, or send an API token as `Authorization: Bearer <token>`",
                ),
            }
        }
        None if safe => Redirect::to("/login").into_response(),
        None => deny(StatusCode::UNAUTHORIZED, "sign in first"),
    };
    with_security_headers(response)
}

fn deny(status: StatusCode, message: &str) -> Response {
    with_security_headers(ApiError::new(status, message).into_response())
}

/// Headers for every admin response. All scripts and styles are served from this origin,
/// so the policy can forbid everything else.
fn with_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
             object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        let status = match &e {
            AuthError::Denied(_) => StatusCode::FORBIDDEN,
            AuthError::Invalid(_) | AuthError::Webauthn(_) => StatusCode::BAD_REQUEST,
            AuthError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            AuthError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %e, "authentication request failed");
        }
        Self::new(status, e.to_string())
    }
}

fn rate_limit(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> Result<(), ApiError> {
    Ok(state
        .auth
        .check_rate(client_ip(&state.config.server, peer, headers))?)
}

fn session_response(token: &str) -> Result<Response, ApiError> {
    let cookie = HeaderValue::from_str(&Auth::session_cookie(token)).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not build the session cookie",
        )
    })?;
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true, "redirect": "/" })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct RegisterStart {
    invite: Option<String>,
}

async fn register_start(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    session: Option<Extension<Session>>,
    headers: HeaderMap,
    Json(body): Json<RegisterStart>,
) -> Result<Json<Value>, ApiError> {
    rate_limit(&state, peer, &headers)?;
    let session = session.map(|Extension(s)| s);
    let (ceremony, options) = state
        .auth
        .register_start(body.invite.as_deref(), session.as_ref())
        .await?;
    Ok(Json(json!({ "ceremony": ceremony, "options": options })))
}

#[derive(Debug, Deserialize)]
struct RegisterFinish {
    ceremony: String,
    credential: RegisterPublicKeyCredential,
    #[serde(default)]
    name: String,
}

async fn register_finish(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinish>,
) -> Result<Response, ApiError> {
    rate_limit(&state, peer, &headers)?;
    let user_agent = header_str(&headers, "user-agent").unwrap_or("");
    match state
        .auth
        .register_finish(&body.ceremony, &body.credential, &body.name, user_agent)
        .await?
    {
        Some(new) => session_response(&new.token),
        None => Ok(Json(json!({ "ok": true, "redirect": "/settings" })).into_response()),
    }
}

async fn login_start(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    rate_limit(&state, peer, &headers)?;
    let (ceremony, options) = state.auth.login_start().await?;
    Ok(Json(json!({ "ceremony": ceremony, "options": options })))
}

#[derive(Debug, Deserialize)]
struct LoginFinish {
    ceremony: String,
    credential: PublicKeyCredential,
}

async fn login_finish(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginFinish>,
) -> Result<Response, ApiError> {
    rate_limit(&state, peer, &headers)?;
    let user_agent = header_str(&headers, "user-agent").unwrap_or("");
    let new = state
        .auth
        .login_finish(&body.ceremony, &body.credential, user_agent)
        .await?;
    session_response(&new.token)
}

async fn logout(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
) -> Result<Response, ApiError> {
    state.auth.logout(&session).await?;
    let cookie = HeaderValue::from_str(&Auth::clear_cookie()).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not clear the session cookie",
        )
    })?;
    Ok((
        [
            (header::SET_COOKIE, cookie),
            (
                header::HeaderName::from_static("hx-redirect"),
                HeaderValue::from_static("/login"),
            ),
        ],
        "",
    )
        .into_response())
}

/// An embedded static file, revalidated with an ETag.
fn asset(path: &str, request_headers: &HeaderMap) -> Response {
    let Some(file) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", hex::encode(&file.metadata.sha256_hash()[..12]));
    if header_str(request_headers, "if-none-match") == Some(etag.as_str()) {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let mime = match path.rsplit('.').next() {
        Some("webmanifest") => "application/manifest+json".to_owned(),
        _ => mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string(),
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_str(&mime)
                    .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            ),
            (
                header::ETAG,
                HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\"")),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

async fn static_file(Path(path): Path<String>, headers: HeaderMap) -> Response {
    asset(&path, &headers)
}

async fn manifest(headers: HeaderMap) -> Response {
    asset("manifest.webmanifest", &headers)
}

/// The service worker must be served from the root to control the whole origin.
async fn service_worker(headers: HeaderMap) -> Response {
    asset("sw.js", &headers)
}
