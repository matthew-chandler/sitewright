//! The admin UI's pages and form handlers. Pages are server-rendered templates; forms
//! are submitted by htmx and answer with a redirect header or a small HTML fragment.

use std::convert::Infallible;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
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

#[derive(Template)]
#[template(path = "site.html")]
struct SitePage {
    chrome: Chrome,
    name: String,
    title: String,
    url: String,
    published: bool,
    busy: bool,
    providers: Vec<ProviderOption>,
    runs: Vec<RunRow>,
    releases: Vec<ReleaseRow>,
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
    passkeys: Vec<PasskeyRow>,
    can_remove_passkey: bool,
    tokens: Vec<TokenRow>,
    version: &'static str,
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
}

async fn new_submit(State(state): State<AppState>, Form(form): Form<NewForm>) -> Response {
    let request = RunRequest {
        site: form.name.trim().to_ascii_lowercase(),
        prompt: form.prompt,
        new: true,
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
    let runs = state
        .agent
        .list(Some(&name), 5)
        .await
        .unwrap_or_default()
        .iter()
        .map(RunRow::from)
        .collect();
    render(SitePage {
        chrome,
        busy: state.agent.is_busy(&name),
        name,
        title: info.site.title,
        url: info.url,
        published: info.site.current_release.is_some(),
        providers: provider_options(&state).await,
        runs,
        releases,
    })
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
        Ok(_) => hx_redirect("/"),
        Err(e) => error_fragment(e.to_string()),
    }
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
    let standard = sw_metrics::dashboard::render(
        sandbox,
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
        Some((spec, _)) => sw_metrics::dashboard::render(sandbox, site.id, &spec, days).await,
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
        can_remove_passkey: passkeys.len() > 1,
        passkeys,
        tokens,
        version: env!("CARGO_PKG_VERSION"),
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
