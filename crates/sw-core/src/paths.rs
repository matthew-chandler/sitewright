//! The data directory layout. All persistent state lives under one directory.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::names::SiteName;

/// Default data directory for an installed daemon.
pub const DEFAULT_DATA_DIR: &str = "/var/lib/sitewright";

/// Paths inside the data directory. Cheap to clone.
#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Create the directory tree (owner-only permissions) if it does not exist yet.
    pub fn ensure(&self) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        for dir in [
            self.root.clone(),
            self.db_dir(),
            self.sites_dir(),
            self.certs_dir(),
            self.acme_dir(),
            self.skills_dir(),
            self.exports_dir(),
            self.backups_dir(),
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets.toml")
    }
    /// Secrets that sites' backends may use, entered by the owner.
    pub fn site_secrets_file(&self) -> PathBuf {
        self.root.join("site-secrets.toml")
    }
    pub fn admin_socket(&self) -> PathBuf {
        self.root.join("admin.sock")
    }
    pub fn db_dir(&self) -> PathBuf {
        self.root.join("db")
    }
    pub fn db_file(&self) -> PathBuf {
        self.db_dir().join("sitewright.db")
    }
    pub fn sites_dir(&self) -> PathBuf {
        self.root.join("sites")
    }
    pub fn site_dir(&self, name: &SiteName) -> PathBuf {
        self.sites_dir().join(name.as_str())
    }
    /// The site's git working tree.
    pub fn site_repo(&self, name: &SiteName) -> PathBuf {
        self.site_dir(name).join("repo")
    }
    pub fn site_releases(&self, name: &SiteName) -> PathBuf {
        self.site_dir(name).join("releases")
    }
    /// An immutable published copy of the site at commit `sha`.
    pub fn site_release(&self, name: &SiteName, sha: &str) -> PathBuf {
        self.site_releases(name).join(sha)
    }
    /// A site's backend data: its database. Lives beside the repository, not in it.
    pub fn site_data(&self, name: &SiteName) -> PathBuf {
        self.site_dir(name).join("data")
    }
    /// A site's own database, used by its backend.
    pub fn site_db(&self, name: &SiteName) -> PathBuf {
        self.site_data(name).join("site.db")
    }
    pub fn certs_dir(&self) -> PathBuf {
        self.root.join("certs")
    }
    pub fn acme_dir(&self) -> PathBuf {
        self.root.join("acme")
    }
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }
    pub fn exports_dir(&self) -> PathBuf {
        self.root.join("exports")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
}
