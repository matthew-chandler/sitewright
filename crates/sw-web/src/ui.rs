//! The admin UI's pages and form handlers. Pages are server-rendered templates; forms
//! are submitted by htmx and answer with a redirect header or a small HTML fragment.

use std::convert::Infallible;

use askama::Template;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::handler::Handler;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Form, Router};
use futures_util::Stream;
use serde::Deserialize;
use sw_agent::{Run, RunRequest, RunStatus};
use sw_core::config::{AuthStyle, ProviderConfig, ProviderKind};
use sw_core::time::now_ms;
use sw_sites::Author;

use crate::api::run_event_stream;
use crate::auth::Session;
use crate::state::AppState;

/// Routes that need a session. The guard in `admin.rs` enforces that.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/new", get(new_page).post(new_submit))
        .route("/sites/{name}", get(site_page))
        .route("/sites/{name}/prompt", post(site_prompt))
        .route("/sites/{name}/rollback", post(site_rollback))
        .route("/sites/{name}/delete", post(site_delete))
        .route("/sites/{name}/rename", post(site_rename))
        .route(
            "/sites/{name}/uploads",
            get(uploads_fragment).put(upload_put.layer(DefaultBodyLimit::disable())),
        )
        .route(
            "/sites/{name}/uploads/parts/{id}",
            put(upload_part.layer(DefaultBodyLimit::max(UPLOAD_PART_MAX))),
        )
        .route("/sites/{name}/uploads/{file}", get(upload_preview))
        .route("/sites/{name}/uploads/{file}/delete", post(upload_delete))
        .route("/sites/{name}/secrets", post(secret_save))
        .route("/sites/{name}/secrets/{secret}/delete", post(secret_delete))
        .route("/sites/{name}/hosts", post(host_approve))
        .route("/sites/{name}/llm", post(llm_save))
        .route("/sites/{name}/containerize", post(site_containerize))
        .route("/sites/{name}/container", get(container_page))
        .route("/sites/{name}/container/{part}", get(container_part))
        .route("/sites/{name}/hosts/{host}/delete", post(host_delete))
        .route("/sites/{name}/export/{format}", get(site_export))
        .route("/sites/{name}/dashboard", get(dashboard_page))
        .route("/sites/{name}/dashboard/data", get(dashboard_data))
        .route(
            "/sites/{name}/dashboard/regenerate",
            post(dashboard_regenerate),
        )
        .route("/runs/{id}", get(run_page))
        .route("/runs/{id}/events", get(run_events))
        .route("/runs/{id}/cancel", post(run_cancel))
        .route("/settings", get(settings_page))
        .route("/settings/providers", post(provider_save))
        .route("/settings/providers/{id}/test", post(provider_test))
        .route("/settings/providers/{id}/delete", post(provider_delete))
        .route("/settings/copilot", get(copilot_panel))
        .route("/settings/copilot/login", post(copilot_login))
        .route("/settings/copilot/account", post(copilot_account))
        .route("/settings/copilot/logout", post(copilot_logout))
        .route("/settings/copilot/models", post(copilot_models))
        .route("/settings/dns-check", post(dns_check))
        .route("/settings/passkeys/{id}/delete", post(passkey_delete))
        .route("/settings/tokens", post(token_create))
        .route("/settings/tokens/{id}/delete", post(token_delete))
}

// ---------------------------------------------------------------------- helpers

/// What every page needs for its frame.
#[derive(Debug, Clone, Default)]
pub struct Chrome {
    pub csrf: String,
    pub signed_in: bool,
}

impl Chrome {
    fn for_session(session: &Session) -> Self {
        Self {
            csrf: session.csrf_token.clone(),
            signed_in: true,
        }
    }
}

fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::error!(%error, "template failed to render");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "the page could not be rendered",
            )
                .into_response()
        }
    }
}

/// Escape text for use inside an HTML fragment.
fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A status fragment for a form: an error.
fn error_fragment(message: impl AsRef<str>) -> Response {
    Html(format!(
        "<p class=\"msg error\" role=\"alert\">{}</p>",
        esc(message.as_ref())
    ))
    .into_response()
}

/// A status fragment for a form: success.
fn ok_fragment(message: impl AsRef<str>) -> Response {
    Html(format!(
        "<p class=\"msg ok\" role=\"status\">{}</p>",
        esc(message.as_ref())
    ))
    .into_response()
}

/// Tell htmx to navigate.
fn hx_redirect(to: &str) -> Response {
    match HeaderValue::from_str(to) {
        Ok(value) => ([(HeaderName::from_static("hx-redirect"), value)], "").into_response(),
        Err(_) => error_fragment("could not redirect"),
    }
}

/// Tell htmx to reload the current page.
fn hx_refresh() -> Response {
    (
        [(
            HeaderName::from_static("hx-refresh"),
            HeaderValue::from_static("true"),
        )],
        "",
    )
        .into_response()
}

