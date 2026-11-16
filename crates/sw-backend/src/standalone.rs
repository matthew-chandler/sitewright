//! What a site needs to run on its own, outside Sitewright.
//!
//! Inside Sitewright the daemon holds a site's settings: which secrets it has, which
//! outside hosts and AI models it may use. A containerized site has no daemon, so those
//! settings travel with it in a [`SiteManifest`], and everything secret comes from the
//! environment of the process that runs it:
//!
//! - a site secret `NAME` is the environment variable `NAME`;
//! - the API key of a model called `fast` is `LLM_FAST_API_KEY`, and `LLM_FAST_BASE_URL`
//!   and `LLM_FAST_MODEL` override where it points.
//!
//! The manifest contains no secret values.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sw_core::config::ProviderConfig;
use sw_core::secrets::Secrets;
use sw_llm::LlmProvider;

use crate::llm::{LlmSettings, ModelInfo, ModelSource};
use crate::sites::SiteBackends;

/// The manifest's file name, beside the site's files.
pub const MANIFEST_FILE: &str = "site.json";

/// A model the site may use. `provider.key_ref` is not used: the key comes from the
/// environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestModel {
    pub provider: ProviderConfig,
    /// US dollars per million input and output tokens, when the owner configured it.
    pub price: Option<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestLlm {
    pub monthly_requests: i64,
    pub per_minute: i64,
    /// The models the site may use; the first is its default.
    pub models: Vec<ManifestModel>,
}

/// A site's settings, as packaged with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiteManifest {
    pub name: String,
    pub title: String,
    /// The release (commit sha) the files are from.
    pub release: String,
    /// The Sitewright version that packaged the site.
    pub sitewright_version: String,
    /// Whether the site has a backend.
    pub backend: bool,
    /// Names of the secrets the backend may read from the environment.
    pub secrets: Vec<String>,
    /// Outside hosts the backend may send requests to.
    pub hosts: Vec<String>,
    /// `None` when the site may not use AI models.
    pub llm: Option<ManifestLlm>,
}

/// The environment variable prefix for a model: `LLM_<NAME>`, with the name uppercased
/// and anything that is not a letter or digit turned into `_`.
pub fn model_env_prefix(name: &str) -> String {
    let upper: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("LLM_{upper}")
}

/// Models configured by a manifest, with keys from the environment.
#[derive(Debug)]
pub struct EnvModels {
    models: Vec<ManifestModel>,
}

impl EnvModels {
    pub fn new(models: Vec<ManifestModel>) -> Self {
        Self { models }
    }
}

impl ModelSource for EnvModels {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, String>> {
        Box::pin(async move {
            Ok(self
                .models
                .iter()
                .enumerate()
                .map(|(i, m)| ModelInfo {
                    name: m.provider.id.clone(),
                    kind: m.provider.kind.as_str().to_owned(),
                    model: m.provider.model.clone(),
                    is_default: i == 0,
                })
                .collect())
        })
    }

    fn provider<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Arc<dyn LlmProvider>, String>> {
        Box::pin(async move {
            let entry = self
                .models
                .iter()
                .find(|m| m.provider.id == name)
                .ok_or_else(|| format!("there is no model named {name:?}"))?;
            let prefix = model_env_prefix(name);
            let env = |suffix: &str| {
                std::env::var(format!("{prefix}_{suffix}"))
                    .ok()
                    .filter(|v| !v.is_empty())
            };
            let mut config = entry.provider.clone();
            if let Some(url) = env("BASE_URL") {
                config.base_url = Some(url);
            }
            if let Some(model) = env("MODEL") {
                config.model = model;
            }
            let mut secrets = Secrets::default();
            config.key_ref = match env("API_KEY") {
                Some(key) => {
                    secrets.keys.insert("key".into(), key);
                    Some("key".into())
                }
                // No key is right for endpoints that need none (a local model server).
                None => None,
            };
            sw_llm::build_provider(&config, &secrets).map_err(|e| {
                format!("the model {name:?} is not set up ({e}); set {prefix}_API_KEY")
            })
        })
    }

    fn price(&self, model: &str) -> Option<(f64, f64)> {
        self.models
            .iter()
            .find(|m| m.provider.model == model)
            .and_then(|m| m.price)
    }
}

impl SiteBackends {
    /// Make a standalone site's stored settings match its manifest: the approved hosts
    /// and the AI model settings. The manifest is the only authority there.
    pub async fn apply_manifest(
        &self,
        site_id: i64,
        manifest: &SiteManifest,
    ) -> Result<(), String> {
        for entry in self.hosts(site_id).await? {
            self.remove_host(site_id, &entry.host).await?;
        }
        for host in &manifest.hosts {
            self.approve_host(site_id, host).await?;
        }
        let settings = match &manifest.llm {
            Some(llm) => {
                self.llm()
                    .set_models(Arc::new(EnvModels::new(llm.models.clone())));
                LlmSettings {
                    enabled: true,
                    models: llm.models.iter().map(|m| m.provider.id.clone()).collect(),
                    monthly_requests: llm.monthly_requests,
                    per_minute: llm.per_minute,
                }
            }
            None => LlmSettings::default(),
        };
        self.llm().set_settings(site_id, &settings).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_become_environment_prefixes() {
        assert_eq!(model_env_prefix("fast"), "LLM_FAST");
        assert_eq!(model_env_prefix("azure-gpt.5 mini"), "LLM_AZURE_GPT_5_MINI");
    }
}
