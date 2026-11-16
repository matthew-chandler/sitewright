//! The JSON API. Served on the admin socket for the CLI (and, from M3, on the admin
//! hostname behind authentication).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use sw_agent::{Run, RunEvent, RunRequest};
use sw_sites::{
    Author, DomainCheck, ImportReport, PublishOutcome, Release, Site, SiteInfo, import,
};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Routes under `/v1`. Callers of this router are trusted: put authentication in front.
pub fn router(state: AppState) -> Router {
    // An import uploads a whole site as one archive.
    let import_limit = state.config.sites.max_site_bytes as usize + 1024 * 1024;
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/sites", get(list_sites).post(create_site))
        .route("/v1/sites/{name}", get(site_info))
        .route(
            "/v1/sites/{name}/import",
            put(import_site).layer(DefaultBodyLimit::max(import_limit)),
        )
        .route("/v1/sites/{name}/publish", post(publish_site))
        .route("/v1/sites/{name}/rollback", post(rollback_site))
        .route("/v1/sites/{name}/delete", post(delete_site))
        .route("/v1/sites/{name}/restore", post(restore_site))
        .route("/v1/sites/{name}/check-domain", get(check_domain))
        .route("/v1/sites/{name}/export", get(export_site))
        .route("/v1/runs", get(list_runs).post(start_run))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/events", get(run_events))
        .route("/v1/runs/{id}/cancel", post(cancel_run))
        .with_state(state)
}

/// The router for the admin Unix socket: the API plus operations that only someone with
/// shell access to the server may perform.
pub fn socket_router(state: AppState) -> Router {
    let local = Router::new()
        .route("/v1/admin/invite", post(create_invite))
        .route("/v1/admin/restart", post(restart))
        .with_state(state.clone());
    router(state).merge(local)
}

/// Create a one-time passkey enrollment link. Reachable only over the admin socket:
/// shell access is the root of trust, and this is the recovery path.
async fn create_invite(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let token = state.auth.create_invite().await?;
    Ok(Json(json!({
        "url": format!("{}/enroll/{token}", state.admin_origin),
        "valid_minutes": crate::auth::INVITE_MINUTES,
    })))
}

/// Ask the daemon to shut down cleanly; its supervisor (systemd, `Restart=always`)
/// starts it again with whatever binary is now on disk. Socket only.
async fn restart(State(state): State<AppState>) -> Json<Value> {
    sw_core::db::audit(state.store.db(), "cli", "daemon.restart", None, None).await;
    state.restart.notify_one();
    Json(json!({ "restarting": true }))
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "routes": state.store.routes().len(),
        "admin_host": &*state.admin_host,
    }))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default)]
    deleted: bool,
}

