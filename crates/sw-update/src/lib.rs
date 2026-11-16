//! Sitewright updater: release checks, signature verification and binary self-replacement.
//!
//! An update is installed only if the downloaded archive carries a valid minisign
//! signature made with the project's release key (compiled into this binary) *and* its
//! SHA-256 matches the published checksum. The previous binary is kept beside the new one
//! so `sitewright update --rollback` can put it back.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The release signing public key, compiled in from `release/minisign.pub`.
pub const PUBLIC_KEY: &str = include_str!("../../../release/minisign.pub");
/// Where releases are published.
pub const DEFAULT_REPO: &str = "matthew-chandler/sitewright";
pub const DEFAULT_API: &str = "https://api.github.com";
/// The version of the running binary.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Largest release archive we are willing to download.
const MAX_ARCHIVE_BYTES: usize = 200 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("could not reach the release server: {0}")]
    Network(String),
    #[error("unexpected answer from the release server: {0}")]
    Protocol(String),
    #[error("release {version} has no build for this platform ({target})")]
    NoBuild { version: String, target: String },
    /// The download is not what the project published. Never install it.
    #[error("the downloaded update did not verify and was NOT installed: {0}")]
    Verification(String),
    #[error("{0}")]
    State(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// The platform this binary was built for, as it appears in release asset names.
pub fn target() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-musl"
    } else {
        "x86_64-unknown-linux-musl"
    }
}

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub url: String,
    #[serde(skip)]
    assets: Vec<(String, String)>,
}

impl Release {
    /// Whether this release is newer than `current`.
    pub fn is_newer_than(&self, current: &Version) -> bool {
        self.version > *current
    }

    /// Whether automatic updates may install it: patch and minor releases only, never a
    /// new major version and never a pre-release.
    pub fn auto_installable_from(&self, current: &Version) -> bool {
        self.is_newer_than(current)
            && self.version.major == current.major
            && self.version.pre.is_empty()
    }

    fn asset(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, url)| url.as_str())
    }
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// Talks to the release server and verifies what it downloads.
#[derive(Debug, Clone)]
pub struct Updater {
    api: String,
    repo: String,
    public_key: String,
    http: reqwest::Client,
}

impl Updater {
    /// An updater for the official releases, trusting the compiled-in key.
    pub fn official() -> Result<Self, UpdateError> {
        Self::new(DEFAULT_API, DEFAULT_REPO, PUBLIC_KEY)
    }

