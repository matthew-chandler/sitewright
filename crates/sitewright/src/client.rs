//! Client for the daemon's admin socket, used by every CLI command except `serve`.

use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::de::DeserializeOwned;
use sw_core::DataDir;

/// Talks JSON over the Unix socket in the data directory.
#[derive(Debug)]
pub struct AdminClient {
    http: reqwest::Client,
    socket: PathBuf,
}

impl AdminClient {
    pub fn new(data: &DataDir) -> anyhow::Result<Self> {
        sw_core::crypto::install_default_provider();
        let socket = data.admin_socket();
        let http = reqwest::Client::builder()
            .unix_socket(socket.clone())
            .no_proxy()
            .build()
            .context("cannot build the admin client")?;
        Ok(Self { http, socket })
    }

    fn url(path: &str) -> String {
        // The host is ignored: the connection always goes to the socket.
        format!("http://sitewright{path}")
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.finish(self.http.get(Self::url(path))).await
    }

    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<T> {
        self.finish(self.http.post(Self::url(path)).json(body))
            .await
    }

    pub async fn delete<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<T> {
        self.finish(self.http.delete(Self::url(path)).json(body))
            .await
    }

    pub async fn put_bytes<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        body: Vec<u8>,
    ) -> anyhow::Result<T> {
        self.finish(self.http.put(Self::url(path)).query(query).body(body))
            .await
    }

    /// GET a binary body (a file download).
    pub async fn get_bytes(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        let response = self
            .http
            .get(Self::url(path))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .context("cannot read the daemon's response")?;
        if !status.is_success() {
            let message = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
                .unwrap_or_else(|| format!("the daemon answered {status}"));
            bail!("{message}");
        }
        Ok(body.to_vec())
    }

    /// Open a streaming GET (server-sent events). The caller reads the body.
    pub async fn stream(&self, path: &str) -> anyhow::Result<reqwest::Response> {
        let response = self
            .http
            .get(Self::url(path))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if !response.status().is_success() {
            bail!("the daemon answered {} for {path}", response.status());
        }
        Ok(response)
    }

    fn unreachable(&self, e: reqwest::Error) -> anyhow::Error {
        if e.is_connect() {
            anyhow::anyhow!(
                "cannot reach the daemon at {}. Is `sitewright serve` running, and are you the user it runs as? If its data directory is elsewhere, pass --data-dir <dir> or set SITEWRIGHT_DATA_DIR.",
                self.socket.display()
            )
        } else {
            anyhow::Error::new(e).context("request to the daemon failed")
        }
    }

    async fn finish<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> anyhow::Result<T> {
        let response = request.send().await.map_err(|e| self.unreachable(e))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .context("cannot read the daemon's response")?;
        if !status.is_success() {
            let message = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).trim().to_owned());
            bail!("{message}");
        }
        serde_json::from_slice(&body).context("unexpected response from the daemon")
    }
}
