//! Backends for Sitewright's sites: the glue between the worker manager, the per-site
//! databases and the site store.
//!
//! - Serving: a live release's `backend/` answers the site's `/api/*` requests.
//! - Publishing: before a release goes live its code must load and its migrations must
//!   apply; the database is copied first and put back if the release is rolled back.
//! - Drafts: the agent can try unpublished code against a copy of the site's data.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sw_core::config::BackendsConfig;
use sw_core::{DataDir, SiteName};
use sw_sites::{ReleaseHooks, ReleasePlan};

use crate::egress::{Egress, FetchRequest, normalize_host};
use crate::manager::{
    BackendError, BackendRequest, BackendResponse, Backends, Host, Limits, Target, WorkerCommand,
};
use crate::secrets::SiteSecrets;
use crate::sitedb::{DbLimits, Migration, SiteDb, SiteDbs};

/// Console lines kept per site.
const LOG_LINES: usize = 200;
/// Longest console line kept.
const LOG_LINE_BYTES: usize = 2000;
/// Most backend source the worker is given, in bytes.
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

/// One line of a backend's console output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLine {
    /// Unix milliseconds.
    pub ts: i64,
    pub level: String,
    pub message: String,
    /// From a draft the agent was testing, not from the live site.
    pub draft: bool,
}

/// A backend's code as read from a directory.
#[derive(Debug, Clone, Default)]
pub struct BackendSource {
    /// JavaScript files by site-relative path (`backend/index.js`, ...).
    pub files: BTreeMap<String, String>,
    pub migrations: Vec<Migration>,
}

impl BackendSource {
    /// Read the backend under `root` (a release directory or a working tree).
    /// `Ok(None)` when there is no `backend/index.js`.
    pub fn read(root: &Path) -> Result<Option<Self>, String> {
        let dir = root.join("backend");
        if !dir.join("index.js").is_file() {
            return Ok(None);
        }
        let mut source = Self::default();
        let mut total = 0u64;
        let mut pending = vec![dir];
        while let Some(dir) = pending.pop() {
            let entries =
                std::fs::read_dir(&dir).map_err(|e| format!("could not read the backend: {e}"))?;
            for entry in entries.flatten() {
                let path = entry.path();
                // Never follow links: a release is plain files and directories.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let Some(rel) = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|p| p.to_str())
                    .map(|p| p.replace('\\', "/"))
                else {
                    continue;
                };
                let is_code = rel.ends_with(".js") || rel.ends_with(".mjs");
                let is_migration = rel.starts_with("backend/migrations/") && rel.ends_with(".sql");
                if !is_code && !is_migration {
                    continue;
                }
                total += entry.metadata().map_or(0, |m| m.len());
                if total > MAX_SOURCE_BYTES {
                    return Err(format!(
                        "the backend is larger than {} MB",
                        MAX_SOURCE_BYTES / (1024 * 1024)
                    ));
                }
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{rel} could not be read as UTF-8 text: {e}"))?;
                if is_migration {
                    source.migrations.push(Migration {
                        name: rel.trim_start_matches("backend/migrations/").to_owned(),
                        sql: text,
                    });
                } else {
                    source.files.insert(rel, text);
                }
            }
        }
        source.migrations.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(source))
    }

    /// A digest of the code and migrations, to tell versions of a draft apart.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        for (path, text) in &self.files {
            hasher.update(path.as_bytes());
            hasher.update([0]);
            hasher.update(text.as_bytes());
            hasher.update([0]);
        }
        for migration in &self.migrations {
            hasher.update(migration.name.as_bytes());
            hasher.update([0]);
            hasher.update(migration.sql.as_bytes());
            hasher.update([0]);
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// What the worker's host calls and logs are served from.
struct SiteHost {
    data: DataDir,
    /// Sitewright's own database, for the per-site list of approved hosts.
    pool: sqlx::SqlitePool,
    secrets: SiteSecrets,
    egress: Egress,
    dbs: SiteDbs,
    logs: Mutex<HashMap<i64, VecDeque<LogLine>>>,
}

impl SiteHost {
    fn db_path(&self, site: &str, draft: bool) -> Result<PathBuf, String> {
        let name = SiteName::parse(site).map_err(|e| e.to_string())?;
        Ok(if draft {
            self.data.site_data(&name).join("draft.db")
        } else {
            self.data.site_db(&name)
        })
    }

    async fn db(&self, target: &Target) -> Result<Arc<SiteDb>, String> {
        self.dbs
            .get(&self.db_path(&target.site, target.draft)?)
            .await
    }
}

/// A host on a site's outbound list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SiteHostEntry {
    pub host: String,
    /// False while the owner has not approved it yet.
    pub approved: bool,
    pub reason: Option<String>,
}

