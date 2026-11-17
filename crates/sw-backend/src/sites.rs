//! Backends for Sitewright's sites: the glue between the worker manager, the per-site
//! databases and the site store.
//!
//! - Serving: a live release's `backend/` answers the site's `/api/*` requests.
//! - Publishing: before a release goes live its code must load and its migrations must
//!   apply; the database is copied first and put back if the release is rolled back.
//! - Drafts: the agent can try unpublished code against a copy of the site's data.
//! - Background work: queued jobs and scheduled tasks of the live release run in the
//!   site's background worker, with the same host calls (and so the same access) as its
//!   request handlers.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
use crate::jobs::{Cron, JobRecord, JobStore, NewJob, ScheduleInfo};
use crate::manager::{
    BackendError, BackendRequest, BackendResponse, Backends, Host, Limits, Target, WorkerCommand,
};
use crate::protocol::{Declared, DeclaredSchedule, TaskKind, TaskMsg};
use crate::secrets::SiteSecrets;
use crate::sitedb::{DbLimits, Migration, ReadRows, SiteDb, SiteDbs};

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
    /// From a background task (a job or a scheduled run), not from a request.
    pub background: bool,
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
    auth: crate::auth::Auth,
    llm: crate::llm::LlmGateway,
    dbs: SiteDbs,
    logs: Mutex<HashMap<i64, VecDeque<LogLine>>>,
    jobs: JobStore,
    /// Whether background jobs and schedules may run on this server.
    jobs_enabled: bool,
    /// What the most recently started worker of each (site, draft) declared, with the
    /// version of the code it ran.
    declared: Mutex<HashMap<(i64, bool), (String, Declared)>>,
    /// Jobs a draft enqueued while the agent tested it. They are reported, not run.
    draft_jobs: Mutex<HashMap<i64, Vec<Value>>>,
}

/// Most jobs remembered per draft test.
const MAX_DRAFT_JOBS: usize = 50;

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

    fn declared_for(&self, target: &Target) -> Option<Declared> {
        self.declared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(target.site_id, target.draft))
            .filter(|(version, _)| *version == target.version)
            .map(|(_, declared)| declared.clone())
    }

    /// `jobs.enqueue`: queue a job for this site. A draft's jobs are only recorded, for
    /// the agent to see and run by hand.
    async fn enqueue(&self, target: &Target, args: &Value) -> Result<Value, String> {
        if !self.jobs_enabled {
            return Err("background jobs are turned off on this server".into());
        }
        let job = NewJob::from_args(args)?;
        if let Some(declared) = self.declared_for(target)
            && !declared.jobs.contains(&job.name)
        {
            return Err(format!(
                "there is no job named {0}; declare it at the top level with jobs.define(\"{0}\", async (payload) => {{ ... }})",
                job.name
            ));
        }
        if target.draft {
            let mut drafts = self.draft_jobs.lock().unwrap_or_else(|e| e.into_inner());
            let list = drafts.entry(target.site_id).or_default();
            if list.len() < MAX_DRAFT_JOBS {
                list.push(serde_json::json!({
                    "name": job.name,
                    "payload": serde_json::from_str::<Value>(&job.payload).unwrap_or(Value::Null),
                    "delaySeconds": job.delay_ms / 1000,
                    "attempts": job.attempts,
                }));
            }
            return Ok(serde_json::json!({ "id": null, "test": true }));
        }
        let id = self.jobs.enqueue(target.site_id, &job).await?;
        Ok(serde_json::json!({ "id": id }))
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
                "llm.chat" => self.llm.chat(target.site_id, &args).await,
                "jobs.enqueue" => self.enqueue(target, &args).await,
                op if op.starts_with("auth.") => {
                    let db = self.db(target).await?;
                    self.auth.call(target.site_id, &db, op, &args).await
                }
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
            background: target.background,
        });
    }

    fn loaded(&self, target: &Target, tasks: &Declared) {
        self.declared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (target.site_id, target.draft),
                (target.version.clone(), tasks.clone()),
            );
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
    /// The jobs and schedules the draft's code declares.
    pub tasks: Declared,
    /// Why the declared jobs or schedules would stop a publish, if they would.
    pub tasks_problem: Option<String>,
    /// Jobs the code enqueued while handling this. In a test they are not run; run one
    /// with [`DraftTask::Job`].
    pub jobs_enqueued: Vec<Value>,
}

/// A background task to try against a draft.
#[derive(Debug, Clone)]
pub enum DraftTask {
    Job { name: String, payload: Value },
    Schedule { name: String },
}

/// What a draft test runs.
enum DraftWork {
    Request(BackendRequest),
    Task(DraftTask),
}

