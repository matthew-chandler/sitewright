//! Shared helpers: boot a daemon in-process on a temporary data directory.
#![allow(dead_code, clippy::unwrap_used)]

use std::path::Path;

use sitewright::client::AdminClient;
use sitewright::daemon::{self, Daemon};
use sw_core::Config;
use sw_core::DataDir;
use sw_core::config::Loaded;

pub const ZONE: &str = "sites.test";
pub const ADMIN: &str = "admin.sites.test";

pub struct TestDaemon {
    pub dir: tempfile::TempDir,
    pub daemon: Daemon,
    pub http: reqwest::Client,
}

pub async fn boot() -> TestDaemon {
    boot_with("").await
}

/// Boot with extra TOML appended to the test config.
pub async fn boot_with(extra: &str) -> TestDaemon {
    sw_core::crypto::install_default_provider();
    let dir = tempfile::tempdir().unwrap();
    let toml = format!(
        "[server]\ntrusted_proxies = [\"127.0.0.1/32\"]\n[domains]\nzones = [\"{ZONE}\"]\npublic_scheme = \"http\"\n{extra}"
    );
    let loaded = Loaded {
        config: Config::from_toml(&toml).unwrap(),
        data_dir: DataDir::new(dir.path().join("data")),
        source: None,
    };
    let daemon = daemon::start(loaded, Some("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    TestDaemon { dir, daemon, http }
}

impl TestDaemon {
    pub fn admin(&self) -> AdminClient {
        AdminClient::new(&self.daemon.data).unwrap()
    }

    /// GET `path` on the public listener with the given `Host`.
    pub async fn get(&self, host: &str, path: &str) -> reqwest::Response {
        self.http
            .get(format!("http://{}{path}", self.daemon.addr))
            .header("host", host)
            .send()
            .await
            .unwrap()
    }

    /// Write files into a fresh directory under the test's temp dir and return its path.
    pub fn site_dir(&self, name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = self.dir.path().join(name);
        let _ = std::fs::remove_dir_all(&root);
        for (path, content) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
        root
    }

    /// Run a CLI command against this daemon, as `sitewright --data-dir <dir> <args>`.
    pub async fn cli(&self, args: &[&str]) -> anyhow::Result<()> {
        use clap::Parser;
        let data = self.daemon.data.root().to_string_lossy().into_owned();
        let mut argv = vec!["sitewright", "--data-dir", &data];
        argv.extend_from_slice(args);
        sitewright::cli::run(sitewright::cli::Cli::try_parse_from(argv)?).await
    }
}

pub fn host(site: &str) -> String {
    format!("{site}.{ZONE}")
}

pub fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}