fn not_found(chrome: Chrome, what: &str) -> Response {
    let mut response = render(MessagePage {
        chrome,
        title: "Not found".into(),
        message: format!("There is no {what} here."),
    });
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// "3 minutes ago" for a Unix-millisecond timestamp.
pub fn ago(ts: i64) -> String {
    let seconds = (now_ms() - ts).max(0) / 1000;
    let (n, unit) = match seconds {
        0..=59 => return "just now".into(),
        60..=3599 => (seconds / 60, "minute"),
        3600..=86_399 => (seconds / 3600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// How long until `ts`, as "in 5 minutes".
pub fn until(ts: i64) -> String {
    let seconds = (ts - now_ms()).max(0) / 1000;
    let (n, unit) = match seconds {
        0..=59 => return "within a minute".into(),
        60..=3599 => ((seconds + 59) / 60, "minute"),
        3600..=86_399 => (seconds / 3600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    format!("in {n} {unit}{}", if n == 1 { "" } else { "s" })
}

fn shorten(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max).collect();
    format!("{head}…")
}

// ---------------------------------------------------------------------- view models

#[derive(Template)]
#[template(path = "message.html")]
struct MessagePage {
    chrome: Chrome,
    title: String,
    message: String,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    chrome: Chrome,
    has_passkeys: bool,
}

#[derive(Template)]
#[template(path = "enroll.html")]
struct EnrollPage {
    chrome: Chrome,
    valid: bool,
    token: String,
}

struct SiteCard {
    name: String,
    title: String,
    url: String,
    /// `up`, `degraded`, `down`, `unknown`, `draft` or `busy`: also the CSS class of the
    /// status dot.
    state: &'static str,
    state_label: &'static str,
    deployed: String,
    /// "99.9% up" over 30 days, or empty before any check ran.
    uptime: String,
    /// Points of the 7-day page-view sparkline, as an SVG polyline `points` value.
    spark: String,
    views_7d: i64,
}

/// SVG polyline points for a small sparkline, 70 by 20 units.
fn sparkline(values: &[i64]) -> String {
    let max = values.iter().copied().max().unwrap_or(0).max(1) as f64;
    let step = if values.len() > 1 {
        70.0 / (values.len() - 1) as f64
    } else {
        0.0
    };
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            format!(
                "{:.1},{:.1}",
                i as f64 * step,
                19.0 - (*v as f64 / max) * 18.0
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

struct RunRow {
    id: i64,
    site: String,
    prompt: String,
    status: &'static str,
    when: String,
}

impl RunRow {
    fn from(run: &Run) -> Self {
        Self {
            id: run.id,
            site: run.site_name.clone(),
            prompt: shorten(&run.prompt, 90),
            status: run.status.as_str(),
            when: ago(run.created_at),
        }
    }
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    chrome: Chrome,
    sites: Vec<SiteCard>,
    runs: Vec<RunRow>,
}

struct ProviderOption {
    id: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "new.html")]
struct NewPage {
    chrome: Chrome,
    providers: Vec<ProviderOption>,
    zone: String,
}

struct ReleaseRow {
    sha: String,
    short: String,
    message: String,
    author: String,
    when: String,
    live: bool,
    changes: String,
}

struct LlmModelOption {
    name: String,
    detail: String,
    checked: bool,
}

/// A site's AI model access, for the site page.
struct LlmView {
    enabled: bool,
    monthly_requests: i64,
    per_minute: i64,
    models: Vec<LlmModelOption>,
    /// For example "12 of 1000 requests this month, $0.0312".
    usage: String,
}

async fn llm_view(state: &AppState, site_id: i64) -> LlmView {
    let llm = state.backend.llm();
    let settings = llm.settings(site_id).await.unwrap_or_default();
    let models = llm
        .available()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|m| LlmModelOption {
            checked: settings.models.contains(&m.name),
            detail: format!(
                "{}{}",
                m.model,
                if m.is_default { ", the default" } else { "" }
            ),
            name: m.name,
        })
        .collect();
    let usage = match llm.usage(site_id).await {
        Ok(u) => format!(
            "{} of {} requests this month{}",
            u.requests,
            settings.monthly_requests,
            u.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default()
        ),
        Err(_) => String::new(),
    };
    LlmView {
        enabled: settings.enabled,
        monthly_requests: settings.monthly_requests,
        per_minute: settings.per_minute,
        models,
        usage,
    }
}

struct BackendLogRow {
    when: String,
    level: String,
    message: String,
    draft: bool,
    background: bool,
}

struct ScheduleRow {
    name: String,
    cron: String,
    next: String,
}

struct JobRow {
    /// "job" or "schedule".
    kind: String,
    name: String,
    status: String,
    when: String,
    /// "attempt 2 of 3", for jobs tried more than once.
    attempts: String,
    error: String,
}

/// What the site page shows about a site's backend.
struct BackendView {
    /// Size of the site's database, for example "48 KB"; empty when there is none.
    database: String,
    logs: Vec<BackendLogRow>,
    schedules: Vec<ScheduleRow>,
    jobs: Vec<JobRow>,
    /// Jobs waiting or running.
    pending_jobs: i64,
}

fn human_bytes(bytes: u64) -> String {
    match bytes {
        b if b >= 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        b if b >= 1024 => format!("{} KB", b / 1024),
        b => format!("{b} bytes"),
    }
}

#[derive(Template)]
#[template(path = "site.html")]
struct SitePage {
    chrome: Chrome,
    name: String,
    title: String,
    url: String,
    published: bool,
    busy: bool,
    /// The run in progress on this site, if any.
    running_run: Option<i64>,
    backend: Option<BackendView>,
    /// Names of the secrets stored for this site's backend.
    secrets: Vec<String>,
    /// Outside hosts the backend may reach, or has asked to.
    backend_hosts: Vec<sw_backend::SiteHostEntry>,
    backends_enabled: bool,
    containerized: bool,
    llm: LlmView,
    zone: String,
    /// Hostnames other than the primary one, with a note for those that redirect.
    other_hostnames: Vec<String>,
    providers: Vec<ProviderOption>,
    runs: Vec<RunRow>,
    releases: Vec<ReleaseRow>,
    uploads: UploadsView,
}

#[derive(Template)]
#[template(path = "run.html")]
struct RunPage {
    chrome: Chrome,
    id: i64,
    site: String,
    prompt: String,
    status: &'static str,
    running: bool,
    model: String,
    /// Unix milliseconds.
    started_at: i64,
    /// Unix milliseconds; 0 while the run is in progress.
    finished_at: i64,
}

struct ProviderRow {
    id: String,
    kind: &'static str,
    model: String,
    base_url: String,
    from_file: bool,
    has_key: bool,
    is_default: bool,
}

struct TlsRow {
    hostname: String,
    ok: bool,
    detail: String,
}

struct PasskeyRow {
    id: i64,
    name: String,
    created: String,
    last_used: String,
}

struct TokenRow {
    id: i64,
    name: String,
    prefix: String,
    created: String,
    last_used: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    chrome: Chrome,
    providers: Vec<ProviderRow>,
    zones: Vec<String>,
    admin_origin: String,
    mode: &'static str,
    max_turns: u32,
    max_run_tokens: u64,
    daily_cap: String,
    spent_today: String,
    tls: Vec<TlsRow>,
    passkeys: Vec<PasskeyRow>,
    can_remove_passkey: bool,
    tokens: Vec<TokenRow>,
    version: &'static str,
    update_available: String,
}

async fn provider_options(state: &AppState) -> Vec<ProviderOption> {
    state
        .providers
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| ProviderOption {
            label: format!("{} ({})", p.config.id, p.config.model),
            id: p.config.id,
            selected: p.is_default,
        })
        .collect()
}

// ---------------------------------------------------------------------- sign-in pages

pub async fn login_page(
    State(state): State<AppState>,
    session: Option<Extension<Session>>,
) -> Response {
    if session.is_some() {
        return Redirect::to("/").into_response();
    }
    render(LoginPage {
        chrome: Chrome::default(),
        has_passkeys: state.auth.has_passkeys().await.unwrap_or(true),
    })
}

pub async fn enroll_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let valid = state.auth.invite_is_valid(&token).await.unwrap_or(false);
    render(EnrollPage {
        chrome: Chrome::default(),
        valid,
        token,
    })
}

// ---------------------------------------------------------------------- home and new

async fn home(State(state): State<AppState>, Extension(session): Extension<Session>) -> Response {
    let sites = state.store.list(false).await.unwrap_or_default();
    let health = sw_metrics::uptime::health(&state.metrics.db, state.metrics.slow_ms)
        .await
        .unwrap_or_default();
    let mut cards = Vec::with_capacity(sites.len());
    for site in sites {
        let site_health = health.get(&site.id);
        let (state_class, state_label) = if state.agent.is_busy(site.name.as_str()) {
            ("busy", "Building")
        } else if site.current_release.is_none() {
            ("draft", "Not published")
        } else {
            match site_health.map(|h| h.status) {
                Some(sw_metrics::uptime::Status::Up) => ("up", "Up"),
                Some(sw_metrics::uptime::Status::Degraded) => ("degraded", "Degraded"),
                Some(sw_metrics::uptime::Status::Down) => ("down", "Down"),
                _ => ("unknown", "Live, not checked yet"),
            }
        };
        let views = site_health
            .map(|h| h.views_7d.clone())
            .unwrap_or_else(|| vec![0; 7]);
        cards.push(SiteCard {
            url: state
                .config
                .domains
                .public_url(&state.config.domains.site_hostname(&site.name)),
            name: site.name.to_string(),
            title: site.title,
            state: state_class,
            state_label,
            deployed: if site.current_release.is_some() {
                ago(site.updated_at)
            } else {
                "never".into()
            },
            uptime: site_health
                .and_then(|h| h.uptime_30d)
                .map(|pct| format!("{pct:.1}% up"))
                .unwrap_or_default(),
            spark: sparkline(&views),
            views_7d: views.iter().sum(),
        });
    }
    let runs = state
        .agent
        .list(None, 8)
        .await
        .unwrap_or_default()
        .iter()
        .map(RunRow::from)
        .collect();
    render(HomePage {
        chrome: Chrome::for_session(&session),
        sites: cards,
        runs,
    })
}

async fn new_page(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
) -> Response {
    render(NewPage {
        chrome: Chrome::for_session(&session),
        providers: provider_options(&state).await,
        zone: state.config.domains.default_zone().to_owned(),
    })
}

#[derive(Debug, Deserialize)]
struct NewForm {
    prompt: String,
    name: String,
    provider: Option<String>,
    /// A checkbox: present when ticked.
    containerize: Option<String>,
}

async fn new_submit(State(state): State<AppState>, Form(form): Form<NewForm>) -> Response {
    let request = RunRequest {
        site: form.name.trim().to_ascii_lowercase(),
        prompt: form.prompt,
        new: true,
        containerize: form.containerize.is_some(),
        provider: form.provider.filter(|p| !p.is_empty()),
    };
    match state.agent.start(request).await {
        Ok(run) => hx_redirect(&format!("/runs/{}", run.id)),
        Err(e) => error_fragment(e.to_string()),
    }
}

// ---------------------------------------------------------------------- site

async fn site_page(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
) -> Response {
    let chrome = Chrome::for_session(&session);
    let Ok(info) = state.store.info(&name).await else {
        return not_found(chrome, "site");
    };
    let name_for_uploads = name.clone();
    let mut releases = Vec::new();
    for release in info.releases.iter().take(15) {
        let changes = match state
            .store
            .release_changes(&info.site.name, &release.sha)
            .await
        {
            Ok(changes) => {
                let count = |status: char| changes.iter().filter(|(s, _)| *s == status).count();
                let parts: Vec<String> = [('A', "added"), ('M', "changed"), ('D', "removed")]
                    .iter()
                    .filter(|(status, _)| count(*status) > 0)
                    .map(|(status, word)| format!("{} {word}", count(*status)))
                    .collect();
                if parts.is_empty() {
                    "no file changes".to_owned()
                } else {
                    format!("{} files: {}", changes.len(), parts.join(", "))
                }
            }
            Err(_) => String::new(),
        };
        releases.push(ReleaseRow {
            short: release.sha.chars().take(8).collect(),
            sha: release.sha.clone(),
            message: release.message.clone(),
            author: release.author.clone(),
            when: ago(release.created_at),
            live: Some(release.id) == info.site.current_release,
            changes,
        });
    }
    let recent = state.agent.list(Some(&name), 5).await.unwrap_or_default();
    let running_run = recent
        .iter()
        .find(|run| run.status == RunStatus::Running)
        .map(|run| run.id);
    let runs = recent.iter().map(RunRow::from).collect();
    let other_hostnames = info
        .hostnames
        .iter()
        .filter(|h| !h.is_primary)
        .map(|h| {
            if h.redirect {
                format!("{} (redirects)", h.hostname)
            } else {
                h.hostname.clone()
            }
        })
        .collect();
    let has_backend = state
        .store
        .routes()
        .sites()
        .iter()
        .any(|route| route.site_id == info.site.id && route.backend);
    let (schedules, jobs, pending_jobs) = if has_backend {
        let schedules = state
            .backend
            .schedules(info.site.id)
            .await
            .into_iter()
            .map(|s| ScheduleRow {
                name: s.name,
                cron: s.cron,
                next: s.next_at.map(until).unwrap_or_default(),
            })
            .collect();
        let jobs = state
            .backend
            .jobs(info.site.id, 20)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|j| JobRow {
                when: match (j.status.as_str(), j.finished_at) {
                    ("queued", _) if j.run_at > now_ms() => until(j.run_at),
                    (_, Some(finished)) => ago(finished),
                    _ => ago(j.created_at),
                },
                attempts: if j.kind == "job" && j.attempts > 1 {
                    format!("attempt {} of {}", j.attempts, j.max_attempts)
                } else {
                    String::new()
                },
                kind: j.kind,
                name: j.name,
                status: j.status,
                error: j.error.unwrap_or_default(),
            })
            .collect();
        let pending = state.backend.pending_jobs(info.site.id).await.unwrap_or(0);
        (schedules, jobs, pending)
    } else {
        (Vec::new(), Vec::new(), 0)
    };
    let backend = has_backend.then(|| BackendView {
        schedules,
        jobs,
        pending_jobs,
        database: state
            .backend
            .database_size(&info.site.name)
            .map(human_bytes)
            .unwrap_or_default(),
        logs: state
            .backend
            .logs_redacted(info.site.id, 30)
            .into_iter()
            .rev()
            .map(|line| BackendLogRow {
                when: ago(line.ts),
                level: line.level,
                message: line.message,
                draft: line.draft,
                background: line.background,
            })
            .collect(),
    });
    render(SitePage {
        chrome,
        secrets: state.backend.secret_names(info.site.id).unwrap_or_default(),
        backend_hosts: state.backend.hosts(info.site.id).await.unwrap_or_default(),
        backends_enabled: state.backend.enabled(),
        containerized: state.store.containerized(&name).await.unwrap_or(false),
        llm: llm_view(&state, info.site.id).await,
        backend,
        running_run,
        zone: state.config.domains.default_zone().to_owned(),
        other_hostnames,
        busy: state.agent.is_busy(&name),
        name,
        title: info.site.title,
        url: info.url,
        published: info.site.current_release.is_some(),
        providers: provider_options(&state).await,
        runs,
        releases,
        uploads: uploads_view(&state, &name_for_uploads, None).await,
    })
}

// ---------------------------------------------------------------------- uploads

/// Uploads have no size limit, but the browser asks for a confirmation above this.
pub const UPLOAD_WARN_BYTES: u64 = 15 * 1024 * 1024;
/// The largest part of a file `upload.js` sends in one request (it sends 8 MB), so no
/// request comes near the limits of proxies (nginx's 25 MB in the examples, Cloudflare's
/// 100 MB).
const UPLOAD_PART_MAX: usize = 16 * 1024 * 1024;

struct UploadRow {
    /// The name inside `uploads/`.
    file: String,
    path: String,
    size: String,
    /// "1200×800", or empty when not an image.
    dims: String,
    is_image: bool,
    /// The extension, shown in place of a thumbnail: "pdf".
    ext: String,
    /// The file is in the live release.
    live: bool,
    /// Its public address on the site.
    url: String,
}

/// The site page's uploads section, also sent alone after a change.
struct UploadsView {
    items: Vec<UploadRow>,
    /// A message about the last change; empty for none.
    message: String,
    error: bool,
    /// Files larger than this need a confirmation in the browser.
    warn_bytes: u64,
    /// The `accept` attribute of the file picker.
    accept: String,
}

#[derive(Template)]
#[template(path = "uploads.html")]
struct UploadsFragment {
    name: String,
    uploads: UploadsView,
}

async fn uploads_view(
    state: &AppState,
    name: &str,
    message: Option<(bool, String)>,
) -> UploadsView {
    let limits = &state.config.sites;
    let site = state.store.get(name).await.ok();
    let base = state
        .store
        .info(name)
        .await
        .map(|i| i.url)
        .unwrap_or_default();
    let live_root = site.as_ref().and_then(|site| {
        state
            .store
            .routes()
            .sites()
            .iter()
            .find(|route| route.site_id == site.id)
            .map(|route| route.root.clone())
    });
    let items = state
        .store
        .list_uploads(name)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|u| UploadRow {
            file: u.file_name().to_owned(),
            dims: match (u.width, u.height) {
                (Some(w), Some(h)) => format!("{w}×{h}"),
                _ => String::new(),
            },
            ext: u.path.rsplit('.').next().unwrap_or_default().to_owned(),
            is_image: matches!(
                u.path.rsplit('.').next(),
                Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico")
            ),
            live: live_root
                .as_ref()
                .is_some_and(|root| root.join(&u.path).is_file()),
            url: format!("{base}/{}", u.path),
            size: human_bytes(u.size),
            path: u.path,
        })
        .collect();
    let accept = sw_sites::confine::UPLOAD_EXTENSIONS
        .iter()
        .filter(|e| limits.allowed_extensions.iter().any(|a| a == *e))
        .map(|e| format!(".{e}"))
        .collect::<Vec<_>>()
        .join(",");
    let (error, message) = message.unwrap_or_default();
    UploadsView {
        items,
        message,
        error,
        warn_bytes: UPLOAD_WARN_BYTES,
        accept,
    }
}

async fn render_uploads(
    state: &AppState,
    name: String,
    message: Option<(bool, String)>,
) -> Response {
    let uploads = uploads_view(state, &name, message).await;
    render(UploadsFragment { name, uploads })
}

async fn uploads_fragment(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if state.store.get(&name).await.is_err() {
        return (StatusCode::NOT_FOUND, "no such site").into_response();
    }
    render_uploads(&state, name, None).await
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    /// The file's name on the owner's device.
    name: String,
    /// When set, the file was sent in parts under this id and the body is empty.
    upload: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PartQuery {
    offset: u64,
}

/// Receive one part of a file sent in parts by `upload.js`. Answers with the number of
/// bytes received so far; on 409 the body is the length the server already has.
async fn upload_part(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    Query(query): Query<PartQuery>,
    body: axum::body::Body,
) -> Response {
    if state.store.get(&name).await.is_err() {
        return (StatusCode::NOT_FOUND, "no such site").into_response();
    }
    let path = match state.store.upload_part_path(&id) {
        Ok(path) => path,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match crate::upload::append_part(&path, query.offset, body).await {
        Ok(len) => len.to_string().into_response(),
        Err(Ok(len)) => (StatusCode::CONFLICT, len.to_string()).into_response(),
        Err(Err(e @ crate::upload::StageError::Disk(_))) => {
            tracing::error!(error = %e, "upload part could not be stored");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(Err(e)) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Store one uploaded file. The body is the file itself, sent by `upload.js`. Answers
/// with the refreshed uploads section, or an error message as plain text.
async fn upload_put(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<UploadQuery>,
    body: axum::body::Body,
) -> Response {
    if state.store.get(&name).await.is_err() {
        return (StatusCode::NOT_FOUND, "no such site").into_response();
    }
    let staged = match &query.upload {
        // Sent in parts: the file is already on disk.
        Some(id) => match state.store.upload_part_path(id) {
            Ok(path) if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) => Ok(path),
            Ok(_) => Err(crate::upload::StageError::Empty),
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        },
        None => crate::upload::stage(&state.store, body).await,
    };
    let staged = match staged {
        Ok(staged) => staged,
        Err(e @ crate::upload::StageError::Disk(_)) => {
            tracing::error!(error = %e, "upload could not be stored");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match state
        .store
        .add_upload_file(&name, &query.name, staged, Author::User)
        .await
    {
        Ok(outcome) => {
            let verb = if outcome.replaced {
                "Replaced"
            } else {
                "Uploaded"
            };
            let path = &outcome.upload.path;
            let message = match &outcome.pending {
                None => format!(
                    "{verb} {path}; it is live. Mention it in a prompt to use it, for example \"use {path} as the logo\"."
                ),
                Some(reason) => format!("{verb} {path}. Not live yet: {reason}."),
            };
            render_uploads(&state, name, Some((false, message))).await
        }
        Err(e) => {
            let status = if e.is_user_error() {
                StatusCode::BAD_REQUEST
            } else {
                tracing::error!(error = %e, "upload failed");
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, e.to_string()).into_response()
        }
    }
}

async fn upload_delete(
    State(state): State<AppState>,
    Path((name, file)): Path<(String, String)>,
) -> Response {
    let message = match state.store.remove_upload(&name, &file, Author::User).await {
        Ok(outcome) => match outcome.pending {
            None => (false, format!("Removed {}.", outcome.upload.path)),
            Some(reason) => (
                false,
                format!("Removed {}. Not live yet: {reason}.", outcome.upload.path),
            ),
        },
        Err(sw_sites::SiteError::Validation { problems, .. }) => (
            true,
            format!(
                "Kept uploads/{file}: the site would not pass its checks without it ({}). Ask the agent to stop using it first.",
                problems.join("; ")
            ),
        ),
        Err(e) => (true, e.to_string()),
    };
    render_uploads(&state, name, Some(message)).await
}

/// Show an uploaded file from the working tree, for thumbnails in the portal. The file
/// is the owner's but may come from anywhere, so it is sandboxed: an SVG with a script
/// in it cannot run on the admin origin.
async fn upload_preview(
    State(state): State<AppState>,
    Path((name, file)): Path<(String, String)>,
) -> Response {
    let path = format!("{}/{file}", sw_sites::confine::UPLOADS_DIR);
    let Ok(data) = state.store.read_file(&name, &path).await else {
        return (StatusCode::NOT_FOUND, "no such file").into_response();
    };
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    (
        [
            (header::CONTENT_TYPE, mime.essence_str().to_owned()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; sandbox".to_owned(),
            ),
            (header::CACHE_CONTROL, "private, no-cache".to_owned()),
        ],
        data,
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct PromptForm {
    prompt: String,
    provider: Option<String>,
}

async fn site_prompt(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<PromptForm>,
) -> Response {
    let request = RunRequest {
        site: name,
        prompt: form.prompt,
        new: false,
        containerize: false,
        provider: form.provider.filter(|p| !p.is_empty()),
    };
    match state.agent.start(request).await {
        Ok(run) => hx_redirect(&format!("/runs/{}", run.id)),
        Err(e) => error_fragment(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct RollbackForm {
    release: String,
}

async fn site_rollback(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<RollbackForm>,
) -> Response {
    if state.agent.is_busy(&name) {
        return error_fragment("a run is in progress on this site; wait for it or cancel it first");
    }
    match state
        .store
        .rollback(&name, Some(&form.release), Author::User)
        .await
    {
        Ok(_) => hx_refresh(),
        Err(e) => error_fragment(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct DeleteForm {
    confirm: String,
}

async fn site_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<DeleteForm>,
) -> Response {
    if form.confirm.trim() != name {
        return error_fragment(format!("type {name} exactly to confirm"));
    }
    if state.agent.is_busy(&name) {
        return error_fragment("a run is in progress on this site; wait for it or cancel it first");
    }
    match state.store.delete(&name, Author::User).await {
        Ok(site) => {
            state.backend.release(site.id, &site.name).await;
            hx_redirect("/")
        }
        Err(e) => error_fragment(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct RenameForm {
    title: String,
    name: String,
    /// A checkbox: present when ticked.
    redirect: Option<String>,
}

/// Change a site's title, its name (and with it its address), or both.
async fn site_rename(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<RenameForm>,
) -> Response {
    let site = match state.store.get(&name).await {
        Ok(site) => site,
        Err(e) => return error_fragment(e.to_string()),
    };
    let new_name = form.name.trim().to_ascii_lowercase();
    let renaming = new_name != name;
    if renaming && state.agent.is_busy(&name) {
        return error_fragment("a run is in progress on this site; wait for it or cancel it first");
    }
    if renaming {
        // The backend's worker and database connection must let go of the directory.
        state.backend.release(site.id, &site.name).await;
    }
    // Rename first: it is the step that can be refused.
    if renaming
        && let Err(e) = state
            .store
            .rename(&name, &new_name, form.redirect.is_some(), Author::User)
            .await
    {
        return error_fragment(e.to_string());
    }
    if form.title.trim() != site.title
        && let Err(e) = state
            .store
            .set_title(&new_name, &form.title, Author::User)
            .await
    {
        return error_fragment(e.to_string());
    }
    hx_redirect(&format!("/sites/{new_name}"))
}

#[derive(Deserialize)]
struct SecretForm {
    name: String,
    value: String,
}

/// Store a secret for a site's backend. The value is never shown again.
async fn secret_save(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<SecretForm>,
) -> Response {
    let site = match state.store.get(&name).await {
        Ok(site) => site,
        Err(e) => return error_fragment(e.to_string()),
    };
    match state
        .backend
        .set_secret(site.id, form.name.trim(), form.value.trim())
    {
        Ok(()) => hx_refresh(),
        Err(e) => error_fragment(e),
    }
}

async fn secret_delete(
    State(state): State<AppState>,
    Path((name, secret)): Path<(String, String)>,
) -> Response {
    let site = match state.store.get(&name).await {
        Ok(site) => site,
        Err(e) => return error_fragment(e.to_string()),
    };
    match state.backend.remove_secret(site.id, &secret) {
        Ok(_) => hx_refresh(),
        Err(e) => error_fragment(e),
    }
}

#[derive(Debug, Deserialize)]
struct HostForm {
    host: String,
}

/// Allow a site's backend to send requests to a host.
async fn host_approve(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<HostForm>,
) -> Response {
    let site = match state.store.get(&name).await {
        Ok(site) => site,
        Err(e) => return error_fragment(e.to_string()),
    };
    match state.backend.approve_host(site.id, &form.host).await {
        Ok(_) => hx_refresh(),
        Err(e) => error_fragment(e),
    }
}

async fn host_delete(
    State(state): State<AppState>,
    Path((name, host)): Path<(String, String)>,
) -> Response {
    let site = match state.store.get(&name).await {
        Ok(site) => site,
        Err(e) => return error_fragment(e.to_string()),
    };
    match state.backend.remove_host(site.id, &host).await {
        Ok(_) => hx_refresh(),
        Err(e) => error_fragment(e),
    }
}

/// The AI access form. Checkboxes arrive as repeated `model` fields, so the body is read
/// as pairs.
async fn llm_save(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Response {
    let site = match state.store.get(&name).await {
        Ok(site) => site,
        Err(e) => return error_fragment(e.to_string()),
    };
    let value = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim().to_owned())
    };
    let number =
        |key: &str, default: i64| value(key).and_then(|v| v.parse().ok()).unwrap_or(default);
    let settings = sw_backend::LlmSettings {
        enabled: value("enabled").is_some(),
        models: fields
            .iter()
            .filter(|(k, _)| k == "model")
            .map(|(_, v)| v.clone())
            .collect(),
        monthly_requests: number("monthly_requests", 1000),
        per_minute: number("per_minute", 20),
    };
    match state.backend.llm().set_settings(site.id, &settings).await {
        Ok(()) => hx_refresh(),
        Err(e) => error_fragment(e),
    }
}

#[derive(Debug, Deserialize)]
struct ContainerizeForm {
    on: String,
}

/// Turn containerizing on or off for a site. Turning it on leads to the container
/// page, which shows the setup guide for the site as it is now.
async fn site_containerize(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<ContainerizeForm>,
) -> Response {
    let on = form.on == "1";
    match state.store.set_containerized(&name, on, Author::User).await {
        Ok(()) if on => hx_redirect(&format!("/sites/{name}/container")),
        Ok(()) => hx_refresh(),
        Err(e) => error_fragment(e.to_string()),
    }
}

#[derive(Template)]
#[template(path = "container.html")]
struct ContainerPage {
    chrome: Chrome,
    name: String,
    title: String,
    release: String,
    image_name: String,
    has_data: bool,
    /// The setup guide, rendered from Markdown by our own renderer.
    guide_html: String,
}

async fn container_page(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
) -> Response {
    let chrome = Chrome::for_session(&session);
    let Ok(site) = state.store.get(&name).await else {
        return not_found(chrome, "site");
    };
    match crate::container::describe(&state, &name).await {
        Ok(package) => render(ContainerPage {
            chrome,
            title: site.title,
            release: package.manifest.release.chars().take(8).collect(),
            image_name: package.file_name.clone(),
            has_data: state.backend.database_size(&site.name).is_some(),
            guide_html: crate::markdown::render(&crate::container::guide(&package)),
            name,
        }),
        Err(message) => render(MessagePage {
            chrome,
            title: "Nothing to package yet".into(),
            message,
        }),
    }
}

async fn container_part(
    State(state): State<AppState>,
    Path((name, part)): Path<(String, String)>,
) -> Response {
    crate::api::container_download(&state, &name, &part)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn site_export(
    State(state): State<AppState>,
    Path((name, format)): Path<(String, String)>,
) -> Response {
    let result = match sw_sites::ExportFormat::parse(&format) {
        Ok(format) => crate::api::export_response(&state, &name, format).await,
        Err(e) => Err(e.into()),
    };
    result.unwrap_or_else(IntoResponse::into_response)
}

// ---------------------------------------------------------------------- dashboard

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    chrome: Chrome,
    name: String,
    title: String,
    has_ai: bool,
    updated: String,
    busy: bool,
}

async fn dashboard_page(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(name): Path<String>,
) -> Response {
    let chrome = Chrome::for_session(&session);
    let Ok(site) = state.store.get(&name).await else {
        return not_found(chrome, "site");
    };
    let ai = sw_metrics::dashboard::load(&state.metrics.db, site.id)
        .await
        .ok()
        .flatten();
    render(DashboardPage {
        chrome,
        busy: state.agent.is_busy(&name),
        name,
        title: site.title,
        has_ai: ai.is_some(),
        updated: ai.map(|(_, at)| ago(at)).unwrap_or_default(),
    })
}

#[derive(Debug, Deserialize)]
struct DashboardQuery {
    days: Option<i64>,
}

/// The data behind a site's dashboards: every panel with its rows.
async fn dashboard_data(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<DashboardQuery>,
) -> Response {
    let Ok(site) = state.store.get(&name).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let days = query.days.unwrap_or(30).clamp(1, 365);
    let sandbox = &state.metrics.sandbox;
    // Panels with `"source": "site"` read this site's own backend database, read-only.
    let site_data = sw_agent::SiteQueries::new(state.backend.clone(), site.name.clone());
    let site_data: Option<&dyn sw_metrics::dashboard::SiteData> = Some(&site_data);
    let standard = sw_metrics::dashboard::render(
        sandbox,
        site_data,
        site.id,
        &sw_metrics::dashboard::default_spec(),
        days,
    )
    .await;
    let custom = match sw_metrics::dashboard::load(&state.metrics.db, site.id)
        .await
        .ok()
        .flatten()
    {
        Some((spec, _)) => {
            sw_metrics::dashboard::render(sandbox, site_data, site.id, &spec, days).await
        }
        None => Vec::new(),
    };
    axum::Json(serde_json::json!({ "days": days, "standard": standard, "custom": custom }))
        .into_response()
}

/// Ask the agent to (re)design the site's dashboard.
async fn dashboard_regenerate(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let request = RunRequest {
        site: name,
        prompt: "Design the analytics dashboard for this site. Load the design-dashboard skill, look at what the site does, \
                 and save a dashboard with panels specific to it. Do not change the site's files."
            .into(),
        new: false,
        containerize: false,
        provider: None,
    };
    match state.agent.start(request).await {
        Ok(run) => hx_redirect(&format!("/runs/{}", run.id)),
        Err(e) => error_fragment(e.to_string()),
    }
}

// ---------------------------------------------------------------------- runs

async fn run_page(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
) -> Response {
    let chrome = Chrome::for_session(&session);
    let Ok(run) = state.agent.get(id).await else {
        return not_found(chrome, "run");
    };
    render(RunPage {
        chrome,
        id,
        site: run.site_name,
        prompt: run.prompt,
        status: run.status.as_str(),
        running: run.status == RunStatus::Running,
        model: format!("{} / {}", run.provider, run.model),
        started_at: run.created_at,
        finished_at: run.finished_at.unwrap_or(0),
    })
}

async fn run_events(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let subscription = state
        .agent
        .subscribe(id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Sse::new(run_event_stream(subscription)).keep_alive(KeepAlive::default()))
}

async fn run_cancel(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    if state.agent.cancel(id) {
        ok_fragment("Cancelling…")
    } else {
        error_fragment("this run has already finished")
    }
}

// ---------------------------------------------------------------------- settings

async fn settings_page(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
) -> Response {
    let when = |ts: Option<i64>| ts.map(ago).unwrap_or_else(|| "never".into());
    let providers = state
        .providers
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| ProviderRow {
            id: p.config.id,
            kind: p.config.kind.as_str(),
            model: p.config.model,
            base_url: p.config.base_url.unwrap_or_default(),
            from_file: p.from_file,
            has_key: p.has_key,
            is_default: p.is_default,
        })
        .collect();
    let passkeys: Vec<PasskeyRow> = state
        .auth
        .list_passkeys()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| PasskeyRow {
            id: p.id,
            name: p.name,
            created: ago(p.created_at),
            last_used: when(p.last_used_at),
        })
        .collect();
    let tokens = state
        .auth
        .list_api_tokens()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| TokenRow {
            id: t.id,
            name: t.name,
            prefix: t.prefix,
            created: ago(t.created_at),
            last_used: when(t.last_used_at),
        })
        .collect();
    let now = now_ms() / 1000;
    let tls = state
        .tls
        .get()
        .map(|manager| manager.status())
        .unwrap_or_default()
        .into_iter()
        .map(|host| match (host.certificate, host.last_error) {
            (Some(cert), error) => TlsRow {
                hostname: host.hostname,
                ok: cert.remaining(now) > 0,
                detail: match error {
                    Some(error) => format!(
                        "certificate valid for {} more days; renewal failed: {error}",
                        cert.remaining(now) / 86_400
                    ),
                    None => format!(
                        "certificate valid for {} more days",
                        cert.remaining(now) / 86_400
                    ),
                },
            },
            (None, Some(error)) => TlsRow {
                hostname: host.hostname,
                ok: false,
                detail: error,
            },
            (None, None) => TlsRow {
                hostname: host.hostname,
                ok: false,
                detail: "waiting for a certificate".into(),
            },
        })
        .collect();
    let spent = state.agent.spent_today().await.unwrap_or(0.0);
    render(SettingsPage {
        chrome: Chrome::for_session(&session),
        providers,
        zones: state.config.domains.zones.clone(),
        admin_origin: state.admin_origin.to_string(),
        mode: match state.config.server.mode {
            sw_core::config::ServeMode::Proxy => "proxy",
            sw_core::config::ServeMode::Standalone => "standalone",
        },
        max_turns: state.config.agent.max_turns,
        max_run_tokens: state.config.agent.max_run_tokens,
        daily_cap: format!("{:.2}", state.config.agent.daily_spend_cap_usd),
        spent_today: format!("{spent:.2}"),
        tls,
        can_remove_passkey: passkeys.len() > 1,
        passkeys,
        tokens,
        version: env!("CARGO_PKG_VERSION"),
        update_available: state
            .update_available
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default(),
    })
}

#[derive(Debug, Deserialize)]
struct ProviderForm {
    id: String,
    kind: String,
    base_url: Option<String>,
    model: String,
    auth: Option<String>,
    api_key: Option<String>,
}

async fn provider_save(State(state): State<AppState>, Form(form): Form<ProviderForm>) -> Response {
    let kind = match form.kind.as_str() {
        "openai_compat" => ProviderKind::OpenaiCompat,
        "anthropic" => ProviderKind::Anthropic,
        "gemini" => ProviderKind::Gemini,
        "copilot" => ProviderKind::Copilot,
        _ => return error_fragment("choose a provider kind"),
    };
    let provider = ProviderConfig {
        id: form.id.trim().to_owned(),
        kind,
        base_url: form
            .base_url
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty()),
        model: form.model.trim().to_owned(),
        auth: if form.auth.as_deref() == Some("api_key_header") {
            AuthStyle::ApiKeyHeader
        } else {
            AuthStyle::Bearer
        },
        key_ref: None,
        timeout_secs: 300,
        max_retries: 3,
        max_output_tokens: None,
        extra_body: toml::Table::new(),
    };
    match state
        .providers
        .save(provider, form.api_key.as_deref())
        .await
    {
        Ok(()) => hx_refresh(),
        Err(e) => error_fragment(e),
    }
}

async fn provider_test(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    use sw_agent::ProviderSource;
    let provider = match state.providers.provider(Some(&id)).await {
        Ok(provider) => provider,
        Err(e) => return error_fragment(e),
    };
    let result = sw_llm::test_connection(provider.as_ref()).await;
    if result.ok {
        ok_fragment(format!(
            "Works: {} ({} ms)",
            result.detail, result.latency_ms
        ))
    } else {
        error_fragment(format!(
            "Failed after {} ms: {}",
            result.latency_ms, result.detail
        ))
    }
}

async fn provider_delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.providers.delete(&id).await {
        Ok(()) => hx_refresh(),
        Err(e) => error_fragment(e),
    }
}

/// The GitHub Copilot part of Settings: where the sign-in stands and what to do next.
/// `note` is an extra line (the answer to "who is signed in?").
fn copilot_fragment(note: Option<String>) -> Response {
    use sw_llm::copilot::SignIn;
    let installed = sw_llm::copilot::runtime_path().is_some_and(|p| p.is_file());
    if !installed {
        return Html(
            "<p class=\"muted\">The Copilot runtime is not installed. On the server, run \
             <code>sitewright copilot install</code>, then reload this page.</p>"
                .to_owned(),
        )
        .into_response();
    }
    let button = |label: &str, path: &str, class: &str| {
        format!(
            "<button type=\"button\" class=\"{class}\" hx-post=\"{path}\" hx-target=\"#copilot\" \
             hx-disabled-elt=\"this\" hx-indicator=\"this\">{label}</button>"
        )
    };
    let sign_in = button("Sign in with GitHub", "/settings/copilot/login", "");
    let who = button("Who is signed in?", "/settings/copilot/account", "");
    let models = "<button type=\"button\" hx-post=\"/settings/copilot/models\" \
                  hx-target=\"#copilot-models\" hx-disabled-elt=\"this\" \
                  hx-indicator=\"this\">Show my models</button>";
    let models_slot = "<div id=\"copilot-models\" aria-live=\"polite\"></div>";
    let sign_out = button("Sign out", "/settings/copilot/logout", "danger");
    let note = note
        .map(|n| format!("<p class=\"msg\" role=\"status\">{}</p>", esc(&n)))
        .unwrap_or_default();
    let body = match sw_llm::copilot::sign_in_state() {
        SignIn::Waiting {
            user_code,
            verification_uri,
        } => {
            let link = if verification_uri.starts_with("https://") {
                esc(&verification_uri)
            } else {
                "https://github.com/login/device".into()
            };
            // Poll until the owner has entered the code.
            format!(
                "<div hx-get=\"/settings/copilot\" hx-trigger=\"every 3s\" hx-target=\"#copilot\">\
                 <p>Open <a href=\"{link}\" target=\"_blank\" rel=\"noopener noreferrer\">{link}</a> \
                 and enter this code:</p>\
                 <p><strong class=\"code\" style=\"font-size:1.6rem;letter-spacing:.1em\">{}</strong></p>\
                 <p class=\"muted\">Waiting for GitHub. This page updates by itself.</p></div>",
                esc(&user_code)
            )
        }
        SignIn::Done { login } => format!(
            "<p class=\"msg ok\" role=\"status\">Signed in{}.</p>\
             <p class=\"muted\">Now add a provider below with the kind \"GitHub Copilot\" and no key.</p>\
             <div class=\"row\">{models}{who}{sign_out}</div>{models_slot}",
            login
                .map(|l| format!(" as {}", esc(&l)))
                .unwrap_or_default()
        ),
        SignIn::Failed { message } => format!(
            "<p class=\"msg error\" role=\"alert\">Sign-in failed: {}</p><div class=\"row\">{sign_in}</div>",
            esc(&message)
        ),
        SignIn::Idle => format!(
            "<p class=\"muted\">Use the models of your GitHub Copilot subscription. You get a short \
             code to enter at github.com; there is no token to create.</p>\
             <div class=\"row\">{sign_in}{models}{who}{sign_out}</div>{models_slot}"
        ),
    };
    Html(format!("{note}{body}")).into_response()
}

async fn copilot_panel() -> Response {
    copilot_fragment(None)
}

async fn copilot_login() -> Response {
    match sw_llm::copilot::sign_in_start().await {
        Ok(_) => copilot_fragment(None),
        Err(e) => copilot_fragment(Some(format!("Could not start the sign-in: {e}"))),
    }
}

async fn copilot_account() -> Response {
    let note = match sw_llm::copilot::account().await {
        Ok(Some(login)) => format!("Signed in as {login}."),
        Ok(None) => "Nobody is signed in.".to_owned(),
        Err(e) => format!("Could not ask the runtime: {e}"),
    };
    copilot_fragment(Some(note))
}

#[derive(Debug, Deserialize)]
struct CopilotModelsForm {
    refresh: Option<String>,
}

/// The account's models as a searchable dropdown, with a form that adds the chosen one
/// as a provider.
async fn copilot_models(Form(form): Form<CopilotModelsForm>) -> Response {
    let models = match sw_llm::copilot::available_models(form.refresh.is_some()).await {
        Ok(models) => models,
        Err(e) => return error_fragment(format!("Could not list the models: {e}")),
    };
    if models.is_empty() {
        return error_fragment("This account has no Copilot models.");
    }
    // The script in app.js filters the dropdown from this list as you type.
    let options: String = models
        .iter()
        .map(|m| {
            format!(
                "<option value=\"{}\">{} ({})</option>",
                esc(&m.id),
                esc(&m.name),
                esc(&m.id)
            )
        })
        .collect();
    Html(format!(
        "<form class=\"model-picker\" hx-post=\"/settings/providers\" hx-target=\"find .status\" \
         hx-disabled-elt=\"find button[type=submit]\">\
         <input type=\"hidden\" name=\"kind\" value=\"copilot\">\
         <label for=\"cm-search\">Search {count} models</label>\
         <input id=\"cm-search\" type=\"search\" data-filter-select=\"cm-model\" autocomplete=\"off\" \
         autocapitalize=\"none\" spellcheck=\"false\" placeholder=\"Type part of a name, for example gpt\">\
         <label for=\"cm-model\">Model</label>\
         <select id=\"cm-model\" name=\"model\" required>{options}</select>\
         <p class=\"hint\" data-filter-count=\"cm-model\"></p>\
         <label for=\"cm-id\">Name it</label>\
         <input id=\"cm-id\" name=\"id\" required maxlength=\"40\" pattern=\"[a-z0-9_-]+\" autocapitalize=\"none\" \
         spellcheck=\"false\" data-id-from=\"cm-model\" placeholder=\"copilot-gpt\">\
         <p class=\"hint\">The name you pick models by when building a site. Lowercase letters, digits, - and _.</p>\
         <div class=\"row\"><button type=\"submit\">Add this model</button>\
         <button type=\"button\" hx-post=\"/settings/copilot/models\" hx-vals='{{\"refresh\":\"1\"}}' \
         hx-target=\"#copilot-models\" hx-disabled-elt=\"this\">Refresh list</button></div>\
         <div class=\"status\" aria-live=\"polite\"></div></form>",
        count = models.len()
    ))
    .into_response()
}

async fn copilot_logout() -> Response {
    let note = match sw_llm::copilot::sign_out() {
        Ok(()) => "Signed out. To also revoke access, remove the Copilot app under GitHub Settings, Applications.".to_owned(),
        Err(e) => format!("Could not remove the stored sign-in: {e}"),
    };
    copilot_fragment(Some(note))
}

#[derive(Debug, Deserialize)]
struct DnsForm {
    hostname: String,
}

async fn dns_check(State(state): State<AppState>, Form(form): Form<DnsForm>) -> Response {
    let port = match (
        state.config.domains.public_scheme,
        state.config.domains.public_port,
    ) {
        (sw_core::config::Scheme::Http, Some(port)) => port,
        _ => 80,
    };
    let check = state.domains.check(form.hostname.trim(), port).await;
    let addresses = check
        .addresses
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let summary = if addresses.is_empty() {
        format!("{}: {}", check.hostname, check.detail)
    } else {
        format!(
            "{} resolves to {addresses}: {}",
            check.hostname, check.detail
        )
    };
    if check.reaches_this_server {
        ok_fragment(summary)
    } else {
        error_fragment(summary)
    }
}

async fn passkey_delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state.auth.delete_passkey(id).await {
        Ok(()) => hx_refresh(),
        Err(e) => error_fragment(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct TokenForm {
    name: String,
}

async fn token_create(State(state): State<AppState>, Form(form): Form<TokenForm>) -> Response {
    match state.auth.create_api_token(&form.name).await {
        Ok(token) => Html(format!(
            "<div class=\"msg ok\" role=\"status\"><p>Copy this token now. It is shown only once.</p>\
             <p><code class=\"token\">{}</code></p><p><a href=\"/settings\">Done</a></p></div>",
            esc(&token)
        ))
        .into_response(),
        Err(e) => error_fragment(e.to_string()),
    }
}

async fn token_delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state.auth.delete_api_token(id).await {
        Ok(()) => hx_refresh(),
        Err(e) => error_fragment(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_escape_html() {
        assert_eq!(
            esc("<b a=\"1\">&'</b>"),
            "&lt;b a=&quot;1&quot;&gt;&amp;&#39;&lt;/b&gt;"
        );
    }

    #[test]
    fn relative_times_read_naturally() {
        let now = now_ms();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 61_000), "1 minute ago");
        assert_eq!(ago(now - 2 * 3_600_000), "2 hours ago");
        assert_eq!(ago(now - 3 * 86_400_000), "3 days ago");
        assert_eq!(shorten("a  b\nc", 10), "a b c");
        assert_eq!(shorten("abcdefgh", 4), "abcd…");
        assert_eq!(sparkline(&[0, 5, 10]), "0.0,19.0 35.0,10.0 70.0,1.0");
        assert_eq!(sparkline(&[0, 0]), "0.0,19.0 70.0,19.0");
    }
}
