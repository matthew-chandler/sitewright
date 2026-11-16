//! The agent's tools: their schemas and their implementations.
//!
//! Every tool is bound to the run's site. The model cannot name another site for a write,
//! and every path it gives goes through the site store's sandbox.

use serde_json::{Value, json};
use sw_llm::{ToolCall, ToolDef};
use sw_metrics::dashboard::{SiteData, Source};
use sw_sites::{Author, DomainChecker, SiteError, SiteStore};

use crate::sitedata::SiteQueries;

/// Largest file `read_file` returns to the model, in bytes.
const MAX_READ_BYTES: usize = 200 * 1024;

/// What a tool needs to run.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub store: SiteStore,
    pub domains: DomainChecker,
    pub skills: crate::skills::Skills,
    pub metrics: sw_metrics::Metrics,
    /// Site backends, when the server has them.
    pub backends: Option<sw_backend::SiteBackends>,
    /// The site this run is bound to.
    pub site: String,
    /// Whether the run was started to create `site` (so `create_site` is allowed).
    pub may_create: bool,
    /// Mark the site as containerized when `create_site` creates it.
    pub containerize_on_create: bool,
    /// Public HTTP port used by `check_domain`.
    pub http_port: u16,
}

fn no_backends() -> ToolOutcome {
    ToolOutcome::error("backends are not available on this server")
}

/// The outcome of one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// JSON text handed back to the model.
    pub content: String,
    pub is_error: bool,
    /// Something the run itself must act on.
    pub effect: Option<Effect>,
}

/// Side effects the runner handles on the agent's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The agent asked to delete the site; a person has to confirm.
    ConfirmDelete,
    /// The site was created, so the run is no longer allowed to create one.
    Created,
}

