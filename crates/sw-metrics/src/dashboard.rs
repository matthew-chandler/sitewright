//! Dashboards: a fixed default one for every site, and an AI-designed one per site.
//!
//! A dashboard is data, not code: a list of panels, each a read-only SQL query plus a
//! Vega-Lite chart description. The AI only chooses *which* charts fit a site. The server
//! validates the spec, runs every query in the [`Sandbox`](crate::Sandbox) and injects
//! the rows itself, so a spec can neither change data nor load anything from elsewhere.
//!
//! A panel reads from one of two places ([`Source`]): the site's traffic metrics, or the
//! site's own backend database. This crate knows nothing about backends, so the second
//! is reached through the [`SiteData`] trait.

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sw_core::time::{MS_PER_DAY, now_ms};

use crate::sandbox::{MAX_SQL_BYTES, QueryResult, Sandbox};

/// Most panels in one dashboard.
pub const MAX_PANELS: usize = 12;
/// Largest accepted chart description, in bytes of JSON.
const MAX_VEGA_BYTES: usize = 8 * 1024;
/// Top-level Vega-Lite properties a panel may use.
const VEGA_TOP_LEVEL: &[&str] = &[
    "mark",
    "encoding",
    "layer",
    "transform",
    "title",
    "description",
    "width",
    "height",
    "config",
    "resolve",
];
/// Property names that may not appear anywhere in a chart description: they load data or
/// link out, and the server supplies the data.
const VEGA_FORBIDDEN_KEYS: &[&str] = &[
    "data", "datasets", "url", "href", "usermeta", "$schema", "lookup", "from",
];

/// What a panel on a site's own data reports when the site has no backend database.
pub const NO_SITE_DATA: &str = "this site has no backend database";

/// Where a panel's query runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The metric views (`v_requests`, `v_events`, `v_checks`, `v_daily`).
    #[default]
    Metrics,
    /// The site's own backend database, read-only.
    Site,
}

impl Source {
    fn is_metrics(&self) -> bool {
        *self == Self::Metrics
    }

    /// Read the optional `source` argument of a tool call.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "metrics" => Ok(Self::Metrics),
            "site" => Ok(Self::Site),
            other => Err(format!(
                "unknown source {other:?}; use \"metrics\" or \"site\""
            )),
        }
    }
}

/// Read-only access to ONE site's own backend database, for panels with
/// `"source": "site"`. An implementation is bound to a single site when it is made, so
/// a query has no way to name another site's database.
pub trait SiteData: Send + Sync {
    /// Run one `SELECT`. `from` and `to` are the dashboard's ISO-8601 UTC range; see
    /// [`prepare_site_sql`] for how `:from` and `:to` are bound.
    fn query<'a>(
        &'a self,
        sql: &'a str,
        from: &'a str,
        to: &'a str,
    ) -> BoxFuture<'a, Result<QueryResult, String>>;
}

/// A dashboard: what is stored and what the agent submits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardSpec {
    pub version: u32,
    pub panels: Vec<Panel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    /// Short identifier, unique in the dashboard: lowercase letters, digits, hyphens.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Where `sql` runs. Left out of the JSON when it is the default, `metrics`.
    #[serde(default, skip_serializing_if = "Source::is_metrics")]
    pub source: Source,
    /// One SELECT. For `metrics`: over the metric views, using `:site_id`, `:from` and
    /// `:to`. For `site`: over the site's own tables, using `:from` and `:to`.
    pub sql: String,
    /// A Vega-Lite spec without `data`: the query's rows are injected by the server.
    pub vega_lite: Value,
}

/// A panel together with its data, ready to draw.
#[derive(Debug, Clone, Serialize)]
pub struct PanelData {
    #[serde(flatten)]
    pub panel: Panel,
    pub rows: Vec<Value>,
    /// Set when the query failed; the panel is shown with the message instead of a chart.
    pub error: Option<String>,
}

/// The JSON Schema of a dashboard spec, for the docs and the agent.
pub fn json_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Sitewright dashboard",
        "type": "object",
        "additionalProperties": false,
        "required": ["version", "panels"],
        "properties": {
            "version": { "const": 1 },
            "panels": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_PANELS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "title", "sql", "vega_lite"],
                    "properties": {
                        "id": { "type": "string", "pattern": "^[a-z0-9]([a-z0-9-]{0,38}[a-z0-9])?$" },
                        "title": { "type": "string", "minLength": 1, "maxLength": 80 },
                        "description": { "type": "string", "maxLength": 300 },
                        "source": { "enum": ["metrics", "site"], "default": "metrics", "description": "Where the query runs: the metric views, or the site's own backend database (read-only)." },
                        "sql": { "type": "string", "description": "One SELECT. source metrics: over v_requests, v_events, v_checks or v_daily, with :site_id, :from, :to (ISO-8601 text). source site: over the site's own tables, with :from and :to (Unix milliseconds)." },
                        "vega_lite": {
                            "type": "object",
                            "description": "A Vega-Lite spec without data. Allowed top-level properties: mark, encoding, layer, transform, title, description, width, height, config, resolve.",
                        },
                    },
                },
            },
        },
    })
}

