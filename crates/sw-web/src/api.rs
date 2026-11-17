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
    Author, DomainCheck, ImportReport, PublishOutcome, Release, Site, SiteInfo, Upload,
    UploadOutcome, import,
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
        .route(
            "/v1/sites/{name}/uploads",
            get(list_uploads)
                .put(add_upload)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/sites/{name}/uploads/{file}",
            axum::routing::delete(remove_upload),
        )
        .route("/v1/sites/{name}/publish", post(publish_site))
        .route("/v1/sites/{name}/rollback", post(rollback_site))
        .route("/v1/sites/{name}/delete", post(delete_site))
        .route("/v1/sites/{name}/restore", post(restore_site))
        .route("/v1/sites/{name}/rename", post(rename_site))
        .route(
            "/v1/sites/{name}/secrets",
            get(list_secrets).post(set_secret).delete(remove_secret),
        )
        .route("/v1/sites/{name}/llm", get(get_site_llm).post(set_site_llm))
        .route("/v1/sites/{name}/containerize", post(containerize_site))
        .route("/v1/sites/{name}/container/{part}", get(container_part))
        .route(
            "/v1/sites/{name}/backend-hosts",
            get(list_backend_hosts)
                .post(allow_backend_host)
                .delete(remove_backend_host),
        )
        .route("/v1/sites/{name}/title", post(retitle_site))
        .route("/v1/sites/{name}/check-domain", get(check_domain))
        .route(
            "/v1/sites/{name}/hostnames",
            post(add_hostname).delete(remove_hostname),
        )
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
        .route("/v1/admin/status", get(status))
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

/// Everything `sitewright doctor` wants to know from the running daemon. Socket only.
async fn status(State(state): State<AppState>) -> Json<Value> {
    let providers: Vec<Value> = state
        .providers
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| json!({ "id": p.config.id, "kind": p.config.kind.as_str(), "model": p.config.model, "has_key": p.has_key, "needs_key": p.config.key_ref.is_some(), "default": p.is_default }))
        .collect();
    let sites = state.store.list(false).await.unwrap_or_default();
    let health = sw_metrics::uptime::health(&state.metrics.db, state.metrics.slow_ms)
        .await
        .unwrap_or_default();
    let site_rows: Vec<Value> = sites
        .iter()
        .map(|s| {
            json!({
                "name": s.name.as_str(),
                "published": s.current_release.is_some(),
                "status": health.get(&s.id).map_or("unknown", |h| h.status.as_str()),
            })
        })
        .collect();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "mode": if state.config.server.mode == sw_core::config::ServeMode::Standalone { "standalone" } else { "proxy" },
        "admin_origin": &*state.admin_origin,
        "zones": state.config.domains.zones,
        "passkeys": state.auth.list_passkeys().await.map(|p| p.len()).unwrap_or(0),
        "providers": providers,
        "sites": site_rows,
        "certificates": state.tls.get().map(|m| m.status()),
        "update_available": state.update_available.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "spent_today_usd": state.agent.spent_today().await.unwrap_or(0.0),
        "daily_cap_usd": state.config.agent.daily_spend_cap_usd,
    }))
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