    /// An updater for another server, repository or key. Used by tests and forks.
    pub fn new(api: &str, repo: &str, public_key: &str) -> Result<Self, UpdateError> {
        sw_core::crypto::install_default_provider();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("sitewright/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| UpdateError::Network(e.to_string()))?;
        Ok(Self {
            api: api.trim_end_matches('/').to_owned(),
            repo: repo.to_owned(),
            public_key: public_key.to_owned(),
            http,
        })
    }

    /// The latest published release, if there is one.
    pub async fn latest(&self) -> Result<Option<Release>, UpdateError> {
        let url = format!("{}/repos/{}/releases/latest", self.api, self.repo);
        let response = self
            .http
            .get(&url)
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| UpdateError::Network(e.without_url().to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None); // no releases yet
        }
        if !response.status().is_success() {
            return Err(UpdateError::Protocol(format!(
                "{} for {url}",
                response.status()
            )));
        }
        let api: ApiRelease = response
            .json()
            .await
            .map_err(|e| UpdateError::Protocol(e.to_string()))?;
        if api.draft {
            return Ok(None);
        }
        let version = Version::parse(api.tag_name.trim_start_matches('v')).map_err(|e| {
            UpdateError::Protocol(format!(
                "release tag {:?} is not a version: {e}",
                api.tag_name
            ))
        })?;
        Ok(Some(Release {
            version,
            tag: api.tag_name,
            url: api.html_url,
            assets: api
                .assets
                .into_iter()
                .map(|a| (a.name, a.browser_download_url))
                .collect(),
        }))
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, UpdateError> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| UpdateError::Network(e.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(UpdateError::Protocol(format!(
                "{} while downloading {url}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| UpdateError::Network(e.without_url().to_string()))?;
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(UpdateError::Protocol(
                "the download is implausibly large".into(),
            ));
        }
        Ok(bytes.to_vec())
    }

    /// Download the build of `release` for this platform, verify its signature and
    /// checksum, and return the new binary's bytes.
    pub async fn download_verified(&self, release: &Release) -> Result<Vec<u8>, UpdateError> {
        let archive_name = format!("sitewright-{}.tar.gz", target());
        let missing = || UpdateError::NoBuild {
            version: release.version.to_string(),
            target: target().to_owned(),
        };
        let archive_url = release.asset(&archive_name).ok_or_else(missing)?;
        let signature_url = release
            .asset(&format!("{archive_name}.minisig"))
            .ok_or_else(|| {
                UpdateError::Verification(format!(
                    "release {} has no signature for {archive_name}",
                    release.version
                ))
            })?;
        let checksum_url = release
            .asset(&format!("{archive_name}.sha256"))
            .ok_or_else(|| {
                UpdateError::Verification(format!(
                    "release {} has no checksum for {archive_name}",
                    release.version
                ))
            })?;

        let archive = self.download(archive_url).await?;
        let signature = String::from_utf8_lossy(&self.download(signature_url).await?).into_owned();
        let checksum = String::from_utf8_lossy(&self.download(checksum_url).await?).into_owned();
        verify(&archive, &signature, &checksum, &self.public_key)?;
        extract_binary(&archive)
    }
}

/// Check an archive against its minisign signature and its SHA-256 checksum.
pub fn verify(
    archive: &[u8],
    signature: &str,
    checksum: &str,
    public_key: &str,
) -> Result<(), UpdateError> {
    let bad = |what: &str| UpdateError::Verification(what.to_owned());
    let key = PublicKey::decode(public_key.trim())
        .or_else(|_| PublicKey::from_base64(public_key.trim()))
        .map_err(|_| bad("the built-in release key is unreadable"))?;
    let signature =
        Signature::decode(signature).map_err(|_| bad("the signature file is malformed"))?;
    key.verify(archive, &signature, false)
        .map_err(|_| bad("the signature does not match the release key"))?;

    let expected = checksum
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let actual = hex::encode(Sha256::digest(archive));
    if expected.len() != 64 || expected != actual {
        return Err(bad("the SHA-256 checksum does not match"));
    }
    Ok(())
}

/// Pull the `sitewright` executable out of a release archive.
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>, UpdateError> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let unreadable = |e: std::io::Error| {
        UpdateError::Protocol(format!("the release archive is unreadable: {e}"))
    };
    for entry in tar.entries().map_err(unreadable)? {
        let mut entry = entry.map_err(unreadable)?;
        let is_binary = entry.header().entry_type().is_file()
            && entry
                .path()
                .is_ok_and(|p| p.file_name().is_some_and(|n| n == "sitewright"));
        if is_binary {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(unreadable)?;
            if bytes.len() < 1024 || !bytes.starts_with(b"\x7fELF") {
                return Err(UpdateError::Protocol(
                    "the release archive does not contain a Linux executable".into(),
                ));
            }
            return Ok(bytes);
        }
    }
    Err(UpdateError::Protocol(
        "the release archive does not contain the sitewright binary".into(),
    ))
}

/// Path of the kept previous binary, beside `exe`.
pub fn previous_path(exe: &Path) -> PathBuf {
    let mut name = exe
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".prev");
    exe.with_file_name(name)
}

/// Replace the executable at `exe` with `binary`, keeping the current one as
/// `<exe>.prev`. The swap is atomic: a crash leaves either the old or the new binary.
pub fn install(binary: &[u8], exe: &Path) -> Result<PathBuf, UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    let previous = previous_path(exe);
    std::fs::copy(exe, &previous)?;
    let mut staged_name = exe
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    staged_name.push(".new");
    let staged = exe.with_file_name(staged_name);
    std::fs::write(&staged, binary)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    let swapped = swap_in(&staged, exe);
    if swapped.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    swapped?;
    Ok(previous)
}