/// Check everything about a spec that does not need the database. Returns every problem
/// found, so the agent can fix them in one go.
pub fn validate_shape(spec: &Value) -> Result<DashboardSpec, Vec<String>> {
    let parsed: DashboardSpec = serde_json::from_value(spec.clone())
        .map_err(|e| vec![format!("the spec does not match the schema: {e}")])?;
    let mut problems = Vec::new();
    if parsed.version != 1 {
        problems.push(format!("version must be 1, not {}", parsed.version));
    }
    if parsed.panels.is_empty() {
        problems.push("a dashboard needs at least one panel".into());
    }
    if parsed.panels.len() > MAX_PANELS {
        problems.push(format!(
            "at most {MAX_PANELS} panels are allowed, not {}",
            parsed.panels.len()
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for panel in &parsed.panels {
        let at = format!("panel {:?}", panel.id);
        let id = panel.id.as_bytes();
        let id_ok = !id.is_empty()
            && id.len() <= 40
            && id
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && id[0] != b'-'
            && id[id.len() - 1] != b'-';
        if !id_ok {
            problems.push(format!(
                "{at}: the id may use lowercase letters, digits and hyphens (at most 40)"
            ));
        }
        if !ids.insert(panel.id.as_str()) {
            problems.push(format!("{at}: the id is used twice"));
        }
        let title_len = panel.title.trim().chars().count();
        if title_len == 0 || title_len > 80 {
            problems.push(format!("{at}: the title must be 1 to 80 characters"));
        }
        if panel.description.chars().count() > 300 {
            problems.push(format!(
                "{at}: the description is longer than 300 characters"
            ));
        }
        problems.extend(
            vega_problems(&panel.vega_lite)
                .into_iter()
                .map(|p| format!("{at}: vega_lite: {p}")),
        );
    }
    if problems.is_empty() {
        Ok(parsed)
    } else {
        Err(problems)
    }
}

fn vega_problems(spec: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(object) = spec.as_object() else {
        return vec!["must be an object".into()];
    };
    if spec.to_string().len() > MAX_VEGA_BYTES {
        problems.push(format!("is larger than {MAX_VEGA_BYTES} bytes"));
    }
    for key in object.keys() {
        if !VEGA_TOP_LEVEL.contains(&key.as_str()) {
            problems.push(format!(
                "the property {key:?} is not allowed; use only {}",
                VEGA_TOP_LEVEL.join(", ")
            ));
        }
    }
    if !object.contains_key("mark") && !object.contains_key("layer") {
        problems.push("needs a mark (or layers)".into());
    }
    scan(spec, 0, &mut problems);
    problems.sort();
    problems.dedup();
    problems
}

/// Look through the whole chart description for anything that loads or links elsewhere.
fn scan(value: &Value, depth: usize, problems: &mut Vec<String>) {
    if depth > 16 {
        problems.push("is nested too deeply".into());
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                if VEGA_FORBIDDEN_KEYS.contains(&key.as_str()) {
                    problems.push(format!("the property {key:?} is not allowed: the server supplies the data, and charts may not load or link to anything"));
                }
                scan(inner, depth + 1, problems);
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| scan(item, depth + 1, problems)),
        Value::String(text) => {
            let lower = text.trim_start().to_ascii_lowercase();
            if ["http://", "https://", "//", "data:", "javascript:"]
                .iter()
                .any(|scheme| lower.starts_with(scheme))
            {
                problems.push("URLs are not allowed".into());
            }
        }
        _ => {}
    }
}

/// Every `field` named in a chart's encodings.
fn encoded_fields(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                match (key.as_str(), inner) {
                    ("field", Value::String(name)) => out.push(name.clone()),
                    _ => encoded_fields(inner, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| encoded_fields(item, out)),
        _ => {}
    }
}

/// The default time range for dashboards: the last `days` days up to now, as ISO text.
pub fn range(days: i64) -> (String, String) {
    let iso = |ms: i64| {
        let secs = ms.div_euclid(1000);
        let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        let (y, m, d) = civil_from_days(days);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        )
    };
    let now = now_ms();
    (iso(now - days * MS_PER_DAY), iso(now + 1000))
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Run a query where its panel says: in the metrics sandbox, or on the site's own
/// database.
pub async fn query(
    sandbox: &Sandbox,
    site_data: Option<&dyn SiteData>,
    site_id: i64,
    source: Source,
    sql: &str,
    (from, to): (&str, &str),
) -> Result<QueryResult, String> {
    match (source, site_data) {
        (Source::Metrics, _) => sandbox.query(site_id, from, to, sql).await,
        (Source::Site, Some(site_data)) => site_data.query(sql, from, to).await,
        (Source::Site, None) => Err(NO_SITE_DATA.into()),
    }
}

/// Check a query for a site's own database and turn its `:from` and `:to` into bound
/// parameters. Returns the SQL with `?` placeholders and the values to bind, in order.
///
/// Site tables conventionally store times as `Date.now()` (Unix milliseconds), so the
/// ISO-8601 range the dashboard uses is bound as two integers. Binding, rather than
/// pasting the numbers in, keeps the values out of the SQL text altogether. What the
/// query may *do* is not decided here: the site database's authorizer does that.
pub fn prepare_site_sql(sql: &str, from: &str, to: &str) -> Result<(String, Vec<i64>), String> {
    let sql = sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return Err("the query is empty".into());
    }
    if sql.len() > MAX_SQL_BYTES {
        return Err(format!("the query is longer than {MAX_SQL_BYTES} bytes"));
    }
    if sql.contains(';') {
        return Err("only one statement is allowed (remove the ';')".into());
    }
    let head = sql
        .trim_start_matches('(')
        .trim_start()
        .to_ascii_lowercase();
    if !(head.starts_with("select") || head.starts_with("with")) {
        return Err("only SELECT queries are allowed".into());
    }
    let (Some(from_ms), Some(to_ms)) = (iso_to_ms(from), iso_to_ms(to)) else {
        return Err("the time range is not valid".into());
    };

    let mut out = String::with_capacity(sql.len());
    let mut params = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // A string literal or quoted identifier is copied as it is: a `?` inside
            // one is not a placeholder, so nothing in it may be replaced. (A doubled
            // quote inside a literal reads as two literals side by side, which copies
            // the same text.)
            quote @ (b'\'' | b'"') => {
                let end = sql[i + 1..]
                    .bytes()
                    .position(|b| b == quote)
                    .map_or(bytes.len(), |at| i + 1 + at + 1);
                out.push_str(&sql[i..end]);
                i = end;
                continue;
            }
            // The values are bound by position, so a placeholder of the query's own
            // would shift them.
            b'?' => {
                return Err(
                    "'?' placeholders are not available; use :from and :to for the time range"
                        .into(),
                );
            }
            b':' => {
                let rest = &sql[i + 1..];
                let name_len = rest
                    .bytes()
                    .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .count();
                match &rest[..name_len] {
                    "from" => params.push(from_ms),
                    "to" => params.push(to_ms),
                    "site_id" => {
                        return Err(
                            ":site_id is not available with source \"site\": the query already runs on this site's own database, so leave it out"
                                .into(),
                        );
                    }
                    "" => {
                        out.push(':');
                        i += 1;
                        continue;
                    }
                    name => {
                        return Err(format!(
                            "unknown parameter :{name}; only :from and :to (Unix milliseconds) are available with source \"site\""
                        ));
                    }
                }
                out.push('?');
                i += 1 + name_len;
                continue;
            }
            _ => {}
        }
        // Advance one whole character.
        let ch_len = sql[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&sql[i..i + ch_len]);
        i += ch_len;
    }
    Ok((out, params))
}

