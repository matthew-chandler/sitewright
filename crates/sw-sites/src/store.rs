//! The site store: creating, editing, publishing, rolling back and deleting sites.
//!
//! Layout on disk, per site: `sites/<name>/repo/` is a git working tree holding the
//! source; `sites/<name>/releases/<sha>/` are immutable copies of published commits.
//! Serving only ever reads a release directory. Publishing commits the working tree,
//! exports the commit to a new release directory and then switches the site's
//! `current_release` pointer in one transaction, so a release goes live all at once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;
use futures_util::future::BoxFuture;
use serde::Serialize;
use sqlx::SqlitePool;
use sw_core::config::{DomainsConfig, SitesConfig};
use sw_core::names::normalize_hostname;
use sw_core::time::{MS_PER_DAY, now_ms};
use sw_core::{Config, DataDir, SiteName};

use crate::confine;
use crate::error::{Result, SiteError};
use crate::git;
use crate::import::ImportFile;
use crate::routes::{Route, RouteTable};

/// Who made a change. Recorded on releases and in the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    Agent,
    User,
}

impl Author {
    pub fn as_str(self) -> &'static str {
        match self {
            Author::Agent => "agent",
            Author::User => "user",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SiteStatus {
    Active,
    Deleted,
}

/// A site row.
#[derive(Debug, Clone, Serialize)]
pub struct Site {
    pub id: i64,
    pub name: SiteName,
    pub title: String,
    pub status: SiteStatus,
    /// The release currently being served, if the site has been published.
    pub current_release: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

/// A published, immutable version of a site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    pub id: i64,
    pub site_id: i64,
    pub sha: String,
    pub message: String,
    pub author: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hostname {
    pub hostname: String,
    pub is_primary: bool,
    /// The hostname redirects to the primary hostname instead of serving the site.
    pub redirect: bool,
}

/// Everything about one site, for `site_info` and the UI.
#[derive(Debug, Clone, Serialize)]
pub struct SiteInfo {
    pub site: Site,
    /// Public URL of the primary hostname.
    pub url: String,
    pub hostnames: Vec<Hostname>,
    /// Newest first.
    pub releases: Vec<Release>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
}

/// A file the owner uploaded, from `uploads/` in the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Upload {
    /// Site-relative path, such as `uploads/logo.png`.
    pub path: String,
    pub size: u64,
    /// Pixel dimensions, for images whose header could be read.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Upload {
    /// The file name without the `uploads/` prefix.
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// The result of adding, replacing or removing an upload.
#[derive(Debug, Clone, Serialize)]
pub struct UploadOutcome {
    /// The file as uploaded, or as it was before it was removed.
    pub upload: Upload,
    /// An upload replaced an existing file with the same name.
    pub replaced: bool,
    /// The release that made the change live, when it was published at once.
    pub release: Option<Release>,
    /// Why the change was not published at once; it then goes live with the next publish.
    pub pending: Option<String>,
}

/// The result of a publish.
#[derive(Debug, Clone, Serialize)]
pub struct PublishOutcome {
    pub release: Release,
    /// False when the working tree was already live and nothing happened.
    pub changed: bool,
    pub url: String,
}

/// The result of an import.
#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub files: usize,
    pub bytes: u64,
    /// Files that were left out, with the reason.
    pub skipped: Vec<(String, String)>,
    pub publish: PublishOutcome,
}

/// An extra check run right after a release goes live. Receives the route that was just
/// switched to and the site's primary hostname. An `Err` rolls the release back.
pub type HealthCheck =
    Arc<dyn Fn(Arc<Route>, String) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// A release about to go live, as the release hooks see it.
#[derive(Debug, Clone)]
pub struct ReleasePlan {
    pub site_id: i64,
    pub site: SiteName,
    pub sha: String,
    /// The release directory.
    pub root: PathBuf,
}

/// Work that belongs to switching a site to a new release but lives outside the store,
/// such as migrating a backend's database. Installed once by the daemon.
pub trait ReleaseHooks: Send + Sync {
    /// Before the release goes live. An `Err` stops the publish; the previous release
    /// stays live and the hook must leave things as it found them.
    fn prepare<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, Result<(), String>>;
    /// The release went live but failed its health check and was rolled back: undo what
    /// `prepare` did.
    fn abandon<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, ()>;
    /// The release is live and healthy.
    fn commit<'a>(&'a self, plan: &'a ReleasePlan) -> BoxFuture<'a, ()>;
}

/// Handle to the site store. Cheap to clone.
#[derive(Clone)]
pub struct SiteStore {
    inner: Arc<Inner>,
}

struct Inner {
    db: SqlitePool,
    data: DataDir,
    domains: DomainsConfig,
    limits: SitesConfig,
    routes: ArcSwap<RouteTable>,
    /// One async mutex per site name, so changes to a site never interleave.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    health_check: OnceLock<HealthCheck>,
    release_hooks: OnceLock<Arc<dyn ReleaseHooks>>,
}

impl std::fmt::Debug for SiteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteStore")
            .field("data", &self.inner.data)
            .finish_non_exhaustive()
    }
}

struct SiteRow {
    id: i64,
    name: String,
    title: String,
    status: String,
    current_release: Option<i64>,
    created_at: i64,
    updated_at: i64,
    deleted_at: Option<i64>,
}

impl TryFrom<SiteRow> for Site {
    type Error = SiteError;
    fn try_from(row: SiteRow) -> Result<Self> {
        Ok(Site {
            id: row.id,
            name: SiteName::parse(&row.name)?,
            title: row.title,
            status: if row.status == "deleted" {
                SiteStatus::Deleted
            } else {
                SiteStatus::Active
            },
            current_release: row.current_release,
            created_at: row.created_at,
            updated_at: row.updated_at,
            deleted_at: row.deleted_at,
        })
    }
}

/// Trim and check a site title.
fn valid_title(title: &str) -> Result<&str> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 120 {
        return Err(SiteError::Invalid(
            "the title must be between 1 and 120 characters".into(),
        ));
    }
    Ok(title)
}

/// Run blocking filesystem or git work off the async executor.
async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SiteError::Internal(format!("background task failed: {e}")))?
}