impl ToolOutcome {
    fn ok(value: Value) -> Self {
        Self {
            content: value.to_string(),
            is_error: false,
            effect: None,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            content: json!({ "error": message.into() }).to_string(),
            is_error: true,
            effect: None,
        }
    }
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// The tool definitions sent to the model.
pub fn definitions() -> Vec<ToolDef> {
    let def = |name: &str, description: &str, parameters: Value| ToolDef {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
    };
    let none = || schema(json!({}), &[]);
    let path = json!({ "type": "string", "description": "Path relative to the site root, for example css/site.css" });
    vec![
        def(
            "list_sites",
            "List all sites on this server with their URLs.",
            none(),
        ),
        def(
            "site_info",
            "Show this site's title, hostnames, URL and release history.",
            none(),
        ),
        def(
            "create_site",
            "Create this conversation's site. Only needed (and only allowed) when the site does not exist yet.",
            schema(
                json!({
                    "name": { "type": "string", "description": "The site name given in the instructions" },
                    "title": { "type": "string", "description": "Short human-readable title, for example \"Washington Hikes\"" },
                }),
                &["name", "title"],
            ),
        ),
        def(
            "list_files",
            "List the files in the site's working tree with their sizes.",
            none(),
        ),
        def(
            "read_file",
            "Read a text file from the working tree.",
            schema(json!({ "path": path }), &["path"]),
        ),
        def(
            "write_file",
            "Create or replace a file in the working tree with the complete new content. Not live until publish_site.",
            schema(
                json!({ "path": path, "content": { "type": "string", "description": "The whole file" } }),
                &["path", "content"],
            ),
        ),
        def(
            "delete_file",
            "Delete a file from the working tree.",
            schema(json!({ "path": path }), &["path"]),
        ),
        def(
            "validate_site",
            "Check the working tree: index.html exists, links and assets resolve, sizes are within limits. Returns problems to fix.",
            none(),
        ),
        def(
            "publish_site",
            "Validate, commit and make the working tree live. Refused if validation finds errors. Rolls back automatically if the new release cannot be served.",
            schema(
                json!({ "message": { "type": "string", "description": "One line describing this release" } }),
                &["message"],
            ),
        ),
        def(
            "rollback_site",
            "Switch the live site to an earlier release and reset the working tree to it.",
            schema(
                json!({ "release": { "type": "string", "description": "Commit sha (or prefix) of the release; omit for the one before the current release" } }),
                &[],
            ),
        ),
        def(
            "delete_site",
            "Ask the user to confirm deleting this site. Does not delete anything itself: the site is removed only after the user confirms.",
            none(),
        ),
        def(
            "export_site",
            "Build a downloadable archive of the live site: \"folder\" (the files, for any static host) or \"context\" (a Docker build context with a Dockerfile). The user downloads it from the site page.",
            schema(
                json!({ "format": { "type": "string", "enum": ["folder", "context"] } }),
                &["format"],
            ),
        ),
        def(
            "metrics_schema",
            "Describe the data you can query for this site's dashboard: the metric views, their columns, the query parameters, how to read the site's own backend tables (source \"site\"), and the dashboard format.",
            none(),
        ),
        def(
            "run_readonly_query",
            "Run one read-only SELECT and return the rows: over this site's metric views, or with source \"site\" over the site's own backend database. Use it to explore the data and to test a dashboard panel's query.",
            schema(
                json!({
                    "sql": { "type": "string", "description": "One SELECT. source metrics: over v_requests, v_events, v_checks or v_daily; may use :site_id, :from and :to (ISO-8601 text). source site: over the site's own tables (see backend_schema); may use :from and :to (Unix milliseconds)." },
                    "days": { "type": "integer", "description": "Length of the time range ending now, 1 to 90. Default 30." },
                    "source": { "type": "string", "enum": ["metrics", "site"], "description": "Where the query runs: \"metrics\" (default) or \"site\", the site's own backend database." },
                }),
                &["sql"],
            ),
        ),
        def(
            "save_dashboard",
            "Validate and store this site's dashboard, replacing the previous one. Every panel's query is run first; problems are returned for you to fix.",
            schema(
                json!({ "spec": { "type": "object", "description": "The dashboard: {\"version\": 1, \"panels\": [...]}. See metrics_schema." } }),
                &["spec"],
            ),
        ),
        def(
            "test_backend",
            "Send one HTTP request to this site's UNPUBLISHED backend (the files under backend/ in the working tree) and return the response and console output. It runs against a private copy of the site's database with the working tree's migrations applied, so nothing live changes. The copy is kept between calls until the backend's files change.",
            schema(
                json!({
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                    "path": { "type": "string", "description": "Path and optional query, for example /api/entries?limit=5" },
                    "body": { "type": "string", "description": "Request body as text. JSON by default." },
                    "content_type": { "type": "string", "description": "Default application/json. Use application/x-www-form-urlencoded to test a form post." },
                    "cookie": { "type": "string", "description": "A Cookie header to send, for example the name=value part of a set-cookie you got back from a sign-in, to test as a signed-in visitor." },
                    "reset": { "type": "boolean", "description": "Start again from a fresh copy of the live data." },
                }),
                &["method", "path"],
            ),
        ),
        def(
            "backend_logs",
            "Recent console output (console.log, errors with stack traces) of this site's backend, live and draft, oldest first.",
            schema(
                json!({ "limit": { "type": "integer", "description": "How many lines, 1 to 200. Default 50." } }),
                &[],
            ),
        ),
        def(
            "backend_access",
            "What this site's backend may use beyond its database: the NAMES of the secrets the owner stored (never their values) and the outside hosts it may send requests to, with whether the owner has approved each.",
            none(),
        ),
        def(
            "list_models",
            "The AI models this server has, by the name a backend passes to llm.chat({ model }), and whether this site's backend may use each. Use it when the user refers to a model or \"agent\" by name, or asks to switch models.",
            none(),
        ),
        def(
            "request_backend_host",
            "Ask the site's owner to allow the backend to send HTTPS requests to an outside host. The owner approves it on the site's page; until then requests to it fail. You cannot approve it yourself.",
            schema(
                json!({
                    "host": { "type": "string", "description": "A host name such as api.example.com (no scheme, path or port)" },
                    "reason": { "type": "string", "description": "One sentence for the owner: what the site uses this host for" },
                }),
                &["host", "reason"],
            ),
        ),
        def(
            "backend_schema",
            "The tables, indexes and views of this site's LIVE database, as CREATE statements, and its size. Empty until a backend with migrations has been published.",
            none(),
        ),
        def(
            "load_skill",
            "Load the full instructions of a skill from the skills index. Do this before starting a task the skill covers.",
            schema(
                json!({ "name": { "type": "string", "description": "Skill name from the index, for example create-site" } }),
                &["name"],
            ),
        ),
        def(
            "read_skill_file",
            "Read a file bundled with a skill, such as references/checklist.md or assets/page.html.",
            schema(
                json!({ "name": { "type": "string" }, "path": { "type": "string", "description": "Path inside the skill folder" } }),
                &["name", "path"],
            ),
        ),
        def(
            "check_domain",
            "Check whether a hostname resolves in DNS and whether requests for it reach this server.",
            schema(json!({ "hostname": { "type": "string" } }), &["hostname"]),
        ),
    ]
}

/// Run one tool call. Never panics and never returns an `Err`: every failure becomes an
/// error result the model can read and react to.
pub async fn execute(ctx: &ToolContext, call: &ToolCall) -> ToolOutcome {
    if let Some(raw) = call.arguments.get("_invalid_json") {
        return ToolOutcome::error(format!(
            "the arguments were not valid JSON (they may have been cut off): {}",
            crate::events::shorten(raw.as_str().unwrap_or(""))
        ));
    }
    let args = &call.arguments;
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let required = |key: &str| {
        text(key)
            .ok_or_else(|| ToolOutcome::error(format!("missing required string argument {key:?}")))
    };

    let result: Result<ToolOutcome, ToolOutcome> = async {
        Ok(match call.name.as_str() {
            "list_sites" => {
                let sites = ctx.store.list(false).await.map_err(fail)?;
                let rows: Vec<Value> = sites
                    .iter()
                    .map(|s| {
                        json!({
                            "name": s.name.as_str(),
                            "title": s.title,
                            "published": s.current_release.is_some(),
                            "url": ctx.store.domains().public_url(&ctx.store.domains().site_hostname(&s.name)),
                        })
                    })
                    .collect();
                ToolOutcome::ok(json!({ "sites": rows }))
            }
            "site_info" => {
                let info = ctx.store.info(&ctx.site).await.map_err(fail)?;
                let releases: Vec<Value> = info
                    .releases
                    .iter()
                    .take(20)
                    .map(|r| json!({ "sha": &r.sha[..r.sha.len().min(8)], "message": r.message, "author": r.author, "live": Some(r.id) == info.site.current_release }))
                    .collect();
                ToolOutcome::ok(json!({
                    "name": info.site.name.as_str(),
                    "title": info.site.title,
                    "url": info.url,
                    "hostnames": info.hostnames.iter().map(|h| h.hostname.as_str()).collect::<Vec<_>>(),
                    "published": info.site.current_release.is_some(),
                    "releases": releases,
                }))
            }
            "create_site" => {
                let name = required("name")?;
                let title = required("title")?;
                if !ctx.may_create {
                    return Err(ToolOutcome::error(format!("the site {:?} already exists; edit it with the file tools", ctx.site)));
                }
                if name != ctx.site {
                    return Err(ToolOutcome::error(format!("this conversation can only create the site named {:?}", ctx.site)));
                }
                ctx.store.create(name, title, Author::Agent).await.map_err(fail)?;
                if ctx.containerize_on_create {
                    // The owner's choice on the "new site" form.
                    ctx.store.set_containerized(name, true, Author::User).await.map_err(fail)?;
                }
                let info = ctx.store.info(name).await.map_err(fail)?;
                let mut outcome = ToolOutcome::ok(json!({ "created": name, "url": info.url, "next": "write the files, then validate_site and publish_site" }));
                outcome.effect = Some(Effect::Created);
                outcome
            }
            "list_files" => {
                let files = ctx.store.list_files(&ctx.site).await.map_err(fail)?;
                ToolOutcome::ok(json!({ "files": files }))
            }
            "read_file" => {
                let path = required("path")?;
                let data = ctx.store.read_file(&ctx.site, path).await.map_err(fail)?;
                if data.len() > MAX_READ_BYTES {
                    return Err(ToolOutcome::error(format!("{path} is {} bytes, too large to read", data.len())));
                }
                match String::from_utf8(data) {
                    Ok(content) => ToolOutcome::ok(json!({ "path": path, "content": content })),
                    Err(_) => ToolOutcome::error(format!("{path} is a binary file and cannot be read as text")),
                }
            }
            "write_file" => {
                let path = required("path")?;
                let content = required("content")?;
                let entry = ctx.store.write_file(&ctx.site, path, content.as_bytes().to_vec()).await.map_err(fail)?;
                ToolOutcome::ok(json!({ "written": entry.path, "bytes": entry.size }))
            }
            "delete_file" => {
                let path = required("path")?;
                ctx.store.delete_file(&ctx.site, path).await.map_err(fail)?;
                ToolOutcome::ok(json!({ "deleted": path }))
            }
            "validate_site" => {
                let report = ctx.store.validate(&ctx.site).await.map_err(fail)?;
                ToolOutcome::ok(json!({
                    "ok": report.ok(),
                    "files": report.files,
                    "bytes": report.bytes,
                    "problems": report.problems,
                    "next": if report.ok() { "ready to publish" } else { "fix every error, then validate again" },
                }))
            }
            "publish_site" => {
                let message = text("message").unwrap_or("Update site");
                let outcome = ctx.store.publish(&ctx.site, message, Author::Agent).await.map_err(fail)?;
                ToolOutcome::ok(json!({
                    "published": outcome.changed,
                    "note": if outcome.changed { "the new release is live" } else { "nothing changed since the live release" },
                    "release": &outcome.release.sha[..outcome.release.sha.len().min(8)],
                    "url": outcome.url,
                }))
            }
            "rollback_site" => {
                let release = ctx.store.rollback(&ctx.site, text("release"), Author::Agent).await.map_err(fail)?;
                ToolOutcome::ok(json!({
                    "live_release": &release.sha[..release.sha.len().min(8)],
                    "message": release.message,
                    "note": "the working tree now matches this release",
                }))
            }
            "delete_site" => {
                // Check the site exists, so the user is not asked about nothing.
                ctx.store.get(&ctx.site).await.map_err(fail)?;
                let mut outcome = ToolOutcome::ok(json!({
                    "status": "needs_confirmation",
                    "note": "The user has been asked to confirm by typing the site name. Nothing has been deleted. \
                             Tell them: the site goes offline at once, can be restored for a few days, and its DNS records are left in place.",
                }));
                outcome.effect = Some(Effect::ConfirmDelete);
                outcome
            }
            "check_domain" => {
                let hostname = required("hostname")?;
                ToolOutcome::ok(serde_json::to_value(ctx.domains.check(hostname, ctx.http_port).await).unwrap_or(Value::Null))
            }
            "export_site" => {
                let format = sw_sites::ExportFormat::parse(required("format")?).map_err(fail)?;
                let export = ctx.store.export(&ctx.site, format).await.map_err(fail)?;
                ToolOutcome::ok(json!({
                    "file": export.file_name,
                    "bytes": export.bytes,
                    "files": export.files,
                    "release": &export.sha[..export.sha.len().min(8)],
                    "download": format!("the Export buttons on the site page, or `sitewright export {} --format {}`", ctx.site, format.as_str()),
                }))
            }
            "test_backend" => {
                let backends = ctx.backends.as_ref().ok_or_else(no_backends)?;
                let method = required("method")?.to_ascii_uppercase();
                let target = required("path")?;
                if !target.starts_with("/api") {
                    return Err(ToolOutcome::error("backends answer paths under /api/ only"));
                }
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let site = ctx.store.get(&ctx.site).await.map_err(fail)?;
                let root = ctx.store.working_tree(&ctx.site).await.map_err(fail)?;
                let source = tokio::task::spawn_blocking(move || sw_backend::BackendSource::read(&root))
                    .await
                    .map_err(|e| ToolOutcome::error(e.to_string()))?
                    .map_err(ToolOutcome::error)?
                    .ok_or_else(|| ToolOutcome::error("this site has no backend/index.js yet; write it first"))?;
                let body = text("body").unwrap_or("").to_owned();
                let mut headers = vec![("x-sitewright-visitor".to_owned(), "test-visitor".to_owned())];
                if !body.is_empty() {
                    headers.push((
                        "content-type".to_owned(),
                        text("content_type").unwrap_or("application/json").to_owned(),
                    ));
                }
                if let Some(cookie) = text("cookie").filter(|c| !c.is_empty()) {
                    headers.push(("cookie".to_owned(), cookie.to_owned()));
                }
                let request = sw_backend::BackendRequest {
                    method,
                    path: path.to_owned(),
                    query: query.to_owned(),
                    headers,
                    body,
                };
                let reset = args.get("reset").and_then(Value::as_bool).unwrap_or(false);
                let outcome = backends
                    .test_draft(site.id, &site.name, source, request, reset)
                    .await
                    .map_err(ToolOutcome::error)?;
                ToolOutcome::ok(json!({
                    "status": outcome.status,
                    "headers": outcome.headers,
                    "body": crate::events::shorten(&outcome.body),
                    "console": outcome.logs.iter().map(|l| format!("[{}] {}", l.level, l.message)).collect::<Vec<_>>(),
                    "migrations_applied": outcome.migrations_applied,
                }))
            }
            "backend_logs" => {
                let backends = ctx.backends.as_ref().ok_or_else(no_backends)?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1, 200) as usize;
                let site = ctx.store.get(&ctx.site).await.map_err(fail)?;
                // Secret values are replaced by their names before the model sees a line.
                ToolOutcome::ok(json!({ "lines": backends.logs_redacted(site.id, limit) }))
            }
            "backend_access" => {
                let backends = ctx.backends.as_ref().ok_or_else(no_backends)?;
                let site = ctx.store.get(&ctx.site).await.map_err(fail)?;
                ToolOutcome::ok(json!({
                    "secrets": backends.secret_names(site.id).map_err(ToolOutcome::error)?,
                    "hosts": backends.hosts(site.id).await.map_err(ToolOutcome::error)?,
                    "note": "Read a secret in a handler with `await secrets.get(\"NAME\")`. Only the owner can add secrets or approve hosts, on the site's page in the portal.",
                }))
            }
            "list_models" => {
                let backends = ctx.backends.as_ref().ok_or_else(no_backends)?;
                let site = ctx.store.get(&ctx.site).await.map_err(fail)?;
                let llm = backends.llm();
                let settings = llm.settings(site.id).await.map_err(ToolOutcome::error)?;
                let allowed = llm.allowed(site.id).await.map_err(ToolOutcome::error)?;
                let models: Vec<Value> = llm
                    .available()
                    .await
                    .map_err(ToolOutcome::error)?
                    .into_iter()
                    .map(|m| {
                        json!({
                            "name": m.name,
                            "provider": m.kind,
                            "model": m.model,
                            "server_default": m.is_default,
                            "this_site_may_use_it": allowed.iter().any(|a| a.name == m.name),
                        })
                    })
                    .collect();
                ToolOutcome::ok(json!({
                    "models": models,
                    "ai_enabled_for_this_site": settings.enabled,
                    "site_default": allowed.first().map(|m| m.name.clone()),
                    "note": "Only the owner can turn AI access on for a site or change which models it may use, under \"Backend access\" on the site's page. If the model the user wants is not allowed, write the code for it anyway and tell them to allow it there.",
                }))
            }
            "request_backend_host" => {
                let backends = ctx.backends.as_ref().ok_or_else(no_backends)?;
                let site = ctx.store.get(&ctx.site).await.map_err(fail)?;
                let host = backends
                    .request_host(site.id, required("host")?, required("reason")?)
                    .await
                    .map_err(ToolOutcome::error)?;
                let approved = backends
                    .hosts(site.id)
                    .await
                    .map_err(ToolOutcome::error)?
                    .iter()
                    .any(|h| h.host == host && h.approved);
                ToolOutcome::ok(json!({
                    "host": host,
                    "approved": approved,
                    "next": if approved {
                        "already approved; fetch will work"
                    } else {
                        "waiting for the owner; tell them in your summary to approve this host on the site's page, and that the feature will not work until they do"
                    },
                }))
            }
            "backend_schema" => {
                let backends = ctx.backends.as_ref().ok_or_else(no_backends)?;
                let site = ctx.store.get(&ctx.site).await.map_err(fail)?;
                let schema = backends.schema(&site.name).await.map_err(ToolOutcome::error)?;
                ToolOutcome::ok(json!({
                    "objects": schema,
                    "database_bytes": backends.database_size(&site.name),
                    "note": "This is the published database. Migrations in the working tree apply when you publish; test_backend applies them to a private copy.",
                }))
            }
            "metrics_schema" => ToolOutcome::ok(sw_metrics::dashboard::metrics_schema()),
            "run_readonly_query" => {
                let sql = required("sql")?;
                let days = args.get("days").and_then(Value::as_i64).unwrap_or(30).clamp(1, 90);
                let site = ctx.store.get(&ctx.site).await.map_err(fail)?;
                let source = match args.get("source") {
                    None | Some(Value::Null) => Source::Metrics,
                    Some(value) => Source::parse(value.as_str().unwrap_or_default()).map_err(ToolOutcome::error)?,
                };
                let (from, to) = sw_metrics::dashboard::range(days);
                let site_data = site_data(ctx, &site.name);
                let mut result = sw_metrics::dashboard::query(
                    &ctx.metrics.sandbox,
                    site_data.as_ref().map(|data| data as &dyn SiteData),
                    site.id,
                    source,
                    sql,
                    (&from, &to),
                )
                .await
                .map_err(ToolOutcome::error)?;
                let mut range = json!({ "from": from, "to": to });
                if source == Source::Site {
                    // Rows of the site's own tables may hold anything its backend stored,
                    // so they get the same treatment as the backend's other output.
                    if let Some(backends) = &ctx.backends {
                        result.rows.iter_mut().for_each(|row| redact_strings(backends, site.id, row));
                    }
                    range["from_ms"] = json!(sw_metrics::dashboard::iso_to_ms(&from));
                    range["to_ms"] = json!(sw_metrics::dashboard::iso_to_ms(&to));
                }
                ToolOutcome::ok(json!({
                    "columns": result.columns,
                    "rows": result.rows,
                    "row_count": result.rows.len(),
                    "truncated": result.truncated,
                    "range": range,
                }))
            }
            "save_dashboard" => {
                let spec = args.get("spec").filter(|s| s.is_object()).ok_or_else(|| ToolOutcome::error("missing required object argument \"spec\""))?;
                let site = ctx.store.get(&ctx.site).await.map_err(fail)?;
                let site_data = site_data(ctx, &site.name);
                let site_data = site_data.as_ref().map(|data| data as &dyn SiteData);
                match sw_metrics::dashboard::validate(&ctx.metrics.sandbox, site_data, site.id, spec).await {
                    Ok(parsed) => {
                        let version = sw_metrics::dashboard::save(&ctx.metrics.db, site.id, &parsed).await.map_err(|e| {
                            tracing::error!(error = %e, "could not save a dashboard");
                            ToolOutcome::error("the server could not save the dashboard")
                        })?;
                        ToolOutcome::ok(json!({ "saved": true, "panels": parsed.panels.len(), "version": version }))
                    }
                    Err(problems) => ToolOutcome {
                        content: json!({ "error": "the dashboard was not saved; fix these problems and try again", "problems": problems }).to_string(),
                        is_error: true,
                        effect: None,
                    },
                }
            }
            "load_skill" => {
                let name = required("name")?;
                let (instructions, files) = ctx.skills.load(name).map_err(ToolOutcome::error)?;
                ToolOutcome::ok(json!({ "skill": name, "instructions": instructions, "files": files }))
            }
            "read_skill_file" => {
                let name = required("name")?;
                let path = required("path")?;
                let content = ctx.skills.read_file(name, path).map_err(ToolOutcome::error)?;
                ToolOutcome::ok(json!({ "skill": name, "path": path, "content": content }))
            }
            other => ToolOutcome::error(format!("there is no tool named {other:?}")),
        })
    }
    .await;
    result.unwrap_or_else(|outcome| outcome)
}

