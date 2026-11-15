//! Daemon configuration, read from a TOML file. Secrets live in a separate file.
//!
//! Every key has a default, so an empty file (or no file) is a valid local-dev config.
//! `docs/configuration.md` documents each key.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::names::{SiteName, normalize_hostname};
use crate::paths::{DEFAULT_DATA_DIR, DataDir};

/// The whole configuration file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where all state lives. The `--data-dir` flag and `SITEWRIGHT_DATA_DIR` override it.
    pub data_dir: Option<PathBuf>,
    pub server: ServerConfig,
    pub domains: DomainsConfig,
    pub sites: SitesConfig,
}

/// How the daemon is exposed to the network.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub mode: ServeMode,
    /// Plain-HTTP listener. In proxy mode this is the only listener.
    pub listen: SocketAddr,
    /// Peers whose `X-Forwarded-For`, `X-Forwarded-Proto` and `CF-Connecting-IP` headers
    /// are believed. Empty means no forwarding header is ever trusted.
    pub trusted_proxies: Vec<IpNet>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            mode: ServeMode::Proxy,
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            trusted_proxies: Vec::new(),
        }
    }
}

impl ServerConfig {
    /// Whether forwarding headers from `peer` should be believed.
    pub fn is_trusted_proxy(&self, peer: IpAddr) -> bool {
        // Compare IPv4-mapped IPv6 peers (::ffff:a.b.c.d) as IPv4.
        let peer = match peer {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(peer, IpAddr::V4),
            v4 => v4,
        };
        self.trusted_proxies.iter().any(|net| net.contains(&peer))
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServeMode {
    /// Behind nginx, Caddy or Cloudflare, which terminate TLS.
    #[default]
    Proxy,
    /// The daemon binds 80/443 itself and gets certificates from Let's Encrypt.
    Standalone,
}

/// Which hostnames belong to this server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DomainsConfig {
    /// DNS zones sites live under. A site's primary hostname is `<name>.<zones[0]>`.
    pub zones: Vec<String>,
    /// Hostname of the admin UI. Default: `admin.<zones[0]>` (`localhost` for local dev).
    pub admin_host: Option<String>,
    /// Scheme visitors use to reach sites and the admin UI: `http` or `https`.
    pub public_scheme: Scheme,
    /// Public port, when it is not the scheme's default (for example 8080 in local dev).
    pub public_port: Option<u16>,
}

impl Default for DomainsConfig {
    fn default() -> Self {
        Self {
            zones: vec!["localhost".to_owned()],
            admin_host: None,
            public_scheme: Scheme::Http,
            public_port: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    #[default]
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

impl DomainsConfig {
    /// The zone new sites are created in.
    pub fn default_zone(&self) -> &str {
        self.zones.first().map_or("localhost", String::as_str)
    }

    /// The admin UI's hostname.
    pub fn admin_host(&self) -> String {
        match &self.admin_host {
            Some(host) => host.clone(),
            None if self.default_zone() == "localhost" => "localhost".to_owned(),
            None => format!("admin.{}", self.default_zone()),
        }
    }

    /// The primary hostname for a site called `name`.
    pub fn site_hostname(&self, name: &SiteName) -> String {
        format!("{name}.{}", self.default_zone())
    }

    /// Whether `hostname` is inside one of the configured zones.
    pub fn in_zone(&self, hostname: &str) -> bool {
        self.zones.iter().any(|zone| {
            hostname
                .strip_suffix(zone.as_str())
                .is_some_and(|rest| rest.ends_with('.'))
        })
    }

    /// The public URL for `hostname`, for example `https://hikes.example.com`.
    pub fn public_url(&self, hostname: &str) -> String {
        let default_port = match self.public_scheme {
            Scheme::Http => 80,
            Scheme::Https => 443,
        };
        match self.public_port {
            Some(port) if port != default_port => {
                format!("{}://{hostname}:{port}", self.public_scheme.as_str())
            }
            _ => format!("{}://{hostname}", self.public_scheme.as_str()),
        }
    }
}

/// Limits on what a site may contain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SitesConfig {
    /// Largest single file, in bytes.
    pub max_file_bytes: u64,
    /// Largest total size of a site's files, in bytes.
    pub max_site_bytes: u64,
    /// How many published releases to keep on disk per site.
    pub keep_releases: u32,
    /// Days a deleted site stays recoverable before it is purged.
    pub purge_after_days: u32,
    /// File extensions (lowercase, no dot) a site may contain.
    pub allowed_extensions: Vec<String>,
}

impl Default for SitesConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024,
            max_site_bytes: 20 * 1024 * 1024,
            keep_releases: 10,
            purge_after_days: 7,
            allowed_extensions: [
                "html",
                "htm",
                "css",
                "js",
                "mjs",
                "json",
                "txt",
                "xml",
                "svg",
                "md",
                "csv",
                "webmanifest",
                "map",
                "png",
                "jpg",
                "jpeg",
                "gif",
                "webp",
                "avif",
                "ico",
                "woff",
                "woff2",
                "ttf",
                "otf",
                "pdf",
                "mp3",
                "mp4",
                "webm",
                "wasm",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }
}

/// Where the configuration came from, after resolving flags, environment and defaults.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub data_dir: DataDir,
    /// The file that was read, if any.
    pub source: Option<PathBuf>,
}

impl Config {
    /// Parse a configuration file's contents and validate it.
    pub fn from_toml(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Read and validate a configuration file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_toml(&text).map_err(|e| match e {
            Error::Config(msg) => Error::Config(format!("{}: {msg}", path.display())),
            other => other,
        })
    }

