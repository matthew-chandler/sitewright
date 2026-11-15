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
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub metrics: MetricsConfig,
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
    /// Full origin of the admin UI, for example `https://admin.example.com`. Needed when
    /// it differs from `public_scheme` + `admin_host` (+ `public_port`). Passkeys are
    /// bound to this origin.
    pub admin_url: Option<String>,
    /// Scheme visitors use to reach sites: `http` or `https`.
    pub public_scheme: Scheme,
    /// Public port, when it is not the scheme's default (for example 8080 in local dev).
    pub public_port: Option<u16>,
}

impl Default for DomainsConfig {
    fn default() -> Self {
        Self {
            zones: vec!["localhost".to_owned()],
            admin_host: None,
            admin_url: None,
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

    /// The admin UI's origin, without a trailing slash.
    pub fn admin_origin(&self) -> String {
        match &self.admin_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => self.public_url(&self.admin_host()),
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

/// LLM providers and prices.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// Id of the provider used when a run does not name one. Default: the first provider.
    pub default: Option<String>,
    pub providers: Vec<ProviderConfig>,
    /// Token prices, used to turn usage into cost. Sitewright ships no prices of its own.
    pub prices: Vec<ModelPrice>,
}

impl LlmConfig {
    /// The provider with this id, or the default provider when `id` is `None`.
    pub fn provider(&self, id: Option<&str>) -> Option<&ProviderConfig> {
        match id.or(self.default.as_deref()) {
            Some(id) => self.providers.iter().find(|p| p.id == id),
            None => self.providers.first(),
        }
    }

    /// The price entry for a model name, if the user configured one.
    pub fn price(&self, model: &str) -> Option<&ModelPrice> {
        self.prices.iter().find(|p| p.model == model)
    }
}

/// One configured LLM endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Name used to refer to this provider in the UI, CLI and `llm.default`.
    pub id: String,
    pub kind: ProviderKind,
    /// API base URL. Required for `openai_compat`; optional for the others.
    pub base_url: Option<String>,
    /// Model name. For Azure OpenAI this is the deployment name.
    pub model: String,
    #[serde(default)]
    pub auth: AuthStyle,
    /// Name of the API key in `secrets.toml`. Omit for endpoints without a key (Ollama).
    pub key_ref: Option<String>,
    /// Seconds to wait for the response to start, and between streamed chunks.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Retries after a rate-limit, server or network error.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Upper bound on tokens generated per model call. Omitted from requests when unset.
    pub max_output_tokens: Option<u32>,
    /// Extra fields merged into every request body, for provider-specific options such
    /// as `reasoning_effort`.
    #[serde(default)]
    pub extra_body: toml::Table,
}

fn default_timeout_secs() -> u64 {
    300
}
fn default_max_retries() -> u32 {
    3
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Chat Completions: OpenAI, Azure OpenAI, OpenRouter, Groq, Ollama and others.
    OpenaiCompat,
    /// Anthropic Messages API.
    Anthropic,
    /// Google Gemini generateContent.
    Gemini,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::OpenaiCompat => "openai_compat",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
        }
    }
}

/// How the API key is sent (only `openai_compat` has a choice).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`
    #[default]
    Bearer,
    /// `api-key: <key>` (Azure OpenAI)
    ApiKeyHeader,
}

/// Price of one model, in US dollars per million tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl ModelPrice {
    /// Cost in US dollars of the given token counts.
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 * self.input_per_mtok + output_tokens as f64 * self.output_per_mtok)
            / 1_000_000.0
    }
}

/// Limits on agent runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// Most model calls in one run.
    pub max_turns: u32,
    /// Most tokens (input plus output, summed over all calls) in one run.
    pub max_run_tokens: u64,
    /// Most US dollars spent across all runs per UTC day. Needs `llm.prices` for the
    /// model in use; without a price, cost is unknown and this cap cannot apply.
    pub daily_spend_cap_usd: f64,
    /// Most runs in progress at once (always at most one per site).
    pub max_concurrent_runs: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 40,
            max_run_tokens: 1_000_000,
            daily_spend_cap_usd: 5.0,
            max_concurrent_runs: 2,
        }
    }
}