/// Most hosts waiting for approval per site.
const MAX_REQUESTED_HOSTS: i64 = 20;

impl SiteHost {
    async fn host_status(&self, site_id: i64, host: &str) -> Result<Option<String>, String> {
        sqlx::query_scalar!(
            "SELECT status FROM site_hosts WHERE site_id = ?1 AND host = ?2",
            site_id,
            host
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())
    }

    /// Record that a site wants to reach `host`. Does nothing if it is already listed.
    async fn request_host(&self, site_id: i64, host: &str, reason: &str) -> Result<(), String> {
        let waiting = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!: i64" FROM site_hosts WHERE site_id = ?1 AND status = 'requested'"#,
            site_id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        if waiting >= MAX_REQUESTED_HOSTS {
            return Err("too many hosts are already waiting for the owner's approval".into());
        }
        let reason: String = reason.chars().take(300).collect();
        let now = sw_core::time::now_ms();
        sqlx::query!(
            "INSERT INTO site_hosts (site_id, host, status, reason, created_at) VALUES (?1, ?2, 'requested', ?3, ?4)
             ON CONFLICT (site_id, host) DO NOTHING",
            site_id,
            host,
            reason,
            now
        )
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn fetch(&self, target: &Target, args: Value) -> Result<Value, String> {
        let request: FetchRequest =
            serde_json::from_value(args).map_err(|e| format!("invalid fetch request: {e}"))?;
        let host = self.egress.target_host(&request.url)?;
        match self.host_status(target.site_id, &host).await?.as_deref() {
            Some("approved") => {}
            Some(_) => {
                return Err(format!(
                    "{host} is waiting for the site owner's approval; requests to it are refused until then"
                ));
            }
            None => {
                let _ = self
                    .request_host(
                        target.site_id,
                        &host,
                        "the backend tried to reach this host",
                    )
                    .await;
                return Err(format!(
                    "{host} is not on this site's list of approved hosts; the owner has been asked to approve it"
                ));
            }
        }
        let response = self.egress.fetch(request).await?;
        serde_json::to_value(response).map_err(|e| e.to_string())
    }
}

fn statement(args: &Value) -> Result<(&str, &[Value]), String> {
    let sql = args
        .get("sql")
        .and_then(Value::as_str)
        .ok_or("the SQL must be a string")?;
    let params = match args.get("params") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(items)) => items.as_slice(),
        Some(_) => return Err("the parameters must be an array".into()),
    };
    Ok((sql, params))
}

impl Host for SiteHost {
    fn call<'a>(
        &'a self,
        target: &'a Target,
        op: &'a str,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            match op {
                "db.query" => {
                    let (sql, params) = statement(&args)?;
                    Ok(Value::Array(
                        self.db(target).await?.query(sql, params).await?,
                    ))
                }
                "db.run" => {
                    let (sql, params) = statement(&args)?;
                    let result = self.db(target).await?.run(sql, params).await?;
                    serde_json::to_value(result).map_err(|e| e.to_string())
                }
                "db.batch" => {
                    let list = args
                        .get("statements")
                        .and_then(Value::as_array)
                        .ok_or("db.batch takes a list of statements")?;
                    if list.len() > 200 {
                        return Err("db.batch takes at most 200 statements".into());
                    }
                    let mut statements = Vec::with_capacity(list.len());
                    for item in list {
                        let (sql, params) = statement(item)?;
                        statements.push((sql.to_owned(), params.to_vec()));
                    }
                    let results = self.db(target).await?.batch(&statements).await?;
                    serde_json::to_value(results).map_err(|e| e.to_string())
                }
                "secrets.get" => {
                    let name = args
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or("secrets.get takes a name")?;
                    match self.secrets.get(target.site_id, name)? {
                        Some(value) => Ok(Value::String(value)),
                        None => Err(format!(
                            "this site has no secret named {name}; the owner adds secrets on the site's page"
                        )),
                    }
                }
                "http.fetch" => self.fetch(target, args).await,
                other => Err(format!("unknown operation {other}")),
            }
        })
    }

    fn log(&self, target: &Target, level: &str, message: &str) {
        let mut message = message.to_owned();
        if message.len() > LOG_LINE_BYTES {
            let mut cut = LOG_LINE_BYTES;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
            message.push('…');
        }
        tracing::debug!(site = %target.site, draft = target.draft, level, "{message}");
        let mut logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        let lines = logs.entry(target.site_id).or_default();
        if lines.len() >= LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(LogLine {
            ts: sw_core::time::now_ms(),
            level: level.to_owned(),
            message,
            draft: target.draft,
        });
    }
}