    /// Resolve the configuration and data directory.
    ///
    /// The config file is the first of: `config_flag`; `<data_dir_flag>/config.toml`;
    /// `/etc/sitewright/config.toml`; `/var/lib/sitewright/config.toml`. A file named by a
    /// flag must exist; the others are optional. The data directory is the first of:
    /// `data_dir_flag`; the file's `data_dir`; the directory holding the file if it is
    /// named `config.toml` inside a data directory; `/var/lib/sitewright`.
    pub fn load(config_flag: Option<&Path>, data_dir_flag: Option<&Path>) -> Result<Loaded> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = data_dir_flag {
            candidates.push(dir.join("config.toml"));
        }
        candidates.push(PathBuf::from("/etc/sitewright/config.toml"));
        candidates.push(Path::new(DEFAULT_DATA_DIR).join("config.toml"));

        let source = match config_flag {
            Some(path) => Some(path.to_owned()),
            None => candidates.into_iter().find(|p| p.is_file()),
        };
        let config = match &source {
            Some(path) => Self::from_file(path)?,
            None => Config::default(),
        };
        let root = data_dir_flag
            .map(Path::to_owned)
            .or_else(|| config.data_dir.clone())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));
        Ok(Loaded {
            config,
            data_dir: DataDir::new(root),
            source,
        })
    }

    /// Check cross-field rules that serde cannot express.
    pub fn validate(&self) -> Result<()> {
        if self.domains.zones.is_empty() {
            return Err(Error::Config(
                "domains.zones must list at least one zone".into(),
            ));
        }
        for zone in &self.domains.zones {
            if normalize_hostname(zone).as_deref() != Some(zone.as_str()) {
                return Err(Error::Config(format!(
                    "domains.zones: {zone:?} is not a lowercase hostname"
                )));
            }
        }
        if let Some(host) = &self.domains.admin_host
            && normalize_hostname(host).as_deref() != Some(host.as_str())
        {
            return Err(Error::Config(format!(
                "domains.admin_host: {host:?} is not a lowercase hostname"
            )));
        }
        if self.sites.keep_releases == 0 {
            return Err(Error::Config(
                "sites.keep_releases must be at least 1".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_local_dev_config() {
        let config = Config::from_toml("").unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.domains.admin_host(), "localhost");
        let name = SiteName::parse("hikes").unwrap();
        assert_eq!(config.domains.site_hostname(&name), "hikes.localhost");
        assert_eq!(
            config.domains.public_url("hikes.localhost"),
            "http://hikes.localhost"
        );
        let dev = Config::from_toml("[domains]\npublic_port = 8080").unwrap();
        assert_eq!(
            dev.domains.public_url("hikes.localhost"),
            "http://hikes.localhost:8080"
        );
    }

    #[test]
    fn parses_a_proxy_mode_config() {
        let config = Config::from_toml(
            r#"
            [server]
            listen = "127.0.0.1:18080"
            trusted_proxies = ["127.0.0.1/32", "::1/128"]

            [domains]
            zones = ["dev.example.com"]
            admin_host = "sitewright-dev.example.com"
            public_scheme = "https"
            public_port = 443
            "#,
        )
        .unwrap();
        assert_eq!(config.server.mode, ServeMode::Proxy);
        assert!(config.server.is_trusted_proxy("127.0.0.1".parse().unwrap()));
        assert!(
            config
                .server
                .is_trusted_proxy("::ffff:127.0.0.1".parse().unwrap())
        );
        assert!(!config.server.is_trusted_proxy("10.0.0.1".parse().unwrap()));
        assert_eq!(config.domains.admin_host(), "sitewright-dev.example.com");
        assert_eq!(
            config.domains.public_url("a.dev.example.com"),
            "https://a.dev.example.com"
        );
        assert!(config.domains.in_zone("a.dev.example.com"));
        assert!(!config.domains.in_zone("dev.example.com"));
        assert!(!config.domains.in_zone("xdev.example.com"));
    }

    #[test]
    fn default_admin_host_uses_first_zone() {
        let config = Config::from_toml("[domains]\nzones = [\"example.com\"]").unwrap();
        assert_eq!(config.domains.admin_host(), "admin.example.com");
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(Config::from_toml("nope = 1").is_err());
        assert!(Config::from_toml("[domains]\nzones = []").is_err());
        assert!(Config::from_toml("[domains]\nzones = [\"Example.com\"]").is_err());
        assert!(Config::from_toml("[server]\nmode = \"magic\"").is_err());
        assert!(Config::from_toml("[sites]\nkeep_releases = 0").is_err());
    }

    #[test]
    fn load_prefers_flags_over_file_values() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("custom.toml");
        std::fs::write(&file, "data_dir = \"/somewhere/else\"\n").unwrap();

        let loaded = Config::load(Some(&file), None).unwrap();
        assert_eq!(loaded.data_dir.root(), Path::new("/somewhere/else"));

        let loaded = Config::load(Some(&file), Some(dir.path())).unwrap();
        assert_eq!(loaded.data_dir.root(), dir.path());

        assert!(Config::load(Some(&dir.path().join("missing.toml")), None).is_err());
    }
}