async fn list_uploads(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<Upload>>> {
    Ok(Json(state.store.list_uploads(&name).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadQuery {
    /// The file's name; it is cleaned up and placed under `uploads/`.
    name: String,
}

/// Upload one file, sent as the request body. There is no size limit; the body is
/// streamed to disk.
async fn add_upload(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<UploadQuery>,
    body: axum::body::Body,
) -> ApiResult<Json<UploadOutcome>> {
    let staged = crate::upload::stage(&state.store, body)
        .await
        .map_err(|e| match e {
            crate::upload::StageError::Disk(_) => {
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            }
            _ => ApiError::bad_request(e.to_string()),
        })?;
    Ok(Json(
        state
            .store
            .add_upload_file(&name, &q.name, staged, Author::User)
            .await?,
    ))
}

async fn remove_upload(
    State(state): State<AppState>,
    Path((name, file)): Path<(String, String)>,
) -> ApiResult<Json<UploadOutcome>> {
    Ok(Json(
        state
            .store
            .remove_upload(&name, &file, Author::User)
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
    let site = state.store.delete(&name, Author::User).await?;
    state.backend.release(site.id, &site.name).await;
    Ok(Json(site))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretBody {
    name: String,
    /// Required to set a secret; ignored when removing one.
    #[serde(default)]
    value: String,
}

/// The names of a site's backend secrets. Values cannot be read back.
async fn list_secrets(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<String>>> {
    let site = state.store.get(&name).await?;
    state
        .backend
        .secret_names(site.id)
        .map(Json)
        .map_err(ApiError::bad_request)
}

async fn set_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<SecretBody>,
) -> ApiResult<Json<Vec<String>>> {
    let site = state.store.get(&name).await?;
    state
        .backend
        .set_secret(site.id, &body.name, &body.value)
        .map_err(ApiError::bad_request)?;
    state
        .backend
        .secret_names(site.id)
        .map(Json)
        .map_err(ApiError::bad_request)
}

async fn remove_secret(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<SecretBody>,
) -> ApiResult<Json<Vec<String>>> {
    let site = state.store.get(&name).await?;
    if !state
        .backend
        .remove_secret(site.id, &body.name)
        .map_err(ApiError::bad_request)?
    {
        return Err(ApiError::bad_request(format!(
            "{name} has no secret named {}",
            body.name
        )));
    }
    state
        .backend
        .secret_names(site.id)
        .map(Json)
        .map_err(ApiError::bad_request)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackendHostBody {
    host: String,
}

async fn list_backend_hosts(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<sw_backend::SiteHostEntry>>> {
    let site = state.store.get(&name).await?;
    state
        .backend
        .hosts(site.id)
        .await
        .map(Json)
        .map_err(ApiError::bad_request)
}

async fn allow_backend_host(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<BackendHostBody>,
) -> ApiResult<Json<Vec<sw_backend::SiteHostEntry>>> {
    let site = state.store.get(&name).await?;
    state
        .backend
        .approve_host(site.id, &body.host)
        .await
        .map_err(ApiError::bad_request)?;
    list_backend_hosts(State(state), Path(name)).await
}

async fn remove_backend_host(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<BackendHostBody>,
) -> ApiResult<Json<Vec<sw_backend::SiteHostEntry>>> {
    let site = state.store.get(&name).await?;
    state
        .backend
        .remove_host(site.id, &body.host)
        .await
        .map_err(ApiError::bad_request)?;
    list_backend_hosts(State(state), Path(name)).await
}

async fn site_llm_view(state: &AppState, site_id: i64) -> ApiResult<Json<serde_json::Value>> {
    let llm = state.backend.llm();
    Ok(Json(serde_json::json!({
        "settings": llm.settings(site_id).await.map_err(ApiError::bad_request)?,
        "usage": llm.usage(site_id).await.map_err(ApiError::bad_request)?,
        "models": llm.available().await.map_err(ApiError::bad_request)?,
    })))
}

/// A site's AI model access: settings, this month's usage and the models there are.
async fn get_site_llm(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let site = state.store.get(&name).await?;
    site_llm_view(&state, site.id).await
}

/// Fields left out keep their current value.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteLlmBody {
    enabled: Option<bool>,
    models: Option<Vec<String>>,
    monthly_requests: Option<i64>,
    per_minute: Option<i64>,
}

async fn set_site_llm(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<SiteLlmBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let site = state.store.get(&name).await?;
    let llm = state.backend.llm();
    let mut settings = llm.settings(site.id).await.map_err(ApiError::bad_request)?;
    if let Some(enabled) = body.enabled {
        settings.enabled = enabled;
    }
    if let Some(models) = body.models {
        settings.models = models;
    }
    if let Some(n) = body.monthly_requests {
        settings.monthly_requests = n;
    }
    if let Some(n) = body.per_minute {
        settings.per_minute = n;
    }
    llm.set_settings(site.id, &settings)
        .await
        .map_err(ApiError::bad_request)?;
    site_llm_view(&state, site.id).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerizeBody {
    #[serde(default = "default_true")]
    on: bool,
}

/// Mark or unmark a site as containerized.
async fn containerize_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ContainerizeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    state
        .store
        .set_containerized(&name, body.on, Author::User)
        .await?;
    Ok(Json(
        serde_json::json!({ "site": name, "containerized": body.on }),
    ))
}

async fn container_part(
    State(state): State<AppState>,
    Path((name, part)): Path<(String, String)>,
) -> ApiResult<axum::response::Response> {
    container_download(&state, &name, &part).await
}

/// One of a site's container downloads: `image` (the archive for `docker load`),
/// `guide` (the setup guide, Markdown) or `data` (a copy of the site's database).
pub async fn container_download(
    state: &AppState,
    name: &str,
    part: &str,
) -> ApiResult<axum::response::Response> {
    use axum::http::header;
    use axum::response::IntoResponse;
    let attachment = |file_name: &str, content_type: &'static str| {
        [
            (header::CONTENT_TYPE, content_type.to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{file_name}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ]
    };
    let stream = |path: std::path::PathBuf| async move {
        let file = tokio::fs::File::open(&path).await.map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the file could not be read",
            )
        })?;
        Ok::<_, ApiError>(axum::body::Body::from_stream(
            tokio_util::io::ReaderStream::new(file),
        ))
    };
    match part {
        "image" => {
            let image = crate::container::build(state, name)
                .await
                .map_err(ApiError::bad_request)?;
            Ok((
                attachment(&image.file_name, "application/x-tar"),
                stream(image.path).await?,
            )
                .into_response())
        }
        "guide" => {
            let package = crate::container::describe(state, name)
                .await
                .map_err(ApiError::bad_request)?;
            Ok((
                attachment(&format!("{name}-SETUP.md"), "text/markdown; charset=utf-8"),
                crate::container::guide(&package),
            )
                .into_response())
        }
        "data" => {
            let path = crate::container::data_snapshot(state, name)
                .await
                .map_err(ApiError::bad_request)?
                .ok_or_else(|| ApiError::bad_request(format!("{name} has no database")))?;
            Ok((
                attachment(&format!("{name}-data.db"), "application/vnd.sqlite3"),
                stream(path).await?,
            )
                .into_response())
        }
        _ => Err(ApiError::new(StatusCode::NOT_FOUND, "no such download")),
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBody {
    /// The new name.
    name: String,
    /// Keep the old address and redirect it to the new one.
    #[serde(default = "default_true")]
    redirect: bool,
}

async fn rename_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult<Json<SiteInfo>> {
    if state.agent.is_busy(&name) {
        return Err(sw_sites::SiteError::Conflict(
            "a run is in progress on this site; wait for it or cancel it first".into(),
        )
        .into());
    }
    let old = state.store.get(&name).await?;
    state.backend.release(old.id, &old.name).await;
    let site = state
        .store
        .rename(&name, &body.name, body.redirect, Author::User)
        .await?;
    Ok(Json(state.store.info(site.name.as_str()).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TitleBody {
    title: String,
}

async fn retitle_site(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<TitleBody>,
) -> ApiResult<Json<Site>> {
    Ok(Json(
        state
            .store
            .set_title(&name, &body.title, Author::User)
            .await?,
    ))
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
    /// With `new`: mark the site as containerized.
    #[serde(default)]
    containerize: bool,
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
            containerize: body.containerize,
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
            let mut payload = serde_json::to_value(&event).unwrap_or_default();
            // Complete messages also carry their Markdown rendered to safe HTML, for the
            // admin UI. API clients can ignore the extra field.
            if let RunEvent::Text { text } = &event {
                payload["html"] = crate::markdown::render(text).into();
            }
            let mut sse = Event::default().data(payload.to_string());
            if seq != 0 {
                sse = sse.id(seq.to_string());
            }
            return Some((Ok(sse), s));
        }
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostnameBody {
    hostname: String,
    /// Make it the site's primary hostname.
    #[serde(default)]
    primary: bool,
}

async fn add_hostname(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<HostnameBody>,
) -> ApiResult<Json<SiteInfo>> {
    state
        .store
        .add_hostname_as(&name, &body.hostname, body.primary, Author::User)
        .await?;
    Ok(Json(state.store.info(&name).await?))
}

async fn remove_hostname(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<HostnameBody>,
) -> ApiResult<Json<SiteInfo>> {
    state
        .store
        .remove_hostname(&name, &body.hostname, Author::User)
        .await?;
    Ok(Json(state.store.info(&name).await?))
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