/// The result of trying a request against a draft backend.
#[derive(Debug, Clone, Serialize)]
pub struct DraftOutcome {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Console output produced while handling the request.
    pub logs: Vec<LogLine>,
    /// Migrations applied to the test database for this request.
    pub migrations_applied: Vec<String>,
}

struct Inner {
    enabled: bool,
    backends: Backends,
    host: Arc<SiteHost>,
    /// Database copies made before a publish, until it commits or is abandoned.
    /// `None` means the site had no database before.
    snapshots: Mutex<HashMap<i64, Option<PathBuf>>>,
    /// The digest of the draft each site's test database was prepared for.
    drafts: tokio::sync::Mutex<HashMap<i64, String>>,
}

/// Site backends. Cheap to clone.
#[derive(Clone)]
pub struct SiteBackends {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SiteBackends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteBackends")
            .field("enabled", &self.inner.enabled)
            .finish_non_exhaustive()
    }
}

fn remove_db_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

impl SiteBackends {
    pub fn new(
        config: &BackendsConfig,
        data: DataDir,
        pool: sqlx::SqlitePool,
        command: WorkerCommand,
    ) -> Self {
        Self::with_egress(config, data, pool, command, Egress::new())
    }

    /// Like [`Self::new`], with a different outbound-request policy. For tests.
    #[doc(hidden)]
    pub fn with_egress(
        config: &BackendsConfig,
        data: DataDir,
        pool: sqlx::SqlitePool,
        command: WorkerCommand,
        egress: Egress,
    ) -> Self {
        let limits = Limits {
            request_timeout: Duration::from_secs(config.request_timeout_secs.max(1)),
            memory_bytes: config.memory_mb.max(8) * 1024 * 1024,
            idle: Duration::from_secs(config.idle_secs.max(1)),
            max_workers: config.max_workers.max(1),
            ..Limits::default()
        };
        let host = Arc::new(SiteHost {
            secrets: SiteSecrets::new(data.site_secrets_file()),
            pool,
            egress,
            data,
            dbs: SiteDbs::new(DbLimits {
                max_bytes: config.max_db_mb.max(1) * 1024 * 1024,
                ..DbLimits::default()
            }),
            logs: Mutex::new(HashMap::new()),
        });
        Self {
            inner: Arc::new(Inner {
                enabled: config.enabled,
                backends: Backends::new(command, limits, host.clone()),
                host,
                snapshots: Mutex::new(HashMap::new()),
                drafts: tokio::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.enabled
    }

    pub fn limits(&self) -> &Limits {
        self.inner.backends.limits()
    }

    /// Answer a request with the backend of the release at `root`.
    pub async fn serve(
        &self,
        site_id: i64,
        site: &SiteName,
        sha: &str,
        root: &Path,
        request: BackendRequest,
    ) -> Result<BackendResponse, BackendError> {
        let target = Target {
            site_id,
            site: site.as_str().to_owned(),
            version: sha.to_owned(),
            draft: false,
        };
        let root = root.to_owned();
        self.inner
            .backends
            .handle(
                &target,
                move || match BackendSource::read(&root)? {
                    Some(source) => Ok(source.files),
                    None => Err("the release has no backend/index.js".into()),
                },
                request,
            )
            .await
    }

    /// Try a request against unpublished code. The draft gets its own worker and its
    /// own database: a copy of the site's live data with the draft's migrations
    /// applied. The copy is kept between calls (so a POST can be followed by a GET)
    /// until the draft's code changes or `reset` is set.
    pub async fn test_draft(
        &self,
        site_id: i64,
        site: &SiteName,
        source: BackendSource,
        request: BackendRequest,
        reset: bool,
    ) -> Result<DraftOutcome, String> {
        if !self.inner.enabled {
            return Err("backends are turned off on this server".into());
        }
        let host = &self.inner.host;
        let digest = source.digest();
        let target = Target {
            site_id,
            site: site.as_str().to_owned(),
            version: digest.clone(),
            draft: true,
        };
        let draft_path = host.db_path(site.as_str(), true)?;
        let mut drafts = self.inner.drafts.lock().await;
        if reset || drafts.get(&site_id) != Some(&digest) || !draft_path.is_file() {
            host.dbs.close(&draft_path).await;
            remove_db_files(&draft_path);
            let live_path = host.db_path(site.as_str(), false)?;
            if live_path.is_file() {
                host.dbs.get(&live_path).await?.copy_to(&draft_path).await?;
            }
            drafts.insert(site_id, digest);
        }
        let migrations_applied = host
            .dbs
            .get(&draft_path)
            .await?
            .migrate(&source.migrations)
            .await?;
        drop(drafts);

        let before = self.log_count(site_id);
        let files = source.files;
        let response = self
            .inner
            .backends
            .handle(&target, move || Ok(files), request)
            .await
            .map_err(|e| e.to_string());
        let logs: Vec<LogLine> = self
            .logs(site_id, LOG_LINES)
            .into_iter()
            .skip(before.min(LOG_LINES))
            .filter(|l| l.draft)
            .collect();
        // What comes back from here goes to the model: no secret values.
        let redact = |text: &str| host.secrets.redact(site_id, text);
        let logs: Vec<LogLine> = logs
            .into_iter()
            .map(|l| LogLine {
                message: redact(&l.message),
                ..l
            })
            .collect();
        let response = response.map_err(|e| redact(&e));
        match response {
            Ok(response) => Ok(DraftOutcome {
                status: response.status,
                headers: response
                    .headers
                    .into_iter()
                    .map(|(k, v)| (k, redact(&v)))
                    .collect(),
                body: redact(&response.body),
                logs,
                migrations_applied,
            }),
            Err(error) => {
                let console: Vec<String> = logs
                    .iter()
                    .map(|l| format!("[{}] {}", l.level, l.message))
                    .collect();
                if console.is_empty() {
                    Err(error)
                } else {
                    Err(format!("{error}\nconsole:\n{}", console.join("\n")))
                }
            }
        }
    }

    fn log_count(&self, site_id: i64) -> usize {
        self.inner
            .host
            .logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&site_id)
            .map_or(0, VecDeque::len)
    }

    /// The newest `limit` console lines of a site's backend, oldest first.
    pub fn logs(&self, site_id: i64, limit: usize) -> Vec<LogLine> {
        let logs = self
            .inner
            .host
            .logs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(lines) = logs.get(&site_id) else {
            return Vec::new();
        };
        lines
            .iter()
            .skip(lines.len().saturating_sub(limit))
            .cloned()
            .collect()
    }

    /// Like [`Self::logs`], with the site's secret values replaced by their names. Use
    /// this wherever the lines leave the server's own memory.
    pub fn logs_redacted(&self, site_id: i64, limit: usize) -> Vec<LogLine> {
        self.logs(site_id, limit)
            .into_iter()
            .map(|l| LogLine {
                message: self.inner.host.secrets.redact(site_id, &l.message),
                ..l
            })
            .collect()
    }

    /// The names of a site's secrets. Values are never returned.
    pub fn secret_names(&self, site_id: i64) -> Result<Vec<String>, String> {
        self.inner.host.secrets.names(site_id)
    }

    pub fn set_secret(&self, site_id: i64, name: &str, value: &str) -> Result<(), String> {
        self.inner.host.secrets.set(site_id, name, value)
    }

    pub fn remove_secret(&self, site_id: i64, name: &str) -> Result<bool, String> {
        self.inner.host.secrets.remove(site_id, name)
    }

    /// Forget the secrets of sites that have been purged. Call at start-up and after
    /// purging.
    pub async fn prune_secrets(&self) -> Result<usize, String> {
        let existing = sqlx::query_scalar!(r#"SELECT id as "id!" FROM sites"#)
            .fetch_all(&self.inner.host.pool)
            .await
            .map_err(|e| e.to_string())?;
        self.inner.host.secrets.retain_sites(&existing)
    }

    /// The hosts a site's backend may reach or has asked for, approved first.
    pub async fn hosts(&self, site_id: i64) -> Result<Vec<SiteHostEntry>, String> {
        let rows = sqlx::query!(
            "SELECT host, status, reason FROM site_hosts WHERE site_id = ?1 ORDER BY status, host",
            site_id
        )
        .fetch_all(&self.inner.host.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| SiteHostEntry {
                host: r.host,
                approved: r.status == "approved",
                reason: r.reason,
            })
            .collect())
    }

    /// Ask the owner to approve a host for a site (what the agent can do).
    pub async fn request_host(
        &self,
        site_id: i64,
        host: &str,
        reason: &str,
    ) -> Result<String, String> {
        let host = normalize_host(host).ok_or_else(|| {
            format!("{host:?} is not a host name; give a plain name such as api.example.com")
        })?;
        self.inner.host.request_host(site_id, &host, reason).await?;
        Ok(host)
    }

    /// Approve a host for a site (the owner's decision).
    pub async fn approve_host(&self, site_id: i64, host: &str) -> Result<String, String> {
        let host = normalize_host(host).ok_or_else(|| {
            format!("{host:?} is not a host name; give a plain name such as api.example.com")
        })?;
        let now = sw_core::time::now_ms();
        sqlx::query!(
            "INSERT INTO site_hosts (site_id, host, status, created_at) VALUES (?1, ?2, 'approved', ?3)
             ON CONFLICT (site_id, host) DO UPDATE SET status = 'approved'",
            site_id,
            host,
            now
        )
        .execute(&self.inner.host.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(host)
    }

    /// Remove a host from a site's list, approved or not.
    pub async fn remove_host(&self, site_id: i64, host: &str) -> Result<bool, String> {
        let removed = sqlx::query!(
            "DELETE FROM site_hosts WHERE site_id = ?1 AND host = ?2",
            site_id,
            host
        )
        .execute(&self.inner.host.pool)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
        Ok(removed > 0)
    }

    /// The live database's tables, or an empty list when the site has no database yet.
    pub async fn schema(&self, site: &SiteName) -> Result<Vec<Value>, String> {
        let path = self.inner.host.db_path(site.as_str(), false)?;
        if !path.is_file() {
            return Ok(Vec::new());
        }
        self.inner.host.dbs.get(&path).await?.schema().await
    }

    /// Whether the site has a database, and its size in bytes.
    pub fn database_size(&self, site: &SiteName) -> Option<u64> {
        let path = self.inner.host.db_path(site.as_str(), false).ok()?;
        std::fs::metadata(path).ok().map(|m| m.len())
    }

    /// Let go of a site's workers and database connections, before its directory is
    /// moved or removed.
    pub async fn release(&self, site_id: i64, site: &SiteName) {
        self.inner.backends.stop(site_id).await;
        for draft in [false, true] {
            if let Ok(path) = self.inner.host.db_path(site.as_str(), draft) {
                self.inner.host.dbs.close(&path).await;
            }
        }
        self.inner.drafts.lock().await.remove(&site_id);
    }

    /// Stop workers that have been idle too long. Call periodically.
    pub fn reap_idle(&self) -> usize {
        self.inner.backends.reap_idle()
    }

    /// How many workers are alive.
    pub fn workers(&self) -> usize {
        self.inner.backends.alive()
    }

    fn snapshot_path(live: &Path) -> PathBuf {
        live.with_file_name("site.db.before-publish")
    }

    async fn restore(&self, site_id: i64, site: &SiteName) {
        let Some(snapshot) = self
            .inner
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&site_id)
        else {
            return;
        };
        let Ok(live) = self.inner.host.db_path(site.as_str(), false) else {
            return;
        };
        self.inner.backends.stop(site_id).await;
        self.inner.host.dbs.close(&live).await;
        remove_db_files(&live);
        if let Some(snapshot) = snapshot
            && let Err(e) = std::fs::rename(&snapshot, &live)
        {
            tracing::error!(site = %site, error = %e, snapshot = %snapshot.display(),
                "could not restore the site database after a failed publish");
        }
    }
}

impl ReleaseHooks for SiteBackends {
    fn prepare<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let Some(source) = BackendSource::read(&plan.root)? else {
                return Ok(());
            };
            if !self.inner.enabled {
                return Err(
                    "this site has a backend, but backends are turned off on this server \
                     ([backends] enabled = false)"
                        .into(),
                );
            }
            let host = &self.inner.host;
            let target = Target {
                site_id: plan.site_id,
                site: plan.site.as_str().to_owned(),
                version: plan.sha.clone(),
                draft: true,
            };
            // 1. The code must load.
            self.inner
                .backends
                .check(&target, source.files)
                .await
                .map_err(|e| e.to_string())?;

            // 2. Copy the database, then migrate it. The copy is what a failed publish
            //    goes back to.
            let live = host.db_path(plan.site.as_str(), false)?;
            let snapshot = if live.is_file() {
                let path = Self::snapshot_path(&live);
                let _ = std::fs::remove_file(&path);
                host.dbs.get(&live).await?.copy_to(&path).await?;
                Some(path)
            } else {
                None
            };
            self.inner
                .snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(plan.site_id, snapshot);
            let migrated = match host.dbs.get(&live).await {
                Ok(db) => db.migrate(&source.migrations).await,
                Err(e) => Err(e),
            };
            if let Err(e) = migrated {
                self.restore(plan.site_id, &plan.site).await;
                return Err(e);
            }
            Ok(())
        })
    }

    fn abandon<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, ()> {
        Box::pin(self.restore(plan.site_id, &plan.site))
    }

    fn commit<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // The copy stays on disk as the last pre-publish backup; only forget it.
            self.inner
                .snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&plan.site_id);
        })
    }
}
