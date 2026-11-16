//! AI model access for site backends.
//!
//! A backend never holds a provider key. `llm.chat` in a backend is a message to the
//! daemon, which makes the call with its own credentials and applies, per site:
//!
//! - an on/off switch (off until the owner turns it on),
//! - a list of the named models the site may use,
//! - a requests-per-minute limit and a requests-per-month cap,
//! - a record of every call, with tokens and cost.
//!
//! Visitors can trigger these calls as often as they can reach the site's API, so the
//! caps are what bounds the bill.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sw_llm::{ChatRequest, Event, LlmProvider, Message, Usage};

/// Longest one model call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest total size of the system prompt and messages, in bytes.
pub const MAX_INPUT_BYTES: usize = 200 * 1024;
const MAX_MESSAGES: usize = 100;

/// A model the owner configured, as backends and the agent may know it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    /// The name to use in `llm.chat({ model })`.
    pub name: String,
    /// The kind of provider behind it (`openai_compat`, `anthropic`, `gemini`).
    pub kind: String,
    /// The model or deployment the name currently points at.
    pub model: String,
    pub is_default: bool,
}

/// Where the gateway gets models from. Implemented by the daemon over its provider
/// configuration.
pub trait ModelSource: Send + Sync {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, String>>;
    fn provider<'a>(&'a self, name: &'a str)
    -> BoxFuture<'a, Result<Arc<dyn LlmProvider>, String>>;
    /// US dollars per million input and output tokens for a model, if configured.
    fn price(&self, model: &str) -> Option<(f64, f64)>;
}

/// A site's model settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LlmSettings {
    pub enabled: bool,
    /// Names the site may use. Empty means "the default model only".
    pub models: Vec<String>,
    pub monthly_requests: i64,
    pub per_minute: i64,
}

impl Default for LlmSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            models: Vec::new(),
            monthly_requests: 1000,
            per_minute: 20,
        }
    }
}

/// What a site has used this calendar month (UTC).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LlmUsage {
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// `None` when none of the calls had a price.
    pub cost_usd: Option<f64>,
}

fn month_start_ms(now_ms: i64) -> i64 {
    // Civil-from-days, to find the first day of the current UTC month.
    let days = now_ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day_of_month = doy - (153 * mp + 2) / 5; // zero-based
    (days - day_of_month) * 86_400_000
}

#[derive(Debug)]
pub struct LlmGateway {
    pool: SqlitePool,
    models: OnceLock<Arc<dyn ModelSource>>,
    /// Recent call times per site, for the per-minute limit.
    recent: Mutex<HashMap<i64, VecDeque<Instant>>>,
}

impl std::fmt::Debug for dyn ModelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ModelSource")
    }
}