/// A site whose live release has a backend: what the scheduler works through.
#[derive(Debug, Clone)]
pub struct LiveBackend {
    pub site_id: i64,
    pub site: SiteName,
    pub sha: String,
    /// The release directory.
    pub root: PathBuf,
}

/// The scheduled tasks of a site's live release, and when each runs next.
struct LiveTasks {
    sha: String,
    declared: Declared,
    schedules: Vec<(DeclaredSchedule, Cron, i64)>,
}

impl LiveTasks {
    fn new(sha: String, declared: Declared, now: i64) -> Self {
        let schedules = declared
            .schedules
            .iter()
            .filter_map(|s| {
                let cron = Cron::parse(&s.cron).ok()?;
                let next = cron.next_after(now)?;
                Some((s.clone(), cron, next))
            })
            .collect();
        Self {
            sha,
            declared,
            schedules,
        }
    }
}

/// Removes a site from the set of sites with background work in progress, also when
/// the work panics.
struct Running {
    set: Arc<Mutex<HashSet<i64>>>,
    site_id: i64,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.site_id);
    }
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
    /// What a release being published declared, from `prepare` until `commit`.
    publishing: Mutex<HashMap<i64, (String, Declared)>>,
    /// The scheduled tasks of each site's live release.
    live_tasks: tokio::sync::Mutex<HashMap<i64, LiveTasks>>,
    /// Sites whose background work is in progress.
    running: Arc<Mutex<HashSet<i64>>>,
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

    /// For a site running on its own, outside Sitewright (in a container): secrets are
    /// read from the process environment, where whoever runs the container put them, and
    /// only those named in `secret_names`.
    pub fn standalone(
        config: &BackendsConfig,
        data: DataDir,
        pool: sqlx::SqlitePool,
        command: WorkerCommand,
        secret_names: Vec<String>,
    ) -> Self {
        Self::build(
            config,
            data,
            pool,
            command,
            Egress::new(),
            SiteSecrets::from_env(secret_names),
        )
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
        let secrets = SiteSecrets::new(data.site_secrets_file());
        Self::build(config, data, pool, command, egress, secrets)
    }

    fn build(
        config: &BackendsConfig,
        data: DataDir,
        pool: sqlx::SqlitePool,
        command: WorkerCommand,
        egress: Egress,
        secrets: SiteSecrets,
    ) -> Self {
        // Cookies for visitor sessions are `Secure` exactly when sites are on HTTPS.
        let secure = config.secure_cookies;
        let limits = Limits {
            request_timeout: Duration::from_secs(config.request_timeout_secs.max(1)),
            memory_bytes: config.memory_mb.max(8) * 1024 * 1024,
            idle: Duration::from_secs(config.idle_secs.max(1)),
            max_workers: config.max_workers.max(1),
            task_timeout: Duration::from_secs(config.job_timeout_secs.max(1)),
            ..Limits::default()
        };
        let host = Arc::new(SiteHost {
            secrets,
            llm: crate::llm::LlmGateway::new(pool.clone()),
            jobs: JobStore::new(pool.clone()),
            jobs_enabled: config.enabled && config.jobs,
            declared: Mutex::new(HashMap::new()),
            draft_jobs: Mutex::new(HashMap::new()),
            pool,
            egress,
            auth: crate::auth::Auth::new(secure),
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
                publishing: Mutex::new(HashMap::new()),
                live_tasks: tokio::sync::Mutex::new(HashMap::new()),
                running: Arc::new(Mutex::new(HashSet::new())),
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
            background: false,
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
        self.draft(site_id, site, source, DraftWork::Request(request), reset)
            .await
    }

    /// Run a job or a scheduled task of unpublished code once, now, the way
    /// [`Self::test_draft`] runs a request. The outcome's status is 200 when the task
    /// finished and 500 when it threw; the body then holds the error.
    pub async fn test_draft_task(
        &self,
        site_id: i64,
        site: &SiteName,
        source: BackendSource,
        task: DraftTask,
        reset: bool,
    ) -> Result<DraftOutcome, String> {
        self.draft(site_id, site, source, DraftWork::Task(task), reset)
            .await
    }

    async fn draft(
        &self,
        site_id: i64,
        site: &SiteName,
        source: BackendSource,
        work: DraftWork,
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
            background: false,
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
        host.draft_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&site_id);
        let files = source.files;
        let response = match work {
            DraftWork::Request(request) => self
                .inner
                .backends
                .handle(&target, move || Ok(files), request)
                .await
                .map_err(|e| e.to_string()),
            DraftWork::Task(task) => {
                if !host.jobs_enabled {
                    return Err("background jobs are turned off on this server".into());
                }
                let (kind, name, payload) = match task {
                    DraftTask::Job { name, payload } => (TaskKind::Job, name, payload),
                    DraftTask::Schedule { name } => (TaskKind::Schedule, name, Value::Null),
                };
                let info = serde_json::json!({
                    "name": name,
                    "id": null,
                    "attempt": 1,
                    "scheduledAt": sw_core::time::now_ms(),
                    "test": true,
                });
                let message = TaskMsg {
                    kind,
                    name,
                    payload,
                    info,
                    timeout_ms: 0,
                };
                self.inner
                    .backends
                    .run_task(&target, move || Ok(files), message)
                    .await
                    .map(|error| BackendResponse {
                        status: if error.is_some() { 500 } else { 200 },
                        headers: Vec::new(),
                        body: error.unwrap_or_default(),
                    })
                    .map_err(|e| e.to_string())
            }
        };
        let tasks = host.declared_for(&target).unwrap_or_default();
        let tasks_problem = crate::jobs::validate(&tasks).err();
        let jobs_enqueued = host
            .draft_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&site_id)
            .unwrap_or_default();
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
                tasks,
                tasks_problem,
                jobs_enqueued,
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

    /// The gateway through which backends reach AI models: settings, usage, models.
    pub fn llm(&self) -> &crate::llm::LlmGateway {
        &self.inner.host.llm
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

    /// Run a read-only query on the site's live database, for its dashboard. The
    /// database is the one that belongs to `site` and no other; it is not created when
    /// the site has none.
    pub async fn query_read_only(
        &self,
        site: &SiteName,
        sql: &str,
        params: &[Value],
        max_rows: usize,
    ) -> Result<ReadRows, String> {
        let path = self.inner.host.db_path(site.as_str(), false)?;
        if !path.is_file() {
            return Err(
                "this site has no database yet (it appears when a backend is published)".into(),
            );
        }
        self.inner
            .host
            .dbs
            .get(&path)
            .await?
            .query_read_only_with_columns(sql, params, max_rows)
            .await
    }

    /// Replace the site's secret values in `text`, as for everything the agent is shown.
    pub fn redact(&self, site_id: i64, text: &str) -> String {
        self.inner.host.secrets.redact(site_id, text)
    }

    /// Write a consistent copy of the site's live database into `dir`, as
    /// `<site>-data.db`, replacing an earlier copy. `None` when the site has no
    /// database.
    pub async fn snapshot(&self, site: &SiteName, dir: &Path) -> Result<Option<PathBuf>, String> {
        let live = self.inner.host.db_path(site.as_str(), false)?;
        if !live.is_file() {
            return Ok(None);
        }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let out = dir.join(format!("{site}-data.db"));
        remove_db_files(&out);
        self.inner.host.dbs.get(&live).await?.copy_to(&out).await?;
        Ok(Some(out))
    }

    /// Where a site's live database is kept under this data directory.
    pub fn database_path(&self, site: &SiteName) -> Result<PathBuf, String> {
        self.inner.host.db_path(site.as_str(), false)
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
        self.inner.live_tasks.lock().await.remove(&site_id);
    }

    /// Stop workers that have been idle too long. Call periodically.
    pub fn reap_idle(&self) -> usize {
        self.inner.backends.reap_idle()
    }

    /// How many workers are alive.
    pub fn workers(&self) -> usize {
        self.inner.backends.alive()
    }

    /// Whether background jobs and schedules run on this server.
    pub fn jobs_enabled(&self) -> bool {
        self.inner.host.jobs_enabled
    }

    /// At start-up, before the first [`Self::run_due`]: settle work that was running
    /// when the daemon last stopped.
    pub async fn recover_jobs(&self) -> Result<u64, String> {
        self.inner.host.jobs.recover().await
    }

    /// Forget finished jobs and runs older than a week. Call periodically.
    pub async fn prune_jobs(&self) -> Result<u64, String> {
        self.inner.host.jobs.prune(sw_core::time::now_ms()).await
    }

    /// The newest jobs and scheduled runs of a site, newest first. Errors have the
    /// site's secret values replaced by their names.
    pub async fn jobs(&self, site_id: i64, limit: i64) -> Result<Vec<JobRecord>, String> {
        let mut records = self.inner.host.jobs.recent(site_id, limit).await?;
        for record in &mut records {
            if let Some(error) = &record.error {
                record.error = Some(self.redact(site_id, error));
            }
        }
        Ok(records)
    }

    /// How many jobs of a site are waiting or running.
    pub async fn pending_jobs(&self, site_id: i64) -> Result<i64, String> {
        self.inner.host.jobs.pending(site_id).await
    }

    /// The scheduled tasks of a site's live release and when each runs next. Empty
    /// until the scheduler has seen the release (at publish, or its first pass).
    pub async fn schedules(&self, site_id: i64) -> Vec<ScheduleInfo> {
        self.inner
            .live_tasks
            .lock()
            .await
            .get(&site_id)
            .map(|tasks| {
                tasks
                    .schedules
                    .iter()
                    .map(|(s, _, next)| ScheduleInfo {
                        name: s.name.clone(),
                        cron: s.cron.clone(),
                        next_at: Some(*next),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Make every scheduled task of a site due now. For tests.
    #[doc(hidden)]
    pub async fn make_schedules_due(&self, site_id: i64) {
        if let Some(tasks) = self.inner.live_tasks.lock().await.get_mut(&site_id) {
            for (_, _, next) in &mut tasks.schedules {
                *next = 0;
            }
        }
    }

    /// Whether background work of a site is in progress.
    pub fn jobs_running(&self, site_id: i64) -> bool {
        self.inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&site_id)
    }

    /// One pass of the scheduler: for each live backend, run the scheduled tasks that
    /// are due and the queued jobs whose time has come. Each site's work runs in the
    /// background, in its own worker, one task after another; a site whose previous
    /// work is still running is skipped until it finishes. Call every few seconds.
    pub async fn run_due(&self, live: Vec<LiveBackend>) {
        if !self.inner.host.jobs_enabled {
            return;
        }
        let now = sw_core::time::now_ms();
        let with_jobs: HashSet<i64> = match self.inner.host.jobs.sites_with_due_jobs(now).await {
            Ok(sites) => sites.into_iter().collect(),
            Err(error) => {
                tracing::error!(%error, "could not read the job queue");
                HashSet::new()
            }
        };
        let live_ids: HashSet<i64> = live.iter().map(|b| b.site_id).collect();
        self.inner
            .live_tasks
            .lock()
            .await
            .retain(|id, _| live_ids.contains(id));
        for backend in live {
            let busy = self
                .inner
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&backend.site_id);
            if busy {
                continue;
            }
            let (due, jobs) = self.due_schedules(&backend, now).await;
            let has_jobs = with_jobs.contains(&backend.site_id);
            if due.is_empty() && !has_jobs {
                continue;
            }
            self.inner
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(backend.site_id);
            let running = Running {
                set: self.inner.running.clone(),
                site_id: backend.site_id,
            };
            let this = self.clone();
            tokio::spawn(async move {
                let _running = running;
                this.work(&backend, due, has_jobs.then_some(jobs)).await;
            });
        }
    }

    /// The schedules of a live backend that are due now (advancing each to its next
    /// time), and the job names it declares. Learns what a release declares the first
    /// time it sees it, by loading its code once.
    async fn due_schedules(
        &self,
        backend: &LiveBackend,
        now: i64,
    ) -> (Vec<(String, i64)>, Vec<String>) {
        let known = self
            .inner
            .live_tasks
            .lock()
            .await
            .get(&backend.site_id)
            .is_some_and(|t| t.sha == backend.sha);
        if !known {
            let declared = self.declared_by(backend).await;
            self.inner.live_tasks.lock().await.insert(
                backend.site_id,
                LiveTasks::new(backend.sha.clone(), declared, now),
            );
        }
        let mut all = self.inner.live_tasks.lock().await;
        let Some(tasks) = all.get_mut(&backend.site_id) else {
            return (Vec::new(), Vec::new());
        };
        let mut due = Vec::new();
        for (schedule, cron, next) in &mut tasks.schedules {
            if *next <= now {
                due.push((schedule.name.clone(), *next));
                // Missed runs (the server was down, or the previous run was long) are
                // not made up: the next run is the next one after now.
                *next = cron.next_after(now).unwrap_or(i64::MAX);
            }
        }
        (due, tasks.declared.jobs.clone())
    }

    /// Load a live release's code in a throwaway worker to see what it declares.
    async fn declared_by(&self, backend: &LiveBackend) -> Declared {
        let source = match BackendSource::read(&backend.root) {
            Ok(Some(source)) => source,
            Ok(None) => return Declared::default(),
            Err(error) => {
                tracing::warn!(site = %backend.site, %error, "could not read the live backend");
                return Declared::default();
            }
        };
        let target = Target {
            site_id: backend.site_id,
            site: backend.site.as_str().to_owned(),
            version: backend.sha.clone(),
            draft: false,
            background: true,
        };
        match self.inner.backends.check(&target, source.files).await {
            Ok(declared) => match crate::jobs::validate(&declared) {
                Ok(()) => declared,
                Err(error) => {
                    tracing::warn!(site = %backend.site, %error, "the live backend's schedules are not valid; none will run");
                    Declared {
                        jobs: declared.jobs,
                        schedules: Vec::new(),
                    }
                }
            },
            Err(error) => {
                tracing::warn!(site = %backend.site, %error, "the live backend does not load; its jobs cannot run");
                Declared::default()
            }
        }
    }

    /// Run one site's due work, one task at a time, in its background worker.
    async fn work(
        &self,
        backend: &LiveBackend,
        due: Vec<(String, i64)>,
        jobs: Option<Vec<String>>,
    ) {
        let host = &self.inner.host;
        let target = Target {
            site_id: backend.site_id,
            site: backend.site.as_str().to_owned(),
            version: backend.sha.clone(),
            draft: false,
            background: true,
        };
        let redact = |error: String| self.redact(backend.site_id, &error);
        for (name, at) in due {
            let id = match host.jobs.start_run(backend.site_id, &name, at).await {
                Ok(id) => id,
                Err(error) => {
                    tracing::error!(site = %backend.site, %error, "could not record a scheduled run");
                    continue;
                }
            };
            let info =
                serde_json::json!({ "name": name, "id": id, "attempt": 1, "scheduledAt": at });
            let error = self
                .task(
                    &target,
                    &backend.root,
                    TaskKind::Schedule,
                    &name,
                    Value::Null,
                    info,
                )
                .await
                .map(redact);
            if let Some(error) = &error {
                tracing::info!(site = %backend.site, schedule = %name, %error, "scheduled task failed");
            }
            if let Err(e) = host.jobs.finish_run(id, error.as_deref()).await {
                tracing::error!(site = %backend.site, error = %e, "could not record a scheduled run");
            }
        }
        let Some(declared) = jobs else { return };
        let claimed = match host
            .jobs
            .claim_due(backend.site_id, sw_core::time::now_ms(), crate::jobs::BATCH)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::error!(site = %backend.site, %error, "could not take jobs from the queue");
                return;
            }
        };
        for job in claimed {
            let (error, retry) = if declared.contains(&job.name) {
                let info = serde_json::json!({
                    "name": job.name,
                    "id": job.id,
                    "attempt": job.attempts,
                    "attempts": job.max_attempts,
                });
                let error = self
                    .task(
                        &target,
                        &backend.root,
                        TaskKind::Job,
                        &job.name,
                        job.payload.clone(),
                        info,
                    )
                    .await
                    .map(redact);
                (error, true)
            } else {
                (
                    Some(format!(
                        "the published backend has no job named {}; nothing ran",
                        job.name
                    )),
                    false,
                )
            };
            if let Err(e) = host.jobs.finish(&job, error.as_deref(), retry).await {
                tracing::error!(site = %backend.site, error = %e, "could not record a job's outcome");
            }
        }
    }

    /// Run one task; `Some(error)` when it failed for any reason.
    async fn task(
        &self,
        target: &Target,
        root: &Path,
        kind: TaskKind,
        name: &str,
        payload: Value,
        info: Value,
    ) -> Option<String> {
        let root = root.to_owned();
        let message = TaskMsg {
            kind,
            name: name.to_owned(),
            payload,
            info,
            timeout_ms: 0,
        };
        let outcome = self
            .inner
            .backends
            .run_task(
                target,
                move || match BackendSource::read(&root)? {
                    Some(source) => Ok(source.files),
                    None => Err("the release has no backend/index.js".into()),
                },
                message,
            )
            .await;
        match outcome {
            Ok(error) => error,
            Err(error) => Some(error.to_string()),
        }
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
                background: false,
            };
            // 1. The code must load, and what it declares must be valid.
            let declared = self
                .inner
                .backends
                .check(&target, source.files)
                .await
                .map_err(|e| e.to_string())?;
            crate::jobs::validate(&declared)?;
            self.inner
                .publishing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(plan.site_id, (plan.sha.clone(), declared));

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
        Box::pin(async move {
            self.inner
                .publishing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&plan.site_id);
            self.restore(plan.site_id, &plan.site).await;
        })
    }

    fn commit<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // The copy stays on disk as the last pre-publish backup; only forget it.
            self.inner
                .snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&plan.site_id);
            let published = self
                .inner
                .publishing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&plan.site_id);
            if let Some((sha, declared)) = published {
                let tasks = LiveTasks::new(sha, declared, sw_core::time::now_ms());
                self.inner
                    .live_tasks
                    .lock()
                    .await
                    .insert(plan.site_id, tasks);
            }
        })
    }
}