/// The run's site's own database, for dashboard panels with `"source": "site"`. Bound to
/// that one site; `None` when the server has no backends.
fn site_data(ctx: &ToolContext, site: &sw_core::SiteName) -> Option<SiteQueries> {
    ctx.backends
        .as_ref()
        .map(|backends| SiteQueries::new(backends.clone(), site.clone()))
}

/// Replace the site's secret values in every string of `value`.
fn redact_strings(backends: &sw_backend::SiteBackends, site_id: i64, value: &mut Value) {
    match value {
        Value::String(text) => *text = backends.redact(site_id, text),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_strings(backends, site_id, item)),
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| redact_strings(backends, site_id, item)),
        _ => {}
    }
}

/// Turn a store error into a result the model can act on. Server-side failures are
/// logged in full but described to the model only in general terms.
fn fail(error: SiteError) -> ToolOutcome {
    if error.is_user_error() || matches!(error, SiteError::HealthCheck { .. }) {
        ToolOutcome::error(error.to_string())
    } else {
        tracing::error!(%error, "tool failed with a server error");
        ToolOutcome::error(
            "the server hit an internal error running this tool; try once more, then stop and tell the user",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_strict_object_schema() {
        let defs = definitions();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "list_sites",
                "site_info",
                "create_site",
                "list_files",
                "read_file",
                "write_file",
                "delete_file",
                "validate_site",
                "publish_site",
                "rollback_site",
                "delete_site",
                "export_site",
                "metrics_schema",
                "run_readonly_query",
                "save_dashboard",
                "test_backend",
                "backend_logs",
                "backend_access",
                "list_models",
                "request_backend_host",
                "backend_schema",
                "load_skill",
                "read_skill_file",
                "check_domain"
            ]
        );
        for def in &defs {
            assert_eq!(def.parameters["type"], "object", "{}", def.name);
            assert_eq!(
                def.parameters["additionalProperties"], false,
                "{}",
                def.name
            );
            assert!(!def.description.is_empty());
            for required in def.parameters["required"].as_array().unwrap() {
                assert!(
                    def.parameters["properties"]
                        .get(required.as_str().unwrap())
                        .is_some(),
                    "{}",
                    def.name
                );
            }
        }
        // The agent must never get a shell or a network tool.
        for forbidden in ["shell", "exec", "bash", "fetch", "http", "browse"] {
            assert!(!names.iter().any(|n| n.contains(forbidden)), "{forbidden}");
        }
    }
}