/// Put the kept previous binary back, keeping the current one as the new "previous"
/// (so a rollback can itself be undone).
pub fn rollback(exe: &Path) -> Result<(), UpdateError> {
    let previous = previous_path(exe);
    if !previous.is_file() {
        return Err(UpdateError::State(format!(
            "there is no previous version to roll back to ({} does not exist)",
            previous.display()
        )));
    }
    let mut aside_name = exe
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    aside_name.push(".rollback-tmp");
    let aside = exe.with_file_name(aside_name);
    std::fs::copy(exe, &aside)?;
    std::fs::copy(&previous, exe.with_extension("restoring"))?;
    swap_in(&exe.with_extension("restoring"), exe)?;
    std::fs::rename(&aside, &previous)?;
    Ok(())
}

/// Move `staged` over `exe`. For the running executable this goes through
/// `self_replace`, which handles the platform's rules for replacing a file in use.
fn swap_in(staged: &Path, exe: &Path) -> Result<(), UpdateError> {
    let running = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    if running.is_some() && running == exe.canonicalize().ok() {
        self_replace::self_replace(staged)?;
        let _ = std::fs::remove_file(staged);
    } else {
        std::fs::rename(staged, exe)?;
    }
    Ok(())
}

/// What `<data>/backups/last-migration.json` records: which version migrated the
/// database, and the copy taken just before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRecord {
    pub migrated_by: String,
    pub backup: PathBuf,
}

const LAST_MIGRATION: &str = "last-migration.json";
const RESTORE_PENDING: &str = "restore-pending.json";

/// Remember that this version migrated the database and where the pre-migration copy is.
pub fn record_migration(backups_dir: &Path, backup: &Path) -> std::io::Result<()> {
    let record = MigrationRecord {
        migrated_by: CURRENT_VERSION.to_owned(),
        backup: backup.to_owned(),
    };
    std::fs::write(
        backups_dir.join(LAST_MIGRATION),
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    )
}

/// Called by `update --rollback` after the binary has been swapped back: if the version
/// being rolled back migrated the database, ask the next start to restore the copy
/// taken before that migration. Returns the backup that will be restored.
pub fn request_database_restore(backups_dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let Ok(text) = std::fs::read_to_string(backups_dir.join(LAST_MIGRATION)) else {
        return Ok(None);
    };
    let Ok(record) = serde_json::from_str::<MigrationRecord>(&text) else {
        return Ok(None);
    };
    if record.migrated_by != CURRENT_VERSION || !record.backup.is_file() {
        return Ok(None);
    }
    std::fs::write(backups_dir.join(RESTORE_PENDING), text)?;
    Ok(Some(record.backup))
}

/// Called at start-up, before the database is opened: if a rollback asked for it,
/// replace the database with its pre-migration copy. The newer database is kept beside
/// it, never deleted. Returns the restored backup's path.
pub fn restore_database_if_requested(
    backups_dir: &Path,
    db_file: &Path,
) -> std::io::Result<Option<PathBuf>> {
    let marker = backups_dir.join(RESTORE_PENDING);
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return Ok(None);
    };
    let record: Option<MigrationRecord> = serde_json::from_str(&text).ok();
    let Some(record) = record.filter(|r| r.backup.is_file()) else {
        std::fs::remove_file(&marker)?;
        return Ok(None);
    };
    if db_file.exists() {
        let mut kept = db_file.as_os_str().to_owned();
        kept.push(format!(".from-{}", record.migrated_by));
        std::fs::rename(db_file, PathBuf::from(kept))?;
    }
    for suffix in ["-wal", "-shm"] {
        let mut side = db_file.as_os_str().to_owned();
        side.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(side));
    }
    std::fs::copy(&record.backup, db_file)?;
    std::fs::remove_file(&marker)?;
    let _ = std::fs::remove_file(backups_dir.join(LAST_MIGRATION));
    Ok(Some(record.backup))
}
