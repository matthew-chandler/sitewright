//! The JSON API. Served on the admin socket for the CLI (and, from M3, on the admin
//! hostname behind authentication).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sw_sites::{Author, ImportReport, PublishOutcome, Release, Site, SiteInfo, import};

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
        .with_state(state)
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
