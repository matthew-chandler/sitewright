//! Shared helpers: boot a daemon in-process on a temporary data directory.
#![allow(dead_code, clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use sitewright::client::AdminClient;
use sitewright::daemon::{self, Daemon};
use sw_core::Config;
use sw_core::DataDir;
use sw_core::config::Loaded;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

pub const ZONE: &str = "sites.test";
pub const ADMIN: &str = "admin.sites.test";
/// The admin origin browsers would use; passkeys are bound to it.
pub const ORIGIN: &str = "https://admin.sites.test";

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
    boot_with_listen(extra, "proxy", "127.0.0.1:0").await
}

/// Boot in the given mode, with the plain-HTTP listener on `listen`.
pub async fn boot_with_listen(extra: &str, mode: &str, listen: &str) -> TestDaemon {
    sw_core::crypto::install_default_provider();
    sitewright::daemon::set_worker_program(env!("CARGO_BIN_EXE_sitewright").into());
    let dir = tempfile::tempdir().unwrap();
    let toml = format!(
        "[server]\nmode = \"{mode}\"\ntrusted_proxies = [\"127.0.0.1/32\"]\n[domains]\nzones = [\"{ZONE}\"]\npublic_scheme = \"http\"\nadmin_url = \"{ORIGIN}\"\n{extra}"
    );
    let loaded = Loaded {
        config: Config::from_toml(&toml).unwrap(),
        data_dir: DataDir::new(dir.path().join("data")),
        source: None,
    };
    let daemon = daemon::start(loaded, Some(listen.parse().unwrap()))
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

// ---------------------------------------------------------------- a scripted model

pub const PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Hikes</title></head>\
    <body><h1>Washington trails</h1></body></html>";

pub fn sse(chunks: Vec<Value>) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

pub fn tool_turn(name: &str, arguments: Value) -> ResponseTemplate {
    sse(vec![
        json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"tool_calls": [
            {"index": 0, "id": format!("call_{name}"), "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}
        ]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 1000, "completion_tokens": 100}}),
    ])
}

pub fn text_turn(text: &str) -> ResponseTemplate {
    sse(vec![
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 1000, "completion_tokens": 100}}),
    ])
}

/// A mock model that answers each successive call with the next scripted turn.
pub async fn mock_model(turns: Vec<ResponseTemplate>) -> MockServer {
    let server = MockServer::start().await;
    let next = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("api-key", "test-key"))
        .respond_with(move |_: &Request| {
            let i = next.fetch_add(1, Ordering::SeqCst);
            turns
                .get(i)
                .cloned()
                .unwrap_or_else(|| text_turn("(script exhausted)"))
        })
        .mount(&server)
        .await;
    server
}

pub fn llm_config(server: &MockServer) -> String {
    format!(
        "[llm]\ndefault = \"mock\"\n[[llm.providers]]\nid = \"mock\"\nkind = \"openai_compat\"\nbase_url = \"{}/v1\"\n\
         model = \"mock-mini\"\nauth = \"api_key_header\"\nkey_ref = \"mock\"\nmax_retries = 0\n\
         [[llm.prices]]\nmodel = \"mock-mini\"\ninput_per_mtok = 0.25\noutput_per_mtok = 2.0\n",
        server.uri()
    )
}

pub fn write_secrets(t: &TestDaemon) {
    let mut secrets = sw_core::Secrets::default();
    secrets.keys.insert("mock".into(), "test-key".into());
    secrets.save(&t.daemon.data.secrets_file()).unwrap();
}