/// Metrics collection and uptime checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Seconds between uptime checks of each site. 0 turns the checks off.
    pub uptime_interval_secs: u64,
    /// A check slower than this many milliseconds marks the site as degraded.
    pub slow_ms: u64,
    /// Check sites through the daemon's own listener instead of public DNS. For local
    /// development, where `*.localhost` names do not resolve for the daemon.
    pub uptime_resolve_local: bool,
    /// Days raw requests, events and checks are kept. Daily rollups are kept forever.
    pub retention_days: u32,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            uptime_interval_secs: 60,
            slow_ms: 2000,
            uptime_resolve_local: false,
            retention_days: 30,
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
        let mut seen = std::collections::HashSet::new();
        for provider in &self.llm.providers {
            if provider.id.is_empty() || !seen.insert(provider.id.as_str()) {
                return Err(Error::Config(format!(
                    "llm.providers: id {:?} is empty or used twice",
                    provider.id
                )));
            }
            if provider.kind == ProviderKind::OpenaiCompat && provider.base_url.is_none() {
                return Err(Error::Config(format!(
                    "llm.providers: {:?} needs a base_url",
                    provider.id
                )));
            }
        }
        if let Some(default) = &self.llm.default
            && !self.llm.providers.iter().any(|p| &p.id == default)
        {
            return Err(Error::Config(format!(
                "llm.default: no provider with id {default:?}"
            )));
        }
        if self.agent.max_turns == 0 || self.agent.max_concurrent_runs == 0 {
            return Err(Error::Config(
                "agent.max_turns and agent.max_concurrent_runs must be at least 1".into(),
            ));
        }
        if let Some(url) = &self.domains.admin_url {
            let host = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
                .map(|rest| rest.trim_end_matches('/'))
                .and_then(normalize_hostname);
            if host.as_deref() != Some(self.domains.admin_host().as_str()) {
                return Err(Error::Config(format!(
                    "domains.admin_url: {url:?} must be http(s)://{} with an optional port",
                    self.domains.admin_host()
                )));
            }
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
    fn parses_llm_providers_and_prices() {
        let config = Config::from_toml(
            r#"
            [llm]
            default = "azure"

            [[llm.providers]]
            id = "azure"
            kind = "openai_compat"
            base_url = "https://example.openai.azure.com/openai/v1/"
            model = "gpt-5-mini"
            auth = "api_key_header"
            key_ref = "azure"
            extra_body = { reasoning_effort = "low" }

            [[llm.providers]]
            id = "claude"
            kind = "anthropic"
            model = "claude-x"
            key_ref = "anthropic"

            [[llm.prices]]
            model = "gpt-5-mini"
            input_per_mtok = 0.25
            output_per_mtok = 2.0

            [agent]
            daily_spend_cap_usd = 5.0
            "#,
        )
        .unwrap();
        let azure = config.llm.provider(None).unwrap();
        assert_eq!(azure.id, "azure");
        assert_eq!(azure.auth, AuthStyle::ApiKeyHeader);
        assert_eq!(azure.timeout_secs, 300);
        assert_eq!(
            config.llm.provider(Some("claude")).unwrap().kind,
            ProviderKind::Anthropic
        );
        assert!(config.llm.provider(Some("nope")).is_none());
        let price = config.llm.price("gpt-5-mini").unwrap();
        assert!((price.cost(1_000_000, 500_000) - 1.25).abs() < 1e-9);
        assert!(config.llm.price("other").is_none());
        assert_eq!(config.agent.max_turns, 40);

        assert!(Config::from_toml("[llm]\ndefault = \"x\"").is_err());
        assert!(
            Config::from_toml(
                "[[llm.providers]]\nid = \"a\"\nkind = \"openai_compat\"\nmodel = \"m\""
            )
            .is_err()
        );
    }

    #[test]
    fn admin_origin_defaults_to_the_public_url_of_the_admin_host() {
        let config = Config::from_toml("[domains]\npublic_port = 8080").unwrap();
        assert_eq!(config.domains.admin_origin(), "http://localhost:8080");
        let config = Config::from_toml(
            "[domains]\nzones = [\"dev.example.com\"]\nadmin_host = \"sw.example.com\"\nadmin_url = \"https://sw.example.com/\"",
        )
        .unwrap();
        assert_eq!(config.domains.admin_origin(), "https://sw.example.com");
        assert!(
            Config::from_toml(
                "[domains]\nzones = [\"example.com\"]\nadmin_url = \"https://other.example.com\""
            )
            .is_err()
        );
        assert!(
            Config::from_toml(
                "[domains]\nzones = [\"example.com\"]\nadmin_url = \"ftp://admin.example.com\""
            )
            .is_err()
        );
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