async fn list_sites(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<SiteInfoBrief>>> {
    let mut out = Vec::new();
    for site in state.store.list(q.deleted).await? {
        let url = state
            .config
            .domains
            .public_url(&state.config.domains.site_hostname(&site.name));
        out.push(SiteInfoBrief { site, url });
    }
    Ok(Json(out))
}

#[derive(Debug, serde::Serialize)]
struct SiteInfoBrief {
    #[serde(flatten)]
    site: Site,
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    title: Option<String>,
}

async fn create_site(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<SiteInfo>)> {
    let title = body.title.as_deref().unwrap_or(&body.name);
    state.store.create(&body.name, title, Author::User).await?;
    Ok((
        StatusCode::CREATED,
        Json(state.store.info(&body.name).await?),
    ))
}

async fn site_info(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<SiteInfo>> {
    Ok(Json(state.store.info(&name).await?))
}

#[derive(Debug, Deserialize)]
struct ImportQuery {
    title: Option<String>,
    message: Option<String>,
}

/// Replace a site's content with the uploaded `.tar.gz` and publish it. Creates the site
/// if it does not exist yet.
async fn import_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<ImportQuery>,
    body: Bytes,
) -> ApiResult<Json<ImportReport>> {
    // Twice the site limit leaves room for files that will be skipped, without letting a
    // small archive expand without bound.
    let files = import::unpack(&body, state.config.sites.max_site_bytes * 2)?;
    if state.store.find(&name).await?.is_none() {
        state
            .store
            .create(&name, q.title.as_deref().unwrap_or(&name), Author::User)
            .await?;
    }
    let message = q.message.as_deref().unwrap_or("Import site");
    Ok(Json(
        state
            .store
            .import(&name, files, message, Author::User)
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishBody {
    message: Option<String>,
}

async fn publish_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<PublishBody>>,
) -> ApiResult<Json<PublishOutcome>> {
    let message = body.and_then(|b| b.0.message).unwrap_or_default();
    Ok(Json(
        state.store.publish(&name, &message, Author::User).await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackBody {
    release: Option<String>,
}

async fn rollback_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<RollbackBody>>,
) -> ApiResult<Json<Release>> {
    let target = body.and_then(|b| b.0.release);
    Ok(Json(
        state
            .store
            .rollback(&name, target.as_deref(), Author::User)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteBody {
    /// Must equal the site's name: the caller's proof that a person confirmed.
    confirm: String,
}

async fn delete_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<DeleteBody>,
) -> ApiResult<Json<Site>> {
    if body.confirm != name {
        return Err(ApiError::bad_request(
            "to delete a site, confirm by sending its exact name",
        ));
    }
    Ok(Json(state.store.delete(&name, Author::User).await?))
}

async fn restore_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Site>> {
    Ok(Json(state.store.restore(&name, Author::User).await?))
}

/// Check that a site's primary hostname resolves and reaches this server.
async fn check_domain(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<DomainCheck>> {
    let info = state.store.info(&name).await?;
    let hostname = info
        .hostnames
        .first()
        .map(|h| h.hostname.clone())
        .unwrap_or_default();
    let port = match (
        state.config.domains.public_scheme,
        state.config.domains.public_port,
    ) {
        (sw_core::config::Scheme::Http, Some(port)) => port,
        _ => 80,
    };
    Ok(Json(state.domains.check(&hostname, port).await))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRunBody {
    site: String,
    prompt: String,
    /// Create the site in this run.
    #[serde(default)]
    new: bool,
    provider: Option<String>,
}

async fn start_run(
    State(state): State<AppState>,
    Json(body): Json<StartRunBody>,
) -> ApiResult<(StatusCode, Json<Run>)> {
    let run = state
        .agent
        .start(RunRequest {
            site: body.site,
            prompt: body.prompt,
            new: body.new,
            provider: body.provider,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(run)))
}

#[derive(Debug, Deserialize)]
struct RunsQuery {
    site: Option<String>,
    limit: Option<i64>,
}

async fn list_runs(
    State(state): State<AppState>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<Vec<Run>>> {
    Ok(Json(
        state
            .agent
            .list(q.site.as_deref(), q.limit.unwrap_or(20).clamp(1, 200))
            .await?,
    ))
}

async fn get_run(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<Run>> {
    Ok(Json(state.agent.get(id).await?))
}

async fn cancel_run(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    state.agent.get(id).await?;
    Ok(Json(json!({ "cancelling": state.agent.cancel(id) })))
}

/// A run's events as server-sent events: everything stored so far, then live events
/// until the run ends. Each stored event carries its sequence number as the SSE id.
async fn run_events(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let subscription = state.agent.subscribe(id).await?;
    Ok(Sse::new(run_event_stream(subscription)).keep_alive(KeepAlive::default()))
}

/// Replay then follow a run. Shared by the API and (from M3) the web UI.
pub fn run_event_stream(
    subscription: sw_agent::Subscription,
) -> impl Stream<Item = Result<Event, Infallible>> {
    struct Follow {
        past: std::vec::IntoIter<(i64, RunEvent)>,
        live: Option<tokio::sync::broadcast::Receiver<(i64, RunEvent)>>,
        last_seq: i64,
        done: bool,
    }
    let state = Follow {
        past: subscription.past.into_iter(),
        live: subscription.live,
        last_seq: 0,
        done: false,
    };
    futures_util::stream::unfold(state, |mut s| async move {
        loop {
            if s.done {
                return None;
            }
            let (seq, event) = match s.past.next() {
                Some(item) => item,
                None => match s.live.as_mut()?.recv().await {
                    Ok(item) => item,
                    // Closed: the run ended. Lagged: this reader fell too far behind; it
                    // can reconnect and replay from the database.
                    Err(_) => return None,
                },
            };
            if seq != 0 {
                if seq <= s.last_seq {
                    continue; // already replayed from the database
                }
                s.last_seq = seq;
            }
            s.done = matches!(event, RunEvent::Done { .. });
            let mut sse = Event::default()
                .data(serde_json::to_string(&event).unwrap_or_else(|_| "{}".into()));
            if seq != 0 {
                sse = sse.id(seq.to_string());
            }
            return Some((Ok(sse), s));
        }
    })
}

#[derive(Debug, Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

/// Download an archive of a site's live release.
async fn export_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<ExportQuery>,
) -> ApiResult<axum::response::Response> {
    let format = sw_sites::ExportFormat::parse(q.format.as_deref().unwrap_or("folder"))?;
    export_response(&state, &name, format).await
}

/// Build an export and return it as a file download.
pub async fn export_response(
    state: &AppState,
    name: &str,
    format: sw_sites::ExportFormat,
) -> ApiResult<axum::response::Response> {
    use axum::http::header;
    use axum::response::IntoResponse;
    let export = state.store.export(name, format).await?;
    let bytes = tokio::fs::read(&export.path).await.map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the export could not be read",
        )
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", export.file_name),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        bytes,
    )
        .into_response())
}