/// `YYYY-MM-DDTHH:MM:SSZ` (what [`range`] produces) as Unix milliseconds.
pub fn iso_to_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let shape_ok = bytes.len() == 20
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            10 => *b == b'T',
            13 | 16 => *b == b':',
            19 => *b == b'Z',
            _ => b.is_ascii_digit(),
        });
    if !shape_ok {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    Some(seconds * 1000)
}

/// (year, month, day) to days since 1970-01-01: the inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Validate a spec fully: its shape, then every query for `site_id`, each where its
/// panel's `source` says. Without `site_data`, panels on the site's own data are
/// reported as problems.
pub async fn validate(
    sandbox: &Sandbox,
    site_data: Option<&dyn SiteData>,
    site_id: i64,
    spec: &Value,
) -> Result<DashboardSpec, Vec<String>> {
    let parsed = validate_shape(spec)?;
    let (from, to) = range(30);
    let mut problems = Vec::new();
    for panel in &parsed.panels {
        let range = (from.as_str(), to.as_str());
        match query(sandbox, site_data, site_id, panel.source, &panel.sql, range).await {
            Ok(result) => {
                // With rows in hand, check the chart only uses columns the query returns.
                if !result.columns.is_empty() {
                    let mut fields = Vec::new();
                    encoded_fields(&panel.vega_lite, &mut fields);
                    for field in fields {
                        let computed = panel
                            .vega_lite
                            .to_string()
                            .contains(&format!("\"as\":\"{field}\""));
                        if !result.columns.contains(&field) && !computed {
                            problems.push(format!(
                                "panel {:?}: the chart uses the field {field:?}, but the query returns {}",
                                panel.id,
                                result.columns.join(", ")
                            ));
                        }
                    }
                }
            }
            Err(e) => problems.push(format!("panel {:?}: sql: {e}", panel.id)),
        }
    }
    if problems.is_empty() {
        Ok(parsed)
    } else {
        Err(problems)
    }
}