impl LlmGateway {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            models: OnceLock::new(),
            recent: Mutex::new(HashMap::new()),
        }
    }

    /// Connect the gateway to the daemon's models. Can be set once.
    pub fn set_models(&self, models: Arc<dyn ModelSource>) {
        let _ = self.models.set(models);
    }

    fn models(&self) -> Result<&Arc<dyn ModelSource>, String> {
        self.models
            .get()
            .ok_or_else(|| "AI models are not available on this server".to_owned())
    }

    /// Every configured model.
    pub async fn available(&self) -> Result<Vec<ModelInfo>, String> {
        self.models()?.list().await
    }

    pub async fn settings(&self, site_id: i64) -> Result<LlmSettings, String> {
        let row = sqlx::query!(
            "SELECT enabled, models, monthly_requests, per_minute FROM site_llm WHERE site_id = ?1",
            site_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(match row {
            Some(r) => LlmSettings {
                enabled: r.enabled != 0,
                models: serde_json::from_str(&r.models).unwrap_or_default(),
                monthly_requests: r.monthly_requests,
                per_minute: r.per_minute,
            },
            None => LlmSettings::default(),
        })
    }

    /// Store a site's settings (the owner's decision). Unknown model names are refused.
    pub async fn set_settings(&self, site_id: i64, settings: &LlmSettings) -> Result<(), String> {
        if !(1..=1_000_000).contains(&settings.monthly_requests) {
            return Err("the monthly cap must be between 1 and 1,000,000 requests".into());
        }
        if !(1..=600).contains(&settings.per_minute) {
            return Err("the per-minute limit must be between 1 and 600".into());
        }
        if !settings.models.is_empty() {
            let known = self.available().await?;
            for name in &settings.models {
                if !known.iter().any(|m| &m.name == name) {
                    return Err(format!("there is no model named {name:?}"));
                }
            }
        }
        let models = serde_json::to_string(&settings.models).map_err(|e| e.to_string())?;
        let now = sw_core::time::now_ms();
        sqlx::query!(
            "INSERT INTO site_llm (site_id, enabled, models, monthly_requests, per_minute, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (site_id) DO UPDATE SET enabled = excluded.enabled, models = excluded.models,
                 monthly_requests = excluded.monthly_requests, per_minute = excluded.per_minute,
                 updated_at = excluded.updated_at",
            site_id,
            settings.enabled,
            models,
            settings.monthly_requests,
            settings.per_minute,
            now
        )
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// This month's usage for a site.
    pub async fn usage(&self, site_id: i64) -> Result<LlmUsage, String> {
        let since = month_start_ms(sw_core::time::now_ms());
        let row = sqlx::query!(
            r#"SELECT count(*) as "requests!: i64",
                      coalesce(sum(input_tokens), 0) as "input!: i64",
                      coalesce(sum(output_tokens), 0) as "output!: i64",
                      sum(cost_usd) as "cost: f64"
               FROM site_llm_usage WHERE site_id = ?1 AND ts >= ?2"#,
            site_id,
            since
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(LlmUsage {
            requests: row.requests,
            input_tokens: row.input,
            output_tokens: row.output,
            cost_usd: row.cost,
        })
    }

    /// The models a site may use, default first. Empty when access is off.
    pub async fn allowed(&self, site_id: i64) -> Result<Vec<ModelInfo>, String> {
        let settings = self.settings(site_id).await?;
        if !settings.enabled {
            return Ok(Vec::new());
        }
        let all = self.available().await?;
        Ok(if settings.models.is_empty() {
            all.into_iter().filter(|m| m.is_default).collect()
        } else {
            settings
                .models
                .iter()
                .filter_map(|name| all.iter().find(|m| &m.name == name).cloned())
                .collect()
        })
    }

    fn within_minute_limit(&self, site_id: i64, per_minute: i64) -> bool {
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let times = recent.entry(site_id).or_default();
        let window = Duration::from_secs(60);
        while times.front().is_some_and(|t| t.elapsed() >= window) {
            times.pop_front();
        }
        if times.len() as i64 >= per_minute {
            return false;
        }
        times.push_back(Instant::now());
        true
    }

    /// Serve `llm.chat` for a site.
    pub async fn chat(&self, site_id: i64, args: &Value) -> Result<Value, String> {
        let settings = self.settings(site_id).await?;
        if !settings.enabled {
            return Err(
                "AI models are not turned on for this site; the owner turns them on under \
                 \"Backend access\" on the site's page"
                    .into(),
            );
        }
        let allowed = self.allowed(site_id).await?;
        let chosen = match args.get("model").and_then(Value::as_str) {
            Some(name) => allowed.iter().find(|m| m.name == name).ok_or_else(|| {
                format!(
                    "this site may not use the model {name:?}; it may use: {}",
                    allowed
                        .iter()
                        .map(|m| m.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
            None => allowed
                .first()
                .ok_or("no AI model is configured on this server")?,
        };

        // Build the request before spending any of the site's allowance.
        let system = args
            .get("system")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let list = args
            .get("messages")
            .and_then(Value::as_array)
            .ok_or("llm.chat needs messages: [{ role, content }, ...]")?;
        if list.is_empty() || list.len() > MAX_MESSAGES {
            return Err(format!(
                "llm.chat takes between 1 and {MAX_MESSAGES} messages"
            ));
        }
        let mut size = system.len();
        let mut messages = Vec::with_capacity(list.len());
        for item in list {
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .ok_or("every message needs a string content")?
                .to_owned();
            size += content.len();
            messages.push(match item.get("role").and_then(Value::as_str) {
                Some("user") => Message::User { content },
                Some("assistant") => Message::Assistant {
                    text: content,
                    tool_calls: Vec::new(),
                    raw: None,
                },
                _ => return Err("a message's role is \"user\" or \"assistant\"".into()),
            });
        }
        if size > MAX_INPUT_BYTES {
            return Err(format!(
                "the prompt is larger than {} KB",
                MAX_INPUT_BYTES / 1024
            ));
        }
        if !matches!(messages.last(), Some(Message::User { .. })) {
            return Err("the last message must be from the user".into());
        }

        if self.usage(site_id).await?.requests >= settings.monthly_requests {
            return Err(format!(
                "this site has used its {} AI requests for this month",
                settings.monthly_requests
            ));
        }
        if !self.within_minute_limit(site_id, settings.per_minute) {
            return Err("too many AI requests right now; try again in a minute".into());
        }

        let provider = self.models()?.provider(&chosen.name).await?;
        let request = ChatRequest {
            system,
            messages,
            tools: Vec::new(),
        };
        let outcome = tokio::time::timeout(CALL_TIMEOUT, collect(provider.as_ref(), request)).await;
        let (result, usage) = match outcome {
            Ok(Ok((text, usage))) => (Ok(text), usage),
            Ok(Err((error, usage))) => (Err(error), usage),
            Err(_) => (
                Err(format!(
                    "the model did not answer within {} seconds",
                    CALL_TIMEOUT.as_secs()
                )),
                Usage::default(),
            ),
        };
        let cost = self
            .models()?
            .price(provider.model())
            .map(|(input, output)| {
                (usage.input_tokens as f64 * input + usage.output_tokens as f64 * output)
                    / 1_000_000.0
            });
        let (input, output) = (usage.input_tokens as i64, usage.output_tokens as i64);
        let (now, ok, model) = (sw_core::time::now_ms(), result.is_ok(), provider.model());
        let recorded = sqlx::query!(
            "INSERT INTO site_llm_usage (site_id, ts, model_name, model, input_tokens, output_tokens, cost_usd, ok)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            site_id,
            now,
            chosen.name,
            model,
            input,
            output,
            cost,
            ok
        )
        .execute(&self.pool)
        .await;
        if let Err(error) = recorded {
            // An unrecorded call would not count against the cap: refuse to hand it over.
            tracing::error!(%error, site_id, "could not record AI usage");
            return Err("the AI request could not be accounted for".into());
        }
        let text = result?;
        Ok(json!({
            "text": text,
            "model": chosen.name,
            "usage": { "inputTokens": usage.input_tokens, "outputTokens": usage.output_tokens },
        }))
    }
}

/// Run one call and gather its text. On failure, the usage seen so far comes back too.
async fn collect(
    provider: &dyn LlmProvider,
    request: ChatRequest,
) -> Result<(String, Usage), (String, Usage)> {
    let mut usage = Usage::default();
    let mut stream = provider
        .stream(request)
        .await
        .map_err(|e| (format!("the model call failed: {e}"), usage))?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::TextDelta(delta)) => text.push_str(&delta),
            Ok(Event::Usage(u)) => usage = u,
            Ok(Event::Stop(_)) => break,
            Ok(_) => {}
            Err(e) => return Err((format!("the model call failed: {e}"), usage)),
        }
    }
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::month_start_ms;

    #[test]
    fn months_start_on_the_first_at_midnight_utc() {
        // 2026-11-16T12:34:56Z -> 2026-11-01T00:00:00Z
        assert_eq!(month_start_ms(1_794_832_496_000), 1_793_491_200_000);
        // 2024-02-29T23:59:59Z (leap day) -> 2024-02-01
        assert_eq!(month_start_ms(1_709_251_199_000), 1_706_745_600_000);
        // 2025-01-01T00:00:00Z is its own month start.
        assert_eq!(month_start_ms(1_735_689_600_000), 1_735_689_600_000);
        // 1970-01-15 -> the epoch.
        assert_eq!(month_start_ms(14 * 86_400_000 + 5), 0);
    }
}