/// Write one file into a working tree after checking the per-file and per-site size
/// limits. Blocking.
fn write_checked(
    repo: &Path,
    rel: &Path,
    path: &str,
    data: &[u8],
    limits: &SitesConfig,
) -> Result<FileEntry> {
    let size = data.len() as u64;
    if size > limits.max_file_bytes {
        return Err(SiteError::Invalid(format!(
            "{path} is {size} bytes; the limit is {} bytes per file",
            limits.max_file_bytes
        )));
    }
    let (files, _) = confine::walk(repo)?;
    let rel_str = rel.to_string_lossy();
    let others: u64 = files
        .iter()
        .filter(|f| f.rel != rel_str)
        .map(|f| f.size)
        .sum();
    if others + size > limits.max_site_bytes {
        return Err(SiteError::Invalid(format!(
            "writing {path} would make the site {} bytes; the limit is {} bytes",
            others + size,
            limits.max_site_bytes
        )));
    }
    let full = confine::resolve_in(repo, rel)?;
    if full.is_dir() {
        return Err(SiteError::path(path, "a directory with this name exists"));
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&full, data)?;
    Ok(FileEntry {
        path: rel_str.into_owned(),
        size,
    })
}

/// Describe an uploaded file, reading an image's dimensions from its header. Blocking.
fn upload_entry(repo: &Path, path: String, size: u64) -> Upload {
    let dims = imagesize::size(repo.join(&path)).ok();
    Upload {
        path,
        size,
        width: dims.and_then(|d| u32::try_from(d.width).ok()),
        height: dims.and_then(|d| u32::try_from(d.height).ok()),
    }
}

