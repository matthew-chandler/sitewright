//! LLM providers from two places: the configuration file (read-only here) and the ones
//! added in the UI (stored in the database, with their keys in `secrets.toml`).

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use sqlx::SqlitePool;
use sw_agent::ProviderSource;
use sw_core::config::{ProviderConfig, ProviderKind};
use sw_core::time::now_ms;
use sw_core::{Config, Secrets};
use sw_llm::LlmProvider;

/// A provider as shown in settings.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderView {
    pub config: ProviderConfig,
    /// True for providers from `config.toml`, which the UI cannot change.
    pub from_file: bool,
    /// Whether a key is stored for it. The key itself is never returned.
    pub has_key: bool,
    pub is_default: bool,
}

/// Looks up, lists and edits providers. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Providers {
    config: Arc<Config>,
    db: SqlitePool,
    secrets_file: PathBuf,
    /// Serializes writes to `secrets.toml`.
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Providers {
    pub fn new(config: Arc<Config>, db: SqlitePool, secrets_file: PathBuf) -> Self {
        Self {
            config,
            db,
            secrets_file,
            write_lock: Arc::default(),
        }
    }

    fn secrets(&self) -> Result<Secrets, String> {
        Secrets::load(&self.secrets_file).map_err(|e| e.to_string())
    }

    async fn stored(&self) -> Result<Vec<ProviderConfig>, String> {
        let rows = sqlx::query!("SELECT id, config FROM providers ORDER BY created_at")
            .fetch_all(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .filter_map(|r| serde_json::from_str(&r.config).ok())
            .collect())
    }

    /// Every provider: the file's first, then the UI's. A UI provider with the same id as
    /// a file provider is ignored, so the file stays authoritative.
    pub async fn list(&self) -> Result<Vec<ProviderView>, String> {
        let secrets = self.secrets()?;
        let has_key = |p: &ProviderConfig| {
            p.key_ref
                .as_deref()
                .is_some_and(|k| secrets.key(k).is_some())
        };
        let mut views: Vec<ProviderView> = self
            .config
            .llm
            .providers
            .iter()
            .map(|p| ProviderView {
                has_key: has_key(p),
                config: p.clone(),
                from_file: true,
                is_default: false,
            })
            .collect();
        for p in self.stored().await? {
            if !views.iter().any(|v| v.config.id == p.id) {
                views.push(ProviderView {
                    has_key: has_key(&p),
                    config: p,
                    from_file: false,
                    is_default: false,
                });
            }
        }
        let default = self
            .config
            .llm
            .default
            .clone()
            .or_else(|| views.first().map(|v| v.config.id.clone()));
        for view in &mut views {
            view.is_default = Some(&view.config.id) == default.as_ref();
        }
        Ok(views)
    }

    /// Add or replace a UI-managed provider. With `key`, the key is written to
    /// `secrets.toml` under the provider's id.
    pub async fn save(
        &self,
        mut provider: ProviderConfig,
        key: Option<&str>,
    ) -> Result<(), String> {
        let id_ok = !provider.id.is_empty()
            && provider.id.len() <= 40
            && provider
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !id_ok {
            return Err(
                "the provider id may use lowercase letters, digits, '-' and '_' (at most 40)"
                    .into(),
            );
        }
        if self
            .config
            .llm
            .providers
            .iter()
            .any(|p| p.id == provider.id)
        {
            return Err(format!(
                "{:?} is defined in config.toml; edit it there",
                provider.id
            ));
        }
        if provider.model.trim().is_empty() {
            return Err("the model name is required".into());
        }
        match (&provider.base_url, provider.kind) {
            (Some(url), _) if !(url.starts_with("https://") || url.starts_with("http://")) => {
                return Err("the base URL must start with https:// or http://".into());
            }
            (None, ProviderKind::OpenaiCompat) => {
                return Err("an OpenAI-compatible provider needs a base URL".into());
            }
            _ => {}
        }

        let _guard = self.write_lock.lock().await;
        let key = key.map(str::trim).filter(|k| !k.is_empty());
        if let Some(key) = key {
            let mut secrets = self.secrets()?;
            secrets.keys.insert(provider.id.clone(), key.to_owned());
            secrets
                .save(&self.secrets_file)
                .map_err(|e| e.to_string())?;
            provider.key_ref = Some(provider.id.clone());
        } else if provider.key_ref.is_none() {
            // Keep a key saved earlier for this id.
            let existing = self
                .stored()
                .await?
                .into_iter()
                .find(|p| p.id == provider.id);
            provider.key_ref = existing.and_then(|p| p.key_ref);
        }
        let json = serde_json::to_string(&provider).map_err(|e| e.to_string())?;
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO providers (id, config, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT (id) DO UPDATE SET config = excluded.config, updated_at = excluded.updated_at",
            provider.id,
            json,
            now
        )
        .execute(&self.db)
        .await
        .map_err(|e| e.to_string())?;
        sw_core::db::audit(&self.db, "admin", "provider.save", Some(&provider.id), None).await;
        Ok(())
    }

    /// Remove a UI-managed provider and its stored key.
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        let _guard = self.write_lock.lock().await;
        let removed = sqlx::query!("DELETE FROM providers WHERE id = ?1", id)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        if removed.rows_affected() == 0 {
            return Err(format!("{id:?} is not a provider added in the UI"));
        }
        let mut secrets = self.secrets()?;
        if secrets.keys.remove(id).is_some() {
            secrets
                .save(&self.secrets_file)
                .map_err(|e| e.to_string())?;
        }
        sw_core::db::audit(&self.db, "admin", "provider.delete", Some(id), None).await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProviderSource for Providers {
    async fn provider(&self, id: Option<&str>) -> Result<Arc<dyn LlmProvider>, String> {
        let all = self.list().await?;
        let chosen = match id {
            Some(id) => all.iter().find(|v| v.config.id == id),
            None => all.iter().find(|v| v.is_default).or(all.first()),
        };
        let view = chosen.ok_or_else(|| match id {
            Some(id) => format!("there is no LLM provider with id {id:?}"),
            None => "no LLM provider is configured; add one in Settings or under [[llm.providers]] in config.toml".to_owned(),
        })?;
        sw_llm::build_provider(&view.config, &self.secrets()?).map_err(|e| e.to_string())
    }
}

/// Backends reach models through the same provider list as the agent, by provider id.
impl sw_backend::ModelSource for Providers {
    fn list(
        &self,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<sw_backend::ModelInfo>, String>> {
        Box::pin(async move {
            Ok(Providers::list(self)
                .await?
                .into_iter()
                .map(|view| sw_backend::ModelInfo {
                    name: view.config.id,
                    kind: view.config.kind.as_str().to_owned(),
                    model: view.config.model,
                    is_default: view.is_default,
                })
                .collect())
        })
    }

    fn provider<'a>(
        &'a self,
        name: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<Arc<dyn LlmProvider>, String>> {
        Box::pin(ProviderSource::provider(self, Some(name)))
    }

    fn price(&self, model: &str) -> Option<(f64, f64)> {
        self.config
            .llm
            .price(model)
            .map(|p| (p.input_per_mtok, p.output_per_mtok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sw_core::config::AuthStyle;

    fn provider(id: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            kind: ProviderKind::OpenaiCompat,
            base_url: Some("https://api.example.com/v1".into()),
            model: "m".into(),
            auth: AuthStyle::Bearer,
            key_ref: None,
            timeout_secs: 300,
            max_retries: 3,
            max_output_tokens: None,
            extra_body: toml::Table::new(),
        }
    }

    async fn setup(config_toml: &str) -> (tempfile::TempDir, Providers) {
        let dir = tempfile::tempdir().unwrap();
        let db = sw_core::db::open_in_memory().await.unwrap();
        let config = Arc::new(Config::from_toml(config_toml).unwrap());
        let providers = Providers::new(config, db, dir.path().join("secrets.toml"));
        (dir, providers)
    }

    #[tokio::test]
    async fn ui_providers_are_saved_with_their_key_in_secrets() {
        let (dir, providers) = setup("").await;
        assert!(
            providers
                .provider(None)
                .await
                .unwrap_err()
                .contains("no LLM provider is configured")
        );

        providers
            .save(provider("mine"), Some(" sk-secret "))
            .await
            .unwrap();
        let listed = providers.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].has_key && listed[0].is_default && !listed[0].from_file);
        assert_eq!(listed[0].config.key_ref.as_deref(), Some("mine"));
        assert!(
            !serde_json::to_string(&listed)
                .unwrap()
                .contains("sk-secret"),
            "keys never leave the server"
        );
        assert_eq!(
            Secrets::load(&dir.path().join("secrets.toml"))
                .unwrap()
                .key("mine"),
            Some("sk-secret")
        );
        assert_eq!(providers.provider(None).await.unwrap().id(), "mine");

        // Saving again without a key keeps the stored key.
        let mut edited = provider("mine");
        edited.model = "m2".into();
        providers.save(edited, None).await.unwrap();
        let listed = providers.list().await.unwrap();
        assert!(listed[0].has_key);
        assert_eq!(listed[0].config.model, "m2");

        providers.delete("mine").await.unwrap();
        assert!(providers.list().await.unwrap().is_empty());
        assert!(
            Secrets::load(&dir.path().join("secrets.toml"))
                .unwrap()
                .key("mine")
                .is_none()
        );
        assert!(providers.delete("mine").await.is_err());
    }

    #[tokio::test]
    async fn file_providers_win_and_cannot_be_edited_here() {
        let (_dir, providers) = setup(
            "[llm]\ndefault = \"file\"\n[[llm.providers]]\nid = \"file\"\nkind = \"openai_compat\"\nbase_url = \"http://127.0.0.1:1/v1\"\nmodel = \"m\"\n",
        )
        .await;
        assert!(
            providers
                .save(provider("file"), None)
                .await
                .unwrap_err()
                .contains("config.toml")
        );
        providers.save(provider("extra"), None).await.unwrap();
        let listed = providers.list().await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|v| v.config.id.as_str())
                .collect::<Vec<_>>(),
            ["file", "extra"]
        );
        assert!(listed[0].from_file && listed[0].is_default && !listed[1].is_default);
        assert_eq!(
            providers.provider(Some("extra")).await.unwrap().id(),
            "extra"
        );
        assert!(providers.provider(Some("nope")).await.is_err());

        for bad in ["", "Has Space", "UPPER"] {
            assert!(providers.save(provider(bad), None).await.is_err(), "{bad}");
        }
        let mut no_url = provider("x");
        no_url.base_url = None;
        assert!(providers.save(no_url, None).await.is_err());
        let mut bad_url = provider("x");
        bad_url.base_url = Some("ftp://x".into());
        assert!(providers.save(bad_url, None).await.is_err());
    }
}