/// Store a validated dashboard for a site, replacing the previous one.
pub async fn save(db: &SqlitePool, site_id: i64, spec: &DashboardSpec) -> Result<i64, sqlx::Error> {
    let json = serde_json::to_string(spec).unwrap_or_else(|_| "{}".into());
    let now = now_ms();
    let version = sqlx::query_scalar!(
        r#"INSERT INTO dashboards (site_id, spec, version, updated_at) VALUES (?1, ?2, 1, ?3)
           ON CONFLICT (site_id) DO UPDATE SET spec = excluded.spec, version = dashboards.version + 1, updated_at = excluded.updated_at
           RETURNING version"#,
        site_id,
        json,
        now
    )
    .fetch_one(db)
    .await?;
    Ok(version)
}

/// A site's AI dashboard, if it has one: the spec and when it was saved.
pub async fn load(
    db: &SqlitePool,
    site_id: i64,
) -> Result<Option<(DashboardSpec, i64)>, sqlx::Error> {
    let row = sqlx::query!(
        "SELECT spec, updated_at FROM dashboards WHERE site_id = ?1",
        site_id
    )
    .fetch_optional(db)
    .await?;
    Ok(row.and_then(|r| Some((serde_json::from_str(&r.spec).ok()?, r.updated_at))))
}