/// Remove directories that `removed` (a file just deleted) left empty, up to `root`.
fn remove_empty_dirs(root: &Path, removed: &Path) {
    let mut dir = removed.parent();
    while let Some(d) = dir {
        if d == root || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

impl SiteStore {
    /// Open the store and build the routing table from the database.
    pub async fn open(db: SqlitePool, data: DataDir, config: &Config) -> Result<Self> {
        let store = Self {
            inner: Arc::new(Inner {
                db,
                data,
                domains: config.domains.clone(),
                limits: config.sites.clone(),
                routes: ArcSwap::from_pointee(RouteTable::default()),
                locks: Mutex::new(HashMap::new()),
                health_check: OnceLock::new(),
                release_hooks: OnceLock::new(),
            }),
        };
        store.reload_routes().await?;
        Ok(store)
    }

    /// Install the post-publish health check. Can be set once; later calls are ignored.
    pub fn set_health_check(&self, check: HealthCheck) {
        let _ = self.inner.health_check.set(check);
    }

    /// Install the release hooks. Can be set once; later calls are ignored.
    pub fn set_release_hooks(&self, hooks: Arc<dyn ReleaseHooks>) {
        let _ = self.inner.release_hooks.set(hooks);
    }

    /// The current routing table snapshot.
    pub fn routes(&self) -> Arc<RouteTable> {
        self.inner.routes.load_full()
    }

    pub fn domains(&self) -> &DomainsConfig {
        &self.inner.domains
    }

    pub fn limits(&self) -> &SitesConfig {
        &self.inner.limits
    }

    pub fn db(&self) -> &SqlitePool {
        &self.inner.db
    }

    // ------------------------------------------------------------------ reading

    /// All active sites, by name. With `include_deleted`, soft-deleted sites too.
    pub async fn list(&self, include_deleted: bool) -> Result<Vec<Site>> {
        let rows = sqlx::query_as!(
            SiteRow,
            r#"SELECT id as "id!", name, title, status, current_release, created_at, updated_at, deleted_at
               FROM sites WHERE status = 'active' OR ?1 ORDER BY name"#,
            include_deleted
        )
        .fetch_all(&self.inner.db)
        .await?;
        rows.into_iter().map(Site::try_from).collect()
    }

    /// Look up a site by name, whatever its status.
    pub async fn find(&self, name: &str) -> Result<Option<Site>> {
        let row = sqlx::query_as!(
            SiteRow,
            r#"SELECT id as "id!", name, title, status, current_release, created_at, updated_at, deleted_at
               FROM sites WHERE name = ?1"#,
            name
        )
        .fetch_optional(&self.inner.db)
        .await?;
        row.map(Site::try_from).transpose()
    }

    /// Look up an active site by name.
    pub async fn get(&self, name: &str) -> Result<Site> {
        match self.find(name).await? {
            Some(site) if site.status == SiteStatus::Active => Ok(site),
            Some(_) => Err(SiteError::Conflict(format!(
                "site {name:?} was deleted; restore it before using it"
            ))),
            None => Err(SiteError::NotFound(name.to_owned())),
        }
    }

    /// A site with its hostnames and release history.
    pub async fn info(&self, name: &str) -> Result<SiteInfo> {
        let site = self.get(name).await?;
        let hostnames = self.hostnames(site.id).await?;
        let releases = self.releases(site.id, 50).await?;
        let primary = hostnames
            .iter()
            .find(|h| h.is_primary)
            .or(hostnames.first())
            .map(|h| h.hostname.clone())
            .unwrap_or_else(|| self.inner.domains.site_hostname(&site.name));
        Ok(SiteInfo {
            url: self.inner.domains.public_url(&primary),
            site,
            hostnames,
            releases,
        })
    }

    async fn hostnames(&self, site_id: i64) -> Result<Vec<Hostname>> {
        let rows = sqlx::query!(
            "SELECT hostname, is_primary, redirect FROM site_hostnames WHERE site_id = ?1 ORDER BY is_primary DESC, hostname",
            site_id
        )
        .fetch_all(&self.inner.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Hostname {
                hostname: r.hostname,
                is_primary: r.is_primary != 0,
                redirect: r.redirect != 0,
            })
            .collect())
    }

    /// Every hostname of every active site, published or not: the names this server
    /// answers for, and the only ones it may request certificates for.
    pub async fn all_hostnames(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            "SELECT h.hostname FROM site_hostnames h JOIN sites s ON s.id = h.site_id WHERE s.status = 'active' ORDER BY h.hostname"
        )
        .fetch_all(&self.inner.db)
        .await?)
    }

    /// The site's releases, newest first.
    pub async fn releases(&self, site_id: i64, limit: i64) -> Result<Vec<Release>> {
        Ok(sqlx::query_as!(
            Release,
            r#"SELECT id as "id!", site_id, sha, message, author, created_at
               FROM releases WHERE site_id = ?1 ORDER BY id DESC LIMIT ?2"#,
            site_id,
            limit
        )
        .fetch_all(&self.inner.db)
        .await?)
    }

    async fn release(&self, id: i64) -> Result<Release> {
        Ok(sqlx::query_as!(
            Release,
            r#"SELECT id as "id!", site_id, sha, message, author, created_at FROM releases WHERE id = ?1"#,
            id
        )
        .fetch_one(&self.inner.db)
        .await?)
    }

    /// The files a release changed compared with the one before it, as `(status, path)`
    /// pairs where status is `A`, `M` or `D`.
    pub async fn release_changes(&self, name: &SiteName, sha: &str) -> Result<Vec<(char, String)>> {
        let repo = self.inner.data.site_repo(name);
        let sha = sha.to_owned();
        let changed = blocking(move || git::changed_files(&repo, &sha)).await?;
        Ok(changed.into_iter().map(|c| (c.status, c.path)).collect())
    }

    // ------------------------------------------------------------------ creating

    /// Create a site: validate the name, reserve its hostname and initialise its repository.
    pub async fn create(&self, name: &str, title: &str, author: Author) -> Result<Site> {
        let (site_name, hostname) = self.usable_name(name)?;
        let title = valid_title(title)?;

        let _guard = self.lock(&site_name).await;
        if let Some(existing) = self.find(name).await? {
            return Err(SiteError::Conflict(match existing.status {
                SiteStatus::Active => format!("a site named {name:?} already exists"),
                SiteStatus::Deleted => format!(
                    "a site named {name:?} was deleted recently; restore it, or wait until it is purged"
                ),
            }));
        }

        let now = now_ms();
        let mut tx = sw_core::db::begin_write(&self.inner.db).await?;
        let taken = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!: i64" FROM site_hostnames WHERE hostname = ?1"#,
            hostname
        )
        .fetch_one(&mut *tx)
        .await?;
        if taken > 0 {
            return Err(SiteError::Conflict(format!(
                "the hostname {hostname} is already used by another site"
            )));
        }
        let site_id = sqlx::query!(
            "INSERT INTO sites (name, title, status, created_at, updated_at) VALUES (?1, ?2, 'active', ?3, ?3)",
            name,
            title,
            now
        )
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query!(
            "INSERT INTO site_hostnames (site_id, hostname, is_primary, created_at) VALUES (?1, ?2, 1, ?3)",
            site_id,
            hostname,
            now
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let site_dir = self.inner.data.site_dir(&site_name);
        let repo = self.inner.data.site_repo(&site_name);
        let releases = self.inner.data.site_releases(&site_name);
        let init = blocking(move || {
            // A directory without a database row is a leftover; start clean.
            if site_dir.exists() {
                std::fs::remove_dir_all(&site_dir)?;
            }
            std::fs::create_dir_all(&releases)?;
            git::init(&repo)
        })
        .await;
        if let Err(e) = init {
            sqlx::query!("DELETE FROM sites WHERE id = ?1", site_id)
                .execute(&self.inner.db)
                .await?;
            return Err(e);
        }

        self.audit(author, "site.create", name, Some(&hostname))
            .await;
        self.get(name).await
    }

    /// Check a name for a new or renamed site and work out its default hostname.
    fn usable_name(&self, name: &str) -> Result<(SiteName, String)> {
        let site_name = SiteName::parse(name)?;
        let hostname = self.inner.domains.site_hostname(&site_name);
        if self.inner.domains.is_reserved(&site_name) || hostname == self.inner.domains.admin_host()
        {
            return Err(SiteError::Invalid(format!("the name {name:?} is reserved")));
        }
        Ok((site_name, hostname))
    }

    /// Whether the site is marked as containerized (see `docs/containers.md`).
    pub async fn containerized(&self, name: &str) -> Result<bool> {
        let site = self.get(name).await?;
        Ok(sqlx::query_scalar!(
            r#"SELECT containerized as "c!: bool" FROM sites WHERE id = ?1"#,
            site.id
        )
        .fetch_one(&self.inner.db)
        .await?)
    }

    /// Mark or unmark a site as containerized.
    pub async fn set_containerized(&self, name: &str, on: bool, author: Author) -> Result<()> {
        let site = self.get(name).await?;
        let now = now_ms();
        sqlx::query!(
            "UPDATE sites SET containerized = ?1, updated_at = ?2 WHERE id = ?3",
            on,
            now,
            site.id
        )
        .execute(&self.inner.db)
        .await?;
        self.audit(
            author,
            if on {
                "site.containerize"
            } else {
                "site.uncontainerize"
            },
            name,
            None,
        )
        .await;
        Ok(())
    }

    /// Change a site's title.
    pub async fn set_title(&self, name: &str, title: &str, author: Author) -> Result<Site> {
        let site = self.get(name).await?;
        let title = valid_title(title)?;
        let now = now_ms();
        sqlx::query!(
            "UPDATE sites SET title = ?1, updated_at = ?2 WHERE id = ?3",
            title,
            now,
            site.id
        )
        .execute(&self.inner.db)
        .await?;
        self.audit(author, "site.title", name, Some(title)).await;
        self.get(name).await
    }

    /// Rename a site. Its directory moves, its run history follows, and
    /// `<new>.<zone>` replaces `<old>.<zone>`, keeping that hostname's role (primary or
    /// extra). With `redirect_old`, the old default hostname stays attached and
    /// redirects to the primary hostname; otherwise it is detached. Other hostnames are
    /// untouched. Callers must make sure no agent run is in progress on the site.
    pub async fn rename(
        &self,
        name: &str,
        new_name: &str,
        redirect_old: bool,
        author: Author,
    ) -> Result<Site> {
        if name == new_name {
            return Err(SiteError::Invalid(
                "the new name is the same as the old one".into(),
            ));
        }
        let (new_site_name, new_host) = self.usable_name(new_name)?;
        let site = self.get(name).await?;
        let old_host = self.inner.domains.site_hostname(&site.name);

        // Lock both names, in a fixed order so two renames cannot deadlock.
        let (first, second) = if site.name.as_str() < new_site_name.as_str() {
            (&site.name, &new_site_name)
        } else {
            (&new_site_name, &site.name)
        };
        let _guard_a = self.lock(first).await;
        let _guard_b = self.lock(second).await;

        let site = self.get(name).await?;
        if let Some(existing) = self.find(new_name).await? {
            return Err(SiteError::Conflict(match existing.status {
                SiteStatus::Active => format!("a site named {new_name:?} already exists"),
                SiteStatus::Deleted => format!(
                    "a site named {new_name:?} was deleted recently; restore it, or wait until it is purged"
                ),
            }));
        }

        let now = now_ms();
        let mut tx = sw_core::db::begin_write(&self.inner.db).await?;
        let taken = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!: i64" FROM site_hostnames WHERE hostname = ?1 AND site_id != ?2"#,
            new_host,
            site.id
        )
        .fetch_one(&mut *tx)
        .await?;
        if taken > 0 {
            return Err(SiteError::Conflict(format!(
                "the hostname {new_host} is already used by another site"
            )));
        }
        let old_role = sqlx::query_scalar!(
            "SELECT is_primary FROM site_hostnames WHERE site_id = ?1 AND hostname = ?2",
            site.id,
            old_host
        )
        .fetch_optional(&mut *tx)
        .await?;
        let has_primary = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!: i64" FROM site_hostnames WHERE site_id = ?1 AND is_primary = 1 AND hostname != ?2"#,
            site.id,
            old_host
        )
        .fetch_one(&mut *tx)
        .await?
            > 0;
        // The new hostname takes over the old one's role. A site whose primary is a
        // custom hostname keeps it.
        let new_is_primary = old_role.is_some_and(|p| p != 0) || !has_primary;
        if new_is_primary {
            sqlx::query!(
                "UPDATE site_hostnames SET is_primary = 0 WHERE site_id = ?1",
                site.id
            )
            .execute(&mut *tx)
            .await?;
        }
        if redirect_old {
            sqlx::query!(
                "UPDATE site_hostnames SET redirect = 1 WHERE site_id = ?1 AND hostname = ?2",
                site.id,
                old_host
            )
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query!(
                "DELETE FROM site_hostnames WHERE site_id = ?1 AND hostname = ?2",
                site.id,
                old_host
            )
            .execute(&mut *tx)
            .await?;
        }
        // The new hostname may already be attached to this site as an extra.
        sqlx::query!(
            "INSERT INTO site_hostnames (site_id, hostname, is_primary, redirect, created_at) VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT (hostname) DO UPDATE SET is_primary = excluded.is_primary, redirect = 0",
            site.id,
            new_host,
            new_is_primary,
            now
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE sites SET name = ?1, updated_at = ?2 WHERE id = ?3",
            new_name,
            now,
            site.id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE agent_runs SET site_name = ?1 WHERE site_name = ?2",
            new_name,
            name
        )
        .execute(&mut *tx)
        .await?;

        // Move the directory while the transaction is open, so either both happen or
        // neither does.
        let from = self.inner.data.site_dir(&site.name);
        let to = self.inner.data.site_dir(&new_site_name);
        let (move_from, move_to) = (from.clone(), to.clone());
        blocking(move || {
            // A directory without a database row is a leftover.
            if move_to.exists() {
                std::fs::remove_dir_all(&move_to)?;
            }
            Ok(std::fs::rename(&move_from, &move_to)?)
        })
        .await?;
        if let Err(e) = tx.commit().await {
            if let Err(undo) = std::fs::rename(&to, &from) {
                tracing::error!(error = %undo, from = %to.display(), to = %from.display(),
                    "could not move a site directory back after a failed rename");
            }
            return Err(e.into());
        }

        self.reload_routes().await?;
        self.audit(author, "site.rename", name, Some(new_name))
            .await;
        self.get(new_name).await
    }

    /// Attach an extra hostname to a site.
    pub async fn add_hostname(&self, name: &str, hostname: &str, author: Author) -> Result<()> {
        self.add_hostname_as(name, hostname, false, author).await
    }

    /// Attach a hostname to a site. With `primary`, it becomes the site's primary
    /// hostname (the one links, uptime checks and the admin UI use) and the previous
    /// primary stays attached as an extra. Making an already attached hostname primary
    /// is allowed.
    pub async fn add_hostname_as(
        &self,
        name: &str,
        hostname: &str,
        primary: bool,
        author: Author,
    ) -> Result<()> {
        let site = self.get(name).await?;
        if primary {
            let host = normalize_hostname(hostname).ok_or_else(|| {
                SiteError::Invalid(format!("{hostname:?} is not a valid hostname"))
            })?;
            let attached = sqlx::query_scalar!(
                r#"SELECT count(*) as "n!: i64" FROM site_hostnames WHERE site_id = ?1 AND hostname = ?2"#,
                site.id,
                host
            )
            .fetch_one(&self.inner.db)
            .await?;
            if attached == 0 {
                Box::pin(self.add_hostname_as(name, &host, false, author)).await?;
            }
            let mut tx = sw_core::db::begin_write(&self.inner.db).await?;
            sqlx::query!(
                "UPDATE site_hostnames SET is_primary = 0 WHERE site_id = ?1",
                site.id
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                "UPDATE site_hostnames SET is_primary = 1, redirect = 0 WHERE site_id = ?1 AND hostname = ?2",
                site.id,
                host
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            self.reload_routes().await?;
            self.audit(author, "site.hostname.primary", name, Some(&host))
                .await;
            return Ok(());
        }
        let host = normalize_hostname(hostname)
            .ok_or_else(|| SiteError::Invalid(format!("{hostname:?} is not a valid hostname")))?;
        if host == self.inner.domains.admin_host() {
            return Err(SiteError::Invalid(format!("{host} is the admin hostname")));
        }
        let now = now_ms();
        let result = sqlx::query!(
            "INSERT INTO site_hostnames (site_id, hostname, is_primary, created_at) VALUES (?1, ?2, 0, ?3)",
            site.id,
            host,
            now
        )
        .execute(&self.inner.db)
        .await;
        match result {
            Ok(_) => {}
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return Err(SiteError::Conflict(format!(
                    "the hostname {host} is already in use"
                )));
            }
            Err(e) => return Err(e.into()),
        }
        self.reload_routes().await?;
        self.audit(author, "site.hostname.add", name, Some(&host))
            .await;
        Ok(())
    }

    /// Detach an extra hostname. The primary hostname cannot be removed.
    pub async fn remove_hostname(&self, name: &str, hostname: &str, author: Author) -> Result<()> {
        let site = self.get(name).await?;
        let removed = sqlx::query!(
            "DELETE FROM site_hostnames WHERE site_id = ?1 AND hostname = ?2 AND is_primary = 0",
            site.id,
            hostname
        )
        .execute(&self.inner.db)
        .await?
        .rows_affected();
        if removed == 0 {
            return Err(SiteError::Invalid(format!(
                "{hostname} is not an extra hostname of {name}"
            )));
        }
        self.reload_routes().await?;
        self.audit(author, "site.hostname.remove", name, Some(hostname))
            .await;
        Ok(())
    }

    // ------------------------------------------------------------------ files

    /// The directory holding the site's working tree (its unpublished files).
    pub async fn working_tree(&self, name: &str) -> Result<PathBuf> {
        let site = self.get(name).await?;
        Ok(self.inner.data.site_repo(&site.name))
    }

    /// The files in the site's working tree.
    pub async fn list_files(&self, name: &str) -> Result<Vec<FileEntry>> {
        let site = self.get(name).await?;
        let repo = self.inner.data.site_repo(&site.name);
        blocking(move || {
            let (files, _) = confine::walk(&repo)?;
            Ok(files
                .into_iter()
                .map(|f| FileEntry {
                    path: f.rel,
                    size: f.size,
                })
                .collect())
        })
        .await
    }

    /// Read one file from the working tree.
    pub async fn read_file(&self, name: &str, path: &str) -> Result<Vec<u8>> {
        let site = self.get(name).await?;
        let rel = confine::clean_site_path(path, &self.inner.limits.allowed_extensions)?;
        let repo = self.inner.data.site_repo(&site.name);
        let path = path.to_owned();
        blocking(move || {
            let full = confine::resolve_in(&repo, &rel)?;
            match std::fs::read(&full) {
                Ok(data) => Ok(data),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(SiteError::Invalid(format!("file {path:?} does not exist")))
                }
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// Create or replace one file in the working tree. Nothing goes live until publish.
    pub async fn write_file(&self, name: &str, path: &str, data: Vec<u8>) -> Result<FileEntry> {
        let site = self.get(name).await?;
        let _guard = self.lock(&site.name).await;
        let rel = confine::clean_site_path(path, &self.inner.limits.allowed_extensions)?;
        let limits = self.inner.limits.clone();
        let repo = self.inner.data.site_repo(&site.name);
        let path = path.to_owned();
        blocking(move || write_checked(&repo, &rel, &path, &data, &limits)).await
    }

    /// Delete one file from the working tree.
    pub async fn delete_file(&self, name: &str, path: &str) -> Result<()> {
        let site = self.get(name).await?;
        let _guard = self.lock(&site.name).await;
        let rel = confine::clean_site_path(path, &self.inner.limits.allowed_extensions)?;
        let repo = self.inner.data.site_repo(&site.name);
        let path = path.to_owned();
        blocking(move || {
            let full = confine::resolve_in(&repo, &rel)?;
            match std::fs::remove_file(&full) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SiteError::Invalid(format!("file {path:?} does not exist")));
                }
                Err(e) => return Err(e.into()),
            }
            remove_empty_dirs(&repo, &full);
            Ok(())
        })
        .await
    }

    // ------------------------------------------------------------------ uploads

    /// The files the owner uploaded, sorted by path.
    pub async fn list_uploads(&self, name: &str) -> Result<Vec<Upload>> {
        let site = self.get(name).await?;
        let repo = self.inner.data.site_repo(&site.name);
        blocking(move || {
            let (files, _) = confine::walk(&repo)?;
            Ok(files
                .into_iter()
                .filter(|f| confine::is_upload_path(&f.rel))
                .map(|f| upload_entry(&repo, f.rel, f.size))
                .collect())
        })
        .await
    }

    /// Store a file the owner uploaded under `uploads/`, named after `file_name`, and
    /// publish it. It is published only when the working tree held exactly the live
    /// release before, so an upload never puts unfinished work live; otherwise it waits
    /// for the next publish. If the publish fails, the working tree is put back.
    pub async fn add_upload(
        &self,
        name: &str,
        file_name: &str,
        data: Vec<u8>,
        author: Author,
    ) -> Result<UploadOutcome> {
        let path = confine::upload_path(file_name, &self.inner.limits.allowed_extensions)?;
        let message = format!("Upload {path}");
        self.change_upload(name, path, Some(data), &message, author)
            .await
    }

    /// Remove an uploaded file, publishing the removal under the same rule as
    /// [`SiteStore::add_upload`]. A page that still uses the file makes the publish fail,
    /// and the file is then kept.
    pub async fn remove_upload(
        &self,
        name: &str,
        path: &str,
        author: Author,
    ) -> Result<UploadOutcome> {
        let path = if confine::is_upload_path(path) {
            path.to_owned()
        } else {
            format!("{}/{path}", confine::UPLOADS_DIR)
        };
        let message = format!("Remove {path}");
        self.change_upload(name, path, None, &message, author).await
    }

    async fn change_upload(
        &self,
        name: &str,
        path: String,
        data: Option<Vec<u8>>,
        message: &str,
        author: Author,
    ) -> Result<UploadOutcome> {
        let site = self.get(name).await?;
        let _guard = self.lock(&site.name).await;
        let site = self.get(name).await?;
        let rel = confine::clean_site_path(&path, &self.inner.limits.allowed_extensions)?;
        let repo = self.inner.data.site_repo(&site.name);
        let live_sha = match site.current_release {
            Some(id) => Some(self.release(id).await?.sha),
            None => None,
        };

        let removing = data.is_none();
        let (clean, previous, upload) = {
            let repo = repo.clone();
            let rel = rel.clone();
            let limits = self.inner.limits.clone();
            let path = path.clone();
            blocking(move || {
                let clean = match &live_sha {
                    Some(sha) => git::worktree_matches(&repo, sha)?,
                    None => false,
                };
                let full = confine::resolve_in(&repo, &rel)?;
                let previous = match std::fs::read(&full) {
                    Ok(bytes) => Some(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e.into()),
                };
                let upload = match data {
                    Some(data) => {
                        let entry = write_checked(&repo, &rel, &path, &data, &limits)?;
                        upload_entry(&repo, entry.path, entry.size)
                    }
                    None => {
                        let Some(old) = &previous else {
                            return Err(SiteError::Invalid(format!("{path} does not exist")));
                        };
                        let upload = upload_entry(&repo, path.clone(), old.len() as u64);
                        std::fs::remove_file(&full)?;
                        upload
                    }
                };
                if removing {
                    remove_empty_dirs(&repo, &full);
                }
                Ok((clean, previous, upload))
            })
            .await?
        };
        let replaced = !removing && previous.is_some();
        self.audit(author, "site.upload", site.name.as_str(), Some(message))
            .await;

        if !clean {
            let pending = if site.current_release.is_none() {
                "the site has not been published yet, so this goes live with its first publish"
            } else {
                "the site has changes that are not published yet, so this goes live with the next publish"
            };
            return Ok(UploadOutcome {
                upload,
                replaced,
                release: None,
                pending: Some(pending.to_owned()),
            });
        }

        match self.publish_locked(&site, message, author).await {
            Ok(outcome) => Ok(UploadOutcome {
                upload,
                replaced,
                release: Some(outcome.release),
                pending: None,
            }),
            Err(error) => {
                // Put the file back the way it was, so the working tree matches what is live.
                let restore = blocking(move || {
                    let full = confine::resolve_in(&repo, &rel)?;
                    match previous {
                        Some(bytes) => {
                            if let Some(parent) = full.parent() {
                                std::fs::create_dir_all(parent)?;
                            }
                            std::fs::write(&full, bytes)?;
                        }
                        None => {
                            std::fs::remove_file(&full)?;
                            remove_empty_dirs(&repo, &full);
                        }
                    }
                    Ok(())
                })
                .await;
                if let Err(restore_error) = restore {
                    tracing::error!(site = %site.name, %restore_error, "could not undo an upload change");
                }
                Err(error)
            }
        }
    }

    /// Replace the working tree with `files` and publish. Files with hidden paths or
    /// disallowed types are skipped and reported; a file over the size limit is an error.
    pub async fn import(
        &self,
        name: &str,
        files: Vec<ImportFile>,
        message: &str,
        author: Author,
    ) -> Result<ImportReport> {
        let site = self.get(name).await?;
        let guard = self.lock(&site.name).await;
        let limits = self.inner.limits.clone();
        let repo = self.inner.data.site_repo(&site.name);

        let (count, bytes, skipped) = blocking(move || {
            let mut accepted: Vec<(PathBuf, Vec<u8>)> = Vec::new();
            let mut skipped = Vec::new();
            let mut total: u64 = 0;
            for file in files {
                match confine::clean_site_path(&file.path, &limits.allowed_extensions) {
                    Ok(rel) => {
                        let size = file.data.len() as u64;
                        if size > limits.max_file_bytes {
                            return Err(SiteError::Invalid(format!(
                                "{} is {size} bytes; the limit is {} bytes per file",
                                file.path, limits.max_file_bytes
                            )));
                        }
                        total += size;
                        accepted.push((rel, file.data));
                    }
                    Err(SiteError::Path { path, reason }) => skipped.push((path, reason)),
                    Err(e) => return Err(e),
                }
            }
            if total > limits.max_site_bytes {
                return Err(SiteError::Invalid(format!(
                    "the site is {total} bytes; the limit is {} bytes",
                    limits.max_site_bytes
                )));
            }
            if !accepted
                .iter()
                .any(|(rel, _)| rel.as_os_str() == "index.html")
            {
                return Err(SiteError::Invalid(
                    "the site has no index.html at its top level".into(),
                ));
            }

            // Mirror: clear the working tree (keeping .git), then write the new files.
            for entry in std::fs::read_dir(&repo)? {
                let entry = entry?;
                if entry.file_name() == ".git" {
                    continue;
                }
                if entry.file_type()?.is_dir() {
                    std::fs::remove_dir_all(entry.path())?;
                } else {
                    std::fs::remove_file(entry.path())?;
                }
            }
            let count = accepted.len();
            for (rel, data) in accepted {
                let full = confine::resolve_in(&repo, &rel)?;
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(full, data)?;
            }
            Ok((count, total, skipped))
        })
        .await?;

        let publish = self.publish_locked(&site, message, author).await?;
        drop(guard);
        Ok(ImportReport {
            files: count,
            bytes,
            skipped,
            publish,
        })
    }

    /// Check the working tree against the publishing rules.
    pub async fn validate(&self, name: &str) -> Result<crate::validate::Report> {
        let site = self.get(name).await?;
        self.validate_site(&site).await
    }

    async fn validate_site(&self, site: &Site) -> Result<crate::validate::Report> {
        let repo = self.inner.data.site_repo(&site.name);
        let limits = self.inner.limits.clone();
        blocking(move || Ok(crate::validate::validate(&repo, &limits)?)).await
    }

    // ------------------------------------------------------------------ publishing

    /// Commit the working tree and make it live. If the new release fails its health
    /// check, the previous release is restored and an error is returned.
    pub async fn publish(
        &self,
        name: &str,
        message: &str,
        author: Author,
    ) -> Result<PublishOutcome> {
        let site = self.get(name).await?;
        let _guard = self.lock(&site.name).await;
        // Re-read under the lock: another publish may have finished in the meantime.
        let site = self.get(name).await?;
        self.publish_locked(&site, message, author).await
    }

    async fn publish_locked(
        &self,
        site: &Site,
        message: &str,
        author: Author,
    ) -> Result<PublishOutcome> {
        let message = message.trim();
        let message = if message.is_empty() {
            "Update site"
        } else {
            message
        };
        let repo = self.inner.data.site_repo(&site.name);
        let url = self.site_url(site).await?;

        let report = self.validate_site(site).await?;
        if !report.ok() {
            return Err(SiteError::Validation {
                site: site.name.clone(),
                problems: report.errors().map(ToString::to_string).collect(),
            });
        }

        let committed = {
            let repo = repo.clone();
            let message = message.to_owned();
            blocking(move || git::commit_all(&repo, &message, author.as_str())).await?
        };
        let sha = committed.sha;

        // Already live and unchanged: nothing to do.
        if !committed.changed
            && let Some(current) = site.current_release
        {
            let current = self.release(current).await?;
            if current.sha == sha {
                return Ok(PublishOutcome {
                    release: current,
                    changed: false,
                    url,
                });
            }
        }

        self.ensure_release_dir(&site.name, &sha).await?;

        let plan = ReleasePlan {
            site_id: site.id,
            site: site.name.clone(),
            sha: sha.clone(),
            root: self.inner.data.site_release(&site.name, &sha),
        };
        let hooks = self.inner.release_hooks.get();
        if let Some(hooks) = hooks
            && let Err(reason) = hooks.prepare(&plan).await
        {
            self.audit(
                author,
                "site.publish.refused",
                site.name.as_str(),
                Some(&reason),
            )
            .await;
            return Err(SiteError::Validation {
                site: site.name.clone(),
                problems: vec![reason],
            });
        }

        let now = now_ms();
        let author_str = author.as_str();
        let mut tx = sw_core::db::begin_write(&self.inner.db).await?;
        let release_id = sqlx::query_scalar!(
            r#"INSERT INTO releases (site_id, sha, message, author, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
               ON CONFLICT (site_id, sha) DO UPDATE SET sha = excluded.sha
               RETURNING id as "id!""#,
            site.id,
            sha,
            message,
            author_str,
            now
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE sites SET current_release = ?1, updated_at = ?2 WHERE id = ?3",
            release_id,
            now,
            site.id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.reload_routes().await?;

        if let Err(reason) = self.health_check(site, &sha).await {
            tracing::warn!(site = %site.name, %sha, %reason, "release failed its health check; rolling back");
            sqlx::query!(
                "UPDATE sites SET current_release = ?1, updated_at = ?2 WHERE id = ?3",
                site.current_release,
                now,
                site.id
            )
            .execute(&self.inner.db)
            .await?;
            self.reload_routes().await?;
            if let Some(hooks) = hooks {
                hooks.abandon(&plan).await;
            }
            self.audit(
                author,
                "site.publish.rolled_back",
                site.name.as_str(),
                Some(&reason),
            )
            .await;
            return Err(SiteError::HealthCheck { sha, reason });
        }

        if let Some(hooks) = hooks {
            hooks.commit(&plan).await;
        }
        self.prune_releases(site).await?;
        self.audit(author, "site.publish", site.name.as_str(), Some(&sha))
            .await;
        Ok(PublishOutcome {
            release: self.release(release_id).await?,
            changed: true,
            url,
        })
    }

    /// Switch to an earlier release: the one before the current release, or the one whose
    /// sha starts with `target`. The working tree is reset to match, so later edits build
    /// on what is live.
    pub async fn rollback(
        &self,
        name: &str,
        target: Option<&str>,
        author: Author,
    ) -> Result<Release> {
        let site = self.get(name).await?;
        let _guard = self.lock(&site.name).await;
        let site = self.get(name).await?;
        let current = site
            .current_release
            .ok_or_else(|| SiteError::Invalid(format!("site {name} has never been published")))?;

        let release = match target {
            None => sqlx::query_as!(
                Release,
                r#"SELECT id as "id!", site_id, sha, message, author, created_at
                   FROM releases WHERE site_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT 1"#,
                site.id,
                current
            )
            .fetch_optional(&self.inner.db)
            .await?
            .ok_or_else(|| {
                SiteError::Invalid(format!(
                    "site {name} has no earlier release to roll back to"
                ))
            })?,
            Some(prefix) => {
                let prefix = prefix.trim().to_ascii_lowercase();
                if prefix.len() < 4 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(SiteError::Invalid(
                        "name a release by at least the first 4 characters of its commit sha"
                            .into(),
                    ));
                }
                let pattern = format!("{prefix}%");
                let mut matches = sqlx::query_as!(
                    Release,
                    r#"SELECT id as "id!", site_id, sha, message, author, created_at
                       FROM releases WHERE site_id = ?1 AND sha LIKE ?2 ORDER BY id DESC LIMIT 2"#,
                    site.id,
                    pattern
                )
                .fetch_all(&self.inner.db)
                .await?;
                match matches.len() {
                    0 => {
                        return Err(SiteError::Invalid(format!(
                            "site {name} has no release {prefix}"
                        )));
                    }
                    1 => matches.remove(0),
                    _ => {
                        return Err(SiteError::Invalid(format!(
                            "release {prefix} is ambiguous; use more characters"
                        )));
                    }
                }
            }
        };
        if release.id == current {
            return Err(SiteError::Invalid(format!(
                "release {} is already live",
                &release.sha[..7.min(release.sha.len())]
            )));
        }

        self.ensure_release_dir(&site.name, &release.sha).await?;
        let now = now_ms();
        sqlx::query!(
            "UPDATE sites SET current_release = ?1, updated_at = ?2 WHERE id = ?3",
            release.id,
            now,
            site.id
        )
        .execute(&self.inner.db)
        .await?;
        self.reload_routes().await?;

        let repo = self.inner.data.site_repo(&site.name);
        let sha = release.sha.clone();
        blocking(move || git::restore_worktree(&repo, &sha)).await?;
        self.audit(author, "site.rollback", name, Some(&release.sha))
            .await;
        Ok(release)
    }

    /// Build an archive of the site's live release in the exports directory.
    pub async fn export(
        &self,
        name: &str,
        format: crate::export::ExportFormat,
    ) -> Result<crate::export::Export> {
        let site = self.get(name).await?;
        let current = site.current_release.ok_or_else(|| {
            SiteError::Invalid(format!(
                "site {name} has not been published yet, so there is nothing to export"
            ))
        })?;
        let release = self.release(current).await?;
        self.ensure_release_dir(&site.name, &release.sha).await?;
        let release_dir = self.inner.data.site_release(&site.name, &release.sha);
        let exports_dir = self.inner.data.exports_dir();
        let name = name.to_owned();
        let export = blocking(move || {
            crate::export::build(&name, &release.sha, &release_dir, &exports_dir, format)
        })
        .await?;
        self.audit(
            Author::User,
            "site.export",
            site.name.as_str(),
            Some(format.as_str()),
        )
        .await;
        Ok(export)
    }

    // ------------------------------------------------------------------ deleting

    /// Soft-delete a site: it stops being served at once and can be restored until it is
    /// purged. Callers must have obtained human confirmation first.
    pub async fn delete(&self, name: &str, author: Author) -> Result<Site> {
        let site = self.get(name).await?;
        let _guard = self.lock(&site.name).await;
        let now = now_ms();
        sqlx::query!(
            "UPDATE sites SET status = 'deleted', deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
            now,
            site.id
        )
        .execute(&self.inner.db)
        .await?;
        self.reload_routes().await?;
        self.audit(author, "site.delete", name, None).await;
        self.find(name)
            .await?
            .ok_or_else(|| SiteError::NotFound(name.to_owned()))
    }

    /// Bring a soft-deleted site back.
    pub async fn restore(&self, name: &str, author: Author) -> Result<Site> {
        let site = self
            .find(name)
            .await?
            .ok_or_else(|| SiteError::NotFound(name.to_owned()))?;
        if site.status != SiteStatus::Deleted {
            return Err(SiteError::Invalid(format!("site {name} is not deleted")));
        }
        let now = now_ms();
        sqlx::query!(
            "UPDATE sites SET status = 'active', deleted_at = NULL, updated_at = ?1 WHERE id = ?2",
            now,
            site.id
        )
        .execute(&self.inner.db)
        .await?;
        self.reload_routes().await?;
        self.audit(author, "site.restore", name, None).await;
        self.get(name).await
    }

    /// Permanently remove sites that were deleted more than `purge_after_days` ago.
    /// Returns the names of the purged sites.
    pub async fn purge_expired(&self) -> Result<Vec<String>> {
        let cutoff = now_ms() - i64::from(self.inner.limits.purge_after_days) * MS_PER_DAY;
        let expired = sqlx::query!(
            r#"SELECT id as "id!", name FROM sites WHERE status = 'deleted' AND deleted_at <= ?1"#,
            cutoff
        )
        .fetch_all(&self.inner.db)
        .await?;
        let mut purged = Vec::new();
        for row in expired {
            let name = SiteName::parse(&row.name)?;
            let _guard = self.lock(&name).await;
            let dir = self.inner.data.site_dir(&name);
            blocking(move || match std::fs::remove_dir_all(&dir) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            })
            .await?;
            let mut tx = sw_core::db::begin_write(&self.inner.db).await?;
            sqlx::query!(
                "UPDATE sites SET current_release = NULL WHERE id = ?1",
                row.id
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!("DELETE FROM requests WHERE site_id = ?1", row.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query!("DELETE FROM sites WHERE id = ?1", row.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            sw_core::db::audit(
                &self.inner.db,
                "system",
                "site.purge",
                Some(&row.name),
                None,
            )
            .await;
            purged.push(row.name);
        }
        Ok(purged)
    }

    // ------------------------------------------------------------------ internals

    async fn lock(&self, name: &SiteName) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.inner.locks.lock().unwrap_or_else(|e| e.into_inner());
            locks.entry(name.as_str().to_owned()).or_default().clone()
        };
        mutex.lock_owned().await
    }

    async fn site_url(&self, site: &Site) -> Result<String> {
        let primary = sqlx::query_scalar!(
            "SELECT hostname FROM site_hostnames WHERE site_id = ?1 ORDER BY is_primary DESC, id LIMIT 1",
            site.id
        )
        .fetch_optional(&self.inner.db)
        .await?
        .unwrap_or_else(|| self.inner.domains.site_hostname(&site.name));
        Ok(self.inner.domains.public_url(&primary))
    }

    /// Make sure `releases/<sha>/` exists, exporting the commit if needed. The export goes
    /// to a temporary directory first and is renamed into place, so a release directory
    /// is always complete.
    async fn ensure_release_dir(&self, name: &SiteName, sha: &str) -> Result<()> {
        let repo = self.inner.data.site_repo(name);
        let releases = self.inner.data.site_releases(name);
        let dest = self.inner.data.site_release(name, sha);
        let sha = sha.to_owned();
        blocking(move || {
            if dest.is_dir() {
                return Ok(());
            }
            std::fs::create_dir_all(&releases)?;
            let tmp = releases.join(format!(".tmp-{sha}-{}", now_ms()));
            let exported = git::export_commit(&repo, &sha, &tmp)
                .and_then(|()| Ok(std::fs::rename(&tmp, &dest)?));
            if exported.is_err() {
                let _ = std::fs::remove_dir_all(&tmp);
            }
            exported
        })
        .await
    }

    /// The checks every new release must pass once it is live.
    async fn health_check(&self, site: &Site, sha: &str) -> Result<(), String> {
        let primary = sqlx::query_scalar!(
            "SELECT hostname FROM site_hostnames WHERE site_id = ?1 ORDER BY is_primary DESC, id LIMIT 1",
            site.id
        )
        .fetch_optional(&self.inner.db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the site has no hostname")?;

        let routes = self.routes();
        let route = routes
            .get(&primary)
            .ok_or_else(|| format!("{primary} is not routed"))?
            .clone();
        if route.sha != sha {
            return Err(format!(
                "{primary} serves {} instead of the new release",
                route.sha
            ));
        }
        let index = route.root.join("index.html");
        match tokio::fs::metadata(&index).await {
            Ok(meta) if meta.is_file() && meta.len() > 0 => {}
            Ok(_) => return Err("the release's index.html is empty".into()),
            Err(e) => return Err(format!("the release's index.html cannot be read: {e}")),
        }
        if let Some(check) = self.inner.health_check.get() {
            check(route, primary).await?;
        }
        Ok(())
    }

    /// Delete release directories beyond the newest `keep_releases`, never the live one.
    /// Their database rows stay: a pruned release can still be rolled back to, because
    /// its commit is in the repository.
    async fn prune_releases(&self, site: &Site) -> Result<()> {
        let keep = i64::from(self.inner.limits.keep_releases);
        let old = sqlx::query_scalar!(
            r#"SELECT sha FROM releases
               WHERE site_id = ?1
                 AND id NOT IN (SELECT id FROM releases WHERE site_id = ?1 ORDER BY id DESC LIMIT ?2)
                 AND id IS NOT (SELECT current_release FROM sites WHERE id = ?1)"#,
            site.id,
            keep
        )
        .fetch_all(&self.inner.db)
        .await?;
        if old.is_empty() {
            return Ok(());
        }
        let dirs: Vec<PathBuf> = old
            .iter()
            .map(|sha| self.inner.data.site_release(&site.name, sha))
            .collect();
        blocking(move || {
            for dir in dirs {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(dir = %dir.display(), error = %e, "could not prune release")
                    }
                }
            }
            Ok(())
        })
        .await
    }

    /// Rebuild the routing table from the database and swap it in.
    pub async fn reload_routes(&self) -> Result<()> {
        let rows = sqlx::query!(
            r#"SELECT h.hostname, s.id as "site_id!", s.name, r.id as "release_id!", r.sha,
                      (SELECT p.hostname FROM site_hostnames p WHERE p.site_id = s.id ORDER BY p.is_primary DESC, p.id LIMIT 1) as "primary_hostname!"
               FROM site_hostnames h
               JOIN sites s ON s.id = h.site_id
               JOIN releases r ON r.id = s.current_release
               WHERE s.status = 'active' AND h.redirect = 0"#
        )
        .fetch_all(&self.inner.db)
        .await?;
        let redirects = sqlx::query!(
            r#"SELECT h.hostname,
                      (SELECT p.hostname FROM site_hostnames p WHERE p.site_id = s.id ORDER BY p.is_primary DESC, p.id LIMIT 1) as "primary_hostname!"
               FROM site_hostnames h
               JOIN sites s ON s.id = h.site_id
               WHERE s.status = 'active' AND h.redirect = 1"#
        )
        .fetch_all(&self.inner.db)
        .await?
        .into_iter()
        .filter(|r| r.hostname != r.primary_hostname)
        .map(|r| (r.hostname, r.primary_hostname))
        .collect();

        let mut by_host = HashMap::with_capacity(rows.len());
        let mut by_site: HashMap<i64, Arc<Route>> = HashMap::new();
        for row in rows {
            let route = match by_site.get(&row.site_id) {
                Some(route) => route.clone(),
                None => {
                    let site = SiteName::parse(&row.name)?;
                    let root = self.inner.data.site_release(&site, &row.sha);
                    if !root.is_dir() {
                        // Pruned or lost on disk; the commit is still in the repository.
                        self.ensure_release_dir(&site, &row.sha).await?;
                    }
                    let route = Arc::new(Route {
                        site_id: row.site_id,
                        site,
                        release_id: row.release_id,
                        sha: row.sha,
                        primary_hostname: row.primary_hostname,
                        backend: root.join("backend/index.js").is_file(),
                        root,
                    });
                    by_site.insert(row.site_id, route.clone());
                    route
                }
            };
            by_host.insert(row.hostname, route);
        }
        self.inner
            .routes
            .store(Arc::new(RouteTable::new(by_host, redirects)));
        Ok(())
    }

    async fn audit(&self, author: Author, action: &str, target: &str, detail: Option<&str>) {
        sw_core::db::audit(
            &self.inner.db,
            author.as_str(),
            action,
            Some(target),
            detail,
        )
        .await;
    }
}