/// Run every panel's query and attach the rows. Without `site_data`, panels on the
/// site's own data carry an error instead.
pub async fn render(
    sandbox: &Sandbox,
    site_data: Option<&dyn SiteData>,
    site_id: i64,
    spec: &DashboardSpec,
    days: i64,
) -> Vec<PanelData> {
    let (from, to) = range(days);
    let mut out = Vec::with_capacity(spec.panels.len());
    for panel in &spec.panels {
        let range = (from.as_str(), to.as_str());
        let result = query(sandbox, site_data, site_id, panel.source, &panel.sql, range).await;
        let (rows, error) = match result {
            Ok(result) => (result.rows, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        out.push(PanelData {
            panel: panel.clone(),
            rows,
            error,
        });
    }
    out
}

/// The fixed dashboard every site gets.
pub fn default_spec() -> DashboardSpec {
    let panel = |id: &str, title: &str, description: &str, sql: &str, vega_lite: Value| Panel {
        id: id.into(),
        title: title.into(),
        description: description.into(),
        source: Source::Metrics,
        sql: sql.into(),
        vega_lite,
    };
    let day =
        json!({"field": "day", "type": "temporal", "title": null, "timeUnit": "yearmonthdate"});
    DashboardSpec {
        version: 1,
        panels: vec![
            panel(
                "views",
                "Page views per day",
                "Pages loaded by people (bots and assets are not counted).",
                "SELECT day, sum(is_page_view) AS views FROM v_requests WHERE ts BETWEEN :from AND :to GROUP BY day ORDER BY day",
                json!({"mark": {"type": "bar"}, "encoding": {"x": day, "y": {"field": "views", "type": "quantitative", "title": "Views"}}}),
            ),
            panel(
                "visitors",
                "Unique visitors per day",
                "Distinct visitors per day. Visitors are counted with a hash that changes daily; no IP addresses are stored.",
                "SELECT day, count(DISTINCT CASE WHEN is_page_view THEN visitor END) AS visitors FROM v_requests WHERE ts BETWEEN :from AND :to GROUP BY day ORDER BY day",
                json!({"mark": {"type": "line", "point": true}, "encoding": {"x": day, "y": {"field": "visitors", "type": "quantitative", "title": "Visitors"}}}),
            ),
            panel(
                "top-pages",
                "Top pages",
                "The ten most viewed pages.",
                "SELECT path, count(*) AS views FROM v_requests WHERE ts BETWEEN :from AND :to AND is_page_view GROUP BY path ORDER BY views DESC LIMIT 10",
                json!({"mark": {"type": "bar"}, "encoding": {"y": {"field": "path", "type": "nominal", "sort": "-x", "title": null}, "x": {"field": "views", "type": "quantitative", "title": "Views"}}}),
            ),
            panel(
                "status",
                "Responses by status",
                "How requests were answered: 2xx success, 3xx not modified or redirect, 4xx not found, 5xx server error.",
                "SELECT (status / 100) || 'xx' AS class, count(*) AS requests FROM v_requests WHERE ts BETWEEN :from AND :to GROUP BY class ORDER BY class",
                json!({"mark": {"type": "bar"}, "encoding": {"x": {"field": "class", "type": "nominal", "title": null}, "y": {"field": "requests", "type": "quantitative", "title": "Requests"}}}),
            ),
            panel(
                "latency",
                "Response time (95th percentile)",
                "95% of requests were answered at least this fast, per day.",
                "SELECT day, p95_ms FROM v_daily WHERE day BETWEEN date(:from) AND date(:to) ORDER BY day",
                json!({"mark": {"type": "line", "point": true}, "encoding": {"x": day, "y": {"field": "p95_ms", "type": "quantitative", "title": "Milliseconds"}}}),
            ),
            panel(
                "uptime",
                "Uptime",
                "Share of successful uptime checks per day.",
                "SELECT day, round(100.0 * avg(ok), 2) AS uptime FROM v_checks WHERE ts BETWEEN :from AND :to GROUP BY day ORDER BY day",
                json!({"mark": {"type": "area", "line": true}, "encoding": {"x": day, "y": {"field": "uptime", "type": "quantitative", "title": "Percent", "scale": {"domain": [0, 100]}}}}),
            ),
        ],
    }
}

/// What the agent's `metrics_schema` tool returns: the views, their columns, the
/// parameters and the rules.
pub fn metrics_schema() -> Value {
    json!({
        "parameters": {
            ":site_id": "this site's id (queries only ever see this site's rows, with or without it)",
            ":from": "start of the selected time range, ISO-8601 UTC text",
            ":to": "end of the selected time range, ISO-8601 UTC text",
        },
        "sources": {
            "metrics": "The default. A panel (or run_readonly_query) reads the views below. Everything under parameters, views and rules is about this source.",
            "site": {
                "description": "Set \"source\": \"site\" on a panel (or pass source to run_readonly_query) to read the site's OWN backend database instead, read-only: guestbook entries, sign-ups, orders, whatever its backend stores. Call backend_schema for the tables. A site without a published backend has no database, and such a panel fails.",
                "parameters": {
                    ":from": "start of the selected time range, as Unix milliseconds (an integer)",
                    ":to": "end of the selected time range, as Unix milliseconds (an integer)",
                },
                "rules": [
                    "One SELECT (WITH is fine) over the site's own tables. The metric views are not available here, and the site's tables are not available to metrics panels: one panel reads one source.",
                    "No :site_id and no '?' placeholders: the database is this site's own.",
                    "Site tables conventionally store times as Date.now() milliseconds, so filter with: created_at BETWEEN :from AND :to, and group by day with: date(created_at / 1000, 'unixepoch').",
                    "sw_users (visitor accounts) exposes only id, email and created_at; do not SELECT * from it. Sessions and password hashes cannot be read.",
                    "Show counts and aggregates, not personal data: never chart emails, names or message texts.",
                    format!("At most {} rows are returned.", crate::sandbox::MAX_ROWS),
                ],
                "example": "SELECT date(created_at / 1000, 'unixepoch') AS day, count(*) AS n FROM entries WHERE created_at BETWEEN :from AND :to GROUP BY day ORDER BY day",
            },
        },
        "views": {
            "v_requests": {
                "description": "One row per request served for this site (kept 30 days).",
                "columns": {
                    "ts": "text, ISO-8601 UTC, e.g. 2026-11-15T09:30:00Z", "day": "text, YYYY-MM-DD", "hour": "integer 0-23",
                    "method": "text", "path": "text, without the query string", "status": "integer HTTP status",
                    "bytes": "integer", "duration_ms": "integer", "referrer_host": "text or NULL: hostname the visitor came from",
                    "ua_class": "text: browser, bot or other", "visitor": "text: daily-rotating visitor hash",
                    "is_page_view": "integer 0/1: a successful page load by a person (not a bot, not an asset)",
                },
            },
            "v_events": {
                "description": "One row per analytics event sent by the site's beacon script (kept 30 days). A 'pageview' event is sent on every page load; other names are custom events the site sends with sw('name', {props}).",
                "columns": {
                    "ts": "text, ISO-8601 UTC", "day": "text, YYYY-MM-DD", "name": "text: event name", "path": "text: page the event happened on",
                    "props": "text: JSON object; read with json_extract(props, '$.key'). pageview has ref (referring host) and screen (small, medium, large)",
                    "visitor": "text: daily-rotating visitor hash",
                },
            },
            "v_checks": {
                "description": "One row per uptime check (about one a minute, kept 30 days).",
                "columns": { "ts": "text, ISO-8601 UTC", "day": "text", "ok": "integer 0/1", "latency_ms": "integer", "status": "integer or NULL", "cert_days": "integer or NULL" },
            },
            "v_daily": {
                "description": "One row per day, kept forever.",
                "columns": { "day": "text, YYYY-MM-DD", "views": "integer", "visitors": "integer", "errors": "integer", "p95_ms": "integer", "uptime_pct": "real or NULL" },
            },
        },
        "rules": [
            "One SELECT (WITH is fine). No other statements, no other tables.",
            format!("At most {} rows are returned and a query may run for {} seconds.", crate::sandbox::MAX_ROWS, crate::sandbox::QUERY_TIMEOUT.as_secs()),
            "Filter by time with: ts BETWEEN :from AND :to (v_daily: day BETWEEN date(:from) AND date(:to)).",
            "Functions available: count, sum, avg, min, max, round, coalesce, substr, lower, date, strftime, json_extract and other ordinary SQLite functions.",
        ],
        "dashboard_schema": json_schema(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(vega: Value) -> Value {
        json!({"version": 1, "panels": [{"id": "trail-searches", "title": "Trail searches per day", "description": "How often visitors search",
            "sql": "SELECT day, count(*) AS n FROM v_events WHERE site_id = :site_id AND name = 'search' AND ts BETWEEN :from AND :to GROUP BY day",
            "vega_lite": vega}]})
    }

    fn line() -> Value {
        json!({"mark": "line", "encoding": {"x": {"field": "day", "type": "temporal"}, "y": {"field": "n", "type": "quantitative"}}})
    }

    #[test]
    fn the_plan_example_is_valid_and_the_default_dashboard_too() {
        assert!(validate_shape(&spec(line())).is_ok());
        let default = serde_json::to_value(default_spec()).unwrap();
        let parsed = validate_shape(&default).unwrap();
        assert_eq!(parsed.panels.len(), 6);
    }

    #[test]
    fn data_urls_and_unknown_fields_are_rejected() {
        let bad_charts = [
            json!({"mark": "line", "data": {"url": "https://evil.example/data.json"}}),
            json!({"mark": "line", "data": {"values": [1]}}),
            json!({"mark": "line", "datasets": {"x": []}}),
            json!({"mark": {"type": "image", "url": "https://evil.example/x.png"}}),
            json!({"mark": "point", "encoding": {"href": {"field": "path"}}}),
            json!({"mark": "image", "encoding": {"url": {"value": "//evil.example/x.png"}}}),
            json!({"mark": "line", "transform": [{"lookup": "day", "from": {"data": {"url": "x"}}}]}),
            json!({"mark": "text", "encoding": {"text": {"value": "https://evil.example"}}}),
            json!({"mark": "line", "params": [{"name": "x"}]}),
            json!({"$schema": "https://vega.github.io/schema/vega-lite/v5.json", "mark": "line"}),
            json!({"encoding": {}}),
            json!("mark"),
        ];
        for chart in bad_charts {
            let problems = validate_shape(&spec(chart.clone())).unwrap_err();
            assert!(!problems.is_empty(), "{chart}");
        }

        let mut unknown_panel_field = spec(line());
        unknown_panel_field["panels"][0]["script"] = json!("alert(1)");
        assert!(validate_shape(&unknown_panel_field).is_err());
        let mut unknown_top = spec(line());
        unknown_top["theme"] = json!("dark");
        assert!(validate_shape(&unknown_top).is_err());

        let mut wrong_version = spec(line());
        wrong_version["version"] = json!(2);
        let mut bad_id = spec(line());
        bad_id["panels"][0]["id"] = json!("Bad Id");
        let mut no_title = spec(line());
        no_title["panels"][0]["title"] = json!(" ");
        let mut duplicate = spec(line());
        let first = duplicate["panels"][0].clone();
        duplicate["panels"].as_array_mut().unwrap().push(first);
        let mut too_many = spec(line());
        let panels: Vec<Value> = (0..13)
            .map(|i| {
                let mut p = too_many["panels"][0].clone();
                p["id"] = json!(format!("p{i}"));
                p
            })
            .collect();
        too_many["panels"] = json!(panels);
        let empty = json!({"version": 1, "panels": []});
        for bad in [wrong_version, bad_id, no_title, duplicate, too_many, empty] {
            assert!(validate_shape(&bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn queries_are_run_in_the_sandbox_before_a_spec_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = sw_core::db::open(&path).await.unwrap();
        sw_core::db::migrate(&db, &dir.path().join("b"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO events (ts, site_id, name, visitor_hash) VALUES (?, 1, 'search', 'v')",
        )
        .bind(now_ms())
        .execute(&db)
        .await
        .unwrap();
        let sandbox = Sandbox::open(&path).await.unwrap();

        assert!(validate(&sandbox, None, 1, &spec(line())).await.is_ok());

        let mut forbidden = spec(line());
        forbidden["panels"][0]["sql"] = json!("SELECT value AS day, 1 AS n FROM settings");
        let problems = validate(&sandbox, None, 1, &forbidden).await.unwrap_err();
        assert!(problems[0].contains("not allowed"), "{problems:?}");

        let mut writes = spec(line());
        writes["panels"][0]["sql"] = json!("DELETE FROM events");
        assert!(validate(&sandbox, None, 1, &writes).await.is_err());

        let mut typo = spec(line());
        typo["panels"][0]["sql"] = json!("SELECT day, count(*) AS n FROM v_event");
        assert!(validate(&sandbox, None, 1, &typo).await.unwrap_err()[0].contains("SQL error"));

        let wrong_field = spec(
            json!({"mark": "line", "encoding": {"x": {"field": "date", "type": "temporal"}, "y": {"field": "n", "type": "quantitative"}}}),
        );
        let problems = validate(&sandbox, None, 1, &wrong_field).await.unwrap_err();
        assert!(
            problems[0].contains("\"date\"") && problems[0].contains("day, n"),
            "{problems:?}"
        );

        // Saving bumps the version; rendering attaches rows.
        let parsed = validate(&sandbox, None, 1, &spec(line())).await.unwrap();
        assert!(save(&db, 1, &parsed).await.is_err(), "the site must exist");
        sqlx::query("INSERT INTO sites (id, name, title, created_at, updated_at) VALUES (1, 's', 'S', 0, 0)").execute(&db).await.unwrap();
        assert_eq!(save(&db, 1, &parsed).await.unwrap(), 1);
        assert_eq!(save(&db, 1, &parsed).await.unwrap(), 2);
        let (loaded, _) = load(&db, 1).await.unwrap().unwrap();
        let data = render(&sandbox, None, 1, &loaded, 30).await;
        assert_eq!(data[0].rows[0]["n"], 1);
        assert!(data[0].error.is_none());

        // The built-in dashboard runs cleanly against an (almost) empty database.
        for panel in render(&sandbox, None, 1, &default_spec(), 30).await {
            assert!(
                panel.error.is_none(),
                "{}: {:?}",
                panel.panel.id,
                panel.error
            );
        }
    }

    #[test]
    fn site_queries_get_their_time_range_as_bound_milliseconds() {
        let (from, to) = ("2026-11-01T00:00:00Z", "2026-11-16T12:30:05Z");
        let (from_ms, to_ms) = (1_793_491_200_000, 1_794_832_205_000);
        assert_eq!(iso_to_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_ms("2000-02-29T00:00:01Z"), Some(951_782_401_000));
        assert_eq!(iso_to_ms(from), Some(from_ms));
        assert_eq!(iso_to_ms(to), Some(to_ms));
        // What `range` writes is what `iso_to_ms` reads.
        let (start, end) = range(7);
        let (start, end) = (iso_to_ms(&start).unwrap(), iso_to_ms(&end).unwrap());
        assert!((end - start - 7 * MS_PER_DAY - 1000).abs() < 2000);
        assert!((end - now_ms()).abs() < 5000);
        for bad in [
            "",
            "2026-11-01",
            "2026-11-01 00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-11-01T24:00:00Z",
            "+026-11-01T00:00:00Z",
            "2026-11-01T00:00:0éZ",
        ] {
            assert_eq!(iso_to_ms(bad), None, "{bad}");
        }

        // Values are bound in the order the tokens appear, however often.
        let (sql, params) = prepare_site_sql(
            " SELECT count(*) AS n FROM entries WHERE created_at <= :to AND created_at >= :from AND updated_at < :to; ",
            from,
            to,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT count(*) AS n FROM entries WHERE created_at <= ? AND created_at >= ? AND updated_at < ?"
        );
        assert_eq!(params, [to_ms, from_ms, to_ms]);

        let plain = "WITH x AS (SELECT 1 AS a) SELECT a FROM x";
        assert_eq!(
            prepare_site_sql(plain, from, to).unwrap(),
            (plain.to_owned(), vec![])
        );

        // Literals and quoted names are left alone, so the placeholders still line up.
        let (sql, params) = prepare_site_sql(
            "SELECT 'at 12:30, :from? it''s :to' AS \"why:to?\", 'é' AS e FROM t WHERE at > :from",
            from,
            to,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT 'at 12:30, :from? it''s :to' AS \"why:to?\", 'é' AS e FROM t WHERE at > ?"
        );
        assert_eq!(params, [from_ms]);
        let open = "SELECT 'never closed :from";
        assert_eq!(
            prepare_site_sql(open, from, to).unwrap(),
            (open.to_owned(), vec![])
        );

        let refused = |sql: &str| prepare_site_sql(sql, from, to).unwrap_err();
        assert!(refused("SELECT * FROM entries WHERE site = :site_id").contains("leave it out"));
        assert!(refused("SELECT :limit").contains("unknown parameter :limit"));
        // Part of a longer name is a different, unknown parameter.
        assert!(refused("SELECT :from_day").contains(":from_day"));
        assert!(refused("SELECT :total").contains(":total"));
        assert!(refused("SELECT * FROM entries WHERE id = ?").contains("placeholders"));
        assert!(refused("SELECT ?1").contains("placeholders"));
        assert!(refused("DELETE FROM entries").contains("only SELECT"));
        assert!(refused("SELECT 1; DELETE FROM entries").contains("one statement"));
        assert!(refused("  ").contains("empty"));
        assert!(refused(&format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES))).contains("longer"));
        assert!(
            prepare_site_sql("SELECT 1", "yesterday", to)
                .unwrap_err()
                .contains("time range")
        );
    }

    /// A stand-in for a site database: it answers with the SQL it was given.
    struct Echo;

    impl SiteData for Echo {
        fn query<'a>(
            &'a self,
            sql: &'a str,
            from: &'a str,
            to: &'a str,
        ) -> BoxFuture<'a, Result<QueryResult, String>> {
            Box::pin(async move {
                let (sql, params) = prepare_site_sql(sql, from, to)?;
                Ok(QueryResult {
                    columns: vec!["day".into(), "n".into()],
                    rows: vec![json!({"day": sql, "n": params.len()})],
                    truncated: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn site_panels_run_on_the_site_data_and_fail_cleanly_without_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = sw_core::db::open(&path).await.unwrap();
        sw_core::db::migrate(&db, &dir.path().join("b"))
            .await
            .unwrap();
        let sandbox = Sandbox::open(&path).await.unwrap();

        let mut mixed = spec(line());
        let mut site_panel = mixed["panels"][0].clone();
        site_panel["id"] = json!("entries");
        site_panel["source"] = json!("site");
        site_panel["sql"] =
            json!("SELECT day, n FROM entries WHERE created_at BETWEEN :from AND :to");
        mixed["panels"].as_array_mut().unwrap().push(site_panel);

        // The source survives a round trip, and the default is left out.
        let parsed = validate(&sandbox, Some(&Echo), 1, &mixed).await.unwrap();
        assert_eq!(parsed.panels[0].source, Source::Metrics);
        assert_eq!(parsed.panels[1].source, Source::Site);
        let stored = serde_json::to_value(&parsed).unwrap();
        assert!(stored["panels"][0].get("source").is_none());
        assert_eq!(stored["panels"][1]["source"], "site");
        assert_eq!(validate_shape(&stored).unwrap(), parsed);

        let data = render(&sandbox, Some(&Echo), 1, &parsed, 30).await;
        assert!(data[0].error.is_none() && data[1].error.is_none());
        assert_eq!(
            data[1].rows[0]["day"],
            "SELECT day, n FROM entries WHERE created_at BETWEEN ? AND ?"
        );
        assert_eq!(data[1].rows[0]["n"], 2);

        // A site table is not reachable from a metrics panel: the source decides.
        let mut wrong_source = mixed.clone();
        wrong_source["panels"][1]
            .as_object_mut()
            .unwrap()
            .remove("source");
        let problems = validate(&sandbox, Some(&Echo), 1, &wrong_source)
            .await
            .unwrap_err();
        assert!(
            problems[0].starts_with("panel \"entries\": sql:"),
            "{problems:?}"
        );

        // The chart's fields are checked against a site query's columns too.
        let mut wrong_field = mixed.clone();
        wrong_field["panels"][1]["vega_lite"]["encoding"]["y"]["field"] = json!("total");
        let problems = validate(&sandbox, Some(&Echo), 1, &wrong_field)
            .await
            .unwrap_err();
        assert!(
            problems[0].contains("\"total\"") && problems[0].contains("day, n"),
            "{problems:?}"
        );

        // Without a site database the panel fails, and only that panel.
        let problems = validate(&sandbox, None, 1, &mixed).await.unwrap_err();
        assert_eq!(
            problems,
            [format!("panel \"entries\": sql: {NO_SITE_DATA}")]
        );
        let data = render(&sandbox, None, 1, &parsed, 30).await;
        assert!(data[0].error.is_none());
        assert_eq!(data[1].error.as_deref(), Some(NO_SITE_DATA));
        assert!(data[1].rows.is_empty());

        let mut unknown = mixed.clone();
        unknown["panels"][1]["source"] = json!("main");
        assert!(validate_shape(&unknown).is_err());
        assert_eq!(Source::parse("site"), Ok(Source::Site));
        assert_eq!(Source::parse("metrics"), Ok(Source::Metrics));
        assert!(Source::parse("main").is_err());

        // The agent is told how: the field, where the tables are listed, the units.
        let schema = metrics_schema();
        let site = schema["sources"]["site"].to_string();
        for needle in [
            "backend_schema",
            "Unix milliseconds",
            "id, email and created_at",
        ] {
            assert!(site.contains(needle), "{needle}");
        }
        let source =
            &schema["dashboard_schema"]["properties"]["panels"]["items"]["properties"]["source"];
        assert_eq!(source["enum"], json!(["metrics", "site"]));
    }

    #[test]
    fn time_ranges_are_iso_text() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        let (from, to) = range(7);
        assert!(
            from < to && from.len() == 20 && from.ends_with('Z') && from.as_bytes()[10] == b'T'
        );
    }
}
