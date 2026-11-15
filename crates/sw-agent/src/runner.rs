//! The agent loop: call the model, run the tools it asks for, repeat until it is done.
//!
//! One [`Agent`] manages all runs. Each run is a background task bound to one site; it
//! persists its events, broadcasts them to live subscribers and stops cleanly when it
//! finishes, is cancelled or hits a limit.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde::Serialize;
use sqlx::SqlitePool;
use sw_core::time::{MS_PER_DAY, now_ms};
use sw_core::{Config, SiteName};
use sw_llm::{ChatRequest, Event, LlmProvider, Message, StopReason, ToolCall, Usage};
use sw_sites::{DomainChecker, SiteError, SiteStatus, SiteStore};
use tokio::sync::{Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

use crate::events::{RunEvent, shorten, shorten_value};
use crate::prompt::{ExistingSite, SiteContext, system_prompt};
use crate::skills::Skills;
use crate::tools::{self, Effect, ToolContext};

/// Longest accepted prompt, in characters.
pub const MAX_PROMPT_CHARS: usize = 20_000;

/// Where runs get their model from. The daemon implements this from the configuration;
/// tests plug in a scripted provider.
#[async_trait::async_trait]
pub trait ProviderSource: Send + Sync {
    /// The provider with this id, or the default one for `None`.
    async fn provider(&self, id: Option<&str>) -> Result<Arc<dyn LlmProvider>, String>;
}

/// A request to start a run.
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub site: String,
    pub prompt: String,
    /// Create the site in this run. The name must be free.
    pub new: bool,
    /// Provider id; `None` for the default.
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// Stopped by a turn, token or spend limit.
    Limit,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Limit => "limit",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "running" => RunStatus::Running,
            "succeeded" => RunStatus::Succeeded,
            "cancelled" => RunStatus::Cancelled,
            "limit" => RunStatus::Limit,
            _ => RunStatus::Failed,
        }
    }
}

/// A run record.
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: i64,
    pub site_name: String,
    pub prompt: String,
    pub status: RunStatus,
    pub provider: String,
    pub model: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// `None` when no price is configured for the model.
    pub cost_usd: Option<f64>,
    pub message: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("{0}")]
    Invalid(String),
    /// The site already has a run, or too many runs are in progress.
    #[error("{0}")]
    Busy(String),
    /// A spend limit prevents starting.
    #[error("{0}")]
    Budget(String),
    #[error("{0}")]
    Provider(String),
    #[error("run {0} does not exist")]
    NotFound(i64),
    #[error(transparent)]
    Site(#[from] SiteError),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Past and future events of a run.
#[derive(Debug)]
pub struct Subscription {
    /// Stored events so far, as `(sequence number, event)`.
    pub past: Vec<(i64, RunEvent)>,
    /// Live events, if the run is still going. Stored events carry their sequence number
    /// (skip those already in `past`); text deltas carry 0.
    pub live: Option<broadcast::Receiver<(i64, RunEvent)>>,
}

struct Active {
    events: broadcast::Sender<(i64, RunEvent)>,
    cancel: CancellationToken,
}

struct Inner {
    db: SqlitePool,
    store: SiteStore,
    domains: DomainChecker,
    skills: Skills,
    config: Arc<Config>,
    providers: Arc<dyn ProviderSource>,
    active: Mutex<HashMap<i64, Active>>,
    busy_sites: Mutex<HashSet<String>>,
    slots: Arc<Semaphore>,
}

/// Starts, tracks and cancels agent runs. Cheap to clone.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent").finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Agent {
    /// Create the run manager. Runs a previous process left unfinished are marked failed.
    pub async fn new(
        store: SiteStore,
        domains: DomainChecker,
        skills: Skills,
        config: Arc<Config>,
        providers: Arc<dyn ProviderSource>,
    ) -> Result<Self, AgentError> {
        let db = store.db().clone();
        let now = now_ms();
        sqlx::query!(
            "UPDATE agent_runs SET status = 'failed', message = 'interrupted: the daemon restarted during this run', finished_at = ?1
             WHERE status = 'running'",
            now
        )
        .execute(&db)
        .await?;
        let slots = Arc::new(Semaphore::new(config.agent.max_concurrent_runs as usize));
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                store,
                domains,
                skills,
                config,
                providers,
                active: Mutex::new(HashMap::new()),
                busy_sites: Mutex::new(HashSet::new()),
                slots,
            }),
        })
    }

    /// Start a run in the background and return its record.
    pub async fn start(&self, request: RunRequest) -> Result<Run, AgentError> {
        let inner = &self.inner;
        let prompt = request.prompt.trim().to_owned();
        if prompt.is_empty() {
            return Err(AgentError::Invalid("the prompt is empty".into()));
        }
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(AgentError::Invalid(format!(
                "the prompt is longer than {MAX_PROMPT_CHARS} characters"
            )));
        }
        let name =
            SiteName::parse(&request.site).map_err(|e| AgentError::Invalid(e.to_string()))?;
        let existing = inner.store.find(name.as_str()).await?;
        match (&existing, request.new) {
            (Some(site), true) => {
                return Err(AgentError::Invalid(match site.status {
                    SiteStatus::Active => format!(
                        "a site named {name} already exists; prompt it without --new to edit it"
                    ),
                    SiteStatus::Deleted => format!(
                        "a site named {name} was deleted recently; restore it or wait until it is purged"
                    ),
                }));
            }
            (None, true) if name.is_reserved(&inner.store.domains().admin_host()) => {
                return Err(AgentError::Invalid(format!("the name {name} is reserved")));
            }
            (None, false) => return Err(AgentError::Site(SiteError::NotFound(name.to_string()))),
            (Some(site), false) if site.status == SiteStatus::Deleted => {
                return Err(AgentError::Invalid(format!(
                    "site {name} was deleted; restore it first"
                )));
            }
            _ => {}
        }
        self.check_daily_budget().await?;
        let provider = inner
            .providers
            .provider(request.provider.as_deref())
            .await
            .map_err(AgentError::Provider)?;

        // One run per site, and a global cap on runs in progress.
        if !lock(&inner.busy_sites).insert(name.to_string()) {
            return Err(AgentError::Busy(format!(
                "site {name} already has a run in progress"
            )));
        }
        let permit = match inner.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                lock(&inner.busy_sites).remove(name.as_str());
                return Err(AgentError::Busy(format!(
                    "{} runs are already in progress; try again when one finishes",
                    inner.config.agent.max_concurrent_runs
                )));
            }
        };

        let now = now_ms();
        let (site_name, provider_id, model) = (
            name.as_str(),
            provider.id().to_owned(),
            provider.model().to_owned(),
        );
        let inserted = sqlx::query!(
            "INSERT INTO agent_runs (site_name, prompt, status, provider, model, created_at) VALUES (?1, ?2, 'running', ?3, ?4, ?5)",
            site_name,
            prompt,
            provider_id,
            model,
            now
        )
        .execute(&inner.db)
        .await;
        let run_id = match inserted {
            Ok(result) => result.last_insert_rowid(),
            Err(e) => {
                lock(&inner.busy_sites).remove(name.as_str());
                return Err(e.into());
            }
        };

        let (events, _) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        lock(&inner.active).insert(
            run_id,
            Active {
                events: events.clone(),
                cancel: cancel.clone(),
            },
        );

        let job = Job {
            inner: inner.clone(),
            run_id,
            site: name.to_string(),
            may_create: request.new,
            prompt,
            provider,
            events,
            cancel,
            seq: 0,
            usage: Usage::default(),
            turns: 0,
        };
        tokio::spawn(async move {
            let _permit = permit;
            job.run().await;
        });
        self.get(run_id).await
    }

    /// Refuse to start when today's spend is already at the cap.
    async fn check_daily_budget(&self) -> Result<(), AgentError> {
        let cap = self.inner.config.agent.daily_spend_cap_usd;
        let spent = spent_today(&self.inner.db).await?;
        if spent >= cap {
            return Err(AgentError::Budget(format!(
                "today's LLM spend (${spent:.2}) has reached the daily cap (${cap:.2}); raise agent.daily_spend_cap_usd or wait until tomorrow (UTC)"
            )));
        }
        Ok(())
    }

    pub async fn get(&self, run_id: i64) -> Result<Run, AgentError> {
        let row = sqlx::query!(
            r#"SELECT id as "id!", site_name, prompt, status, provider, model, turns, input_tokens, output_tokens,
                      cost_usd, message, created_at, finished_at
               FROM agent_runs WHERE id = ?1"#,
            run_id
        )
        .fetch_optional(&self.inner.db)
        .await?
        .ok_or(AgentError::NotFound(run_id))?;
        Ok(Run {
            id: row.id,
            site_name: row.site_name,
            prompt: row.prompt,
            status: RunStatus::parse(&row.status),
            provider: row.provider,
            model: row.model,
            turns: row.turns,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cost_usd: row.cost_usd,
            message: row.message,
            created_at: row.created_at,
            finished_at: row.finished_at,
        })
    }

    /// The most recent runs, newest first, optionally for one site.
    pub async fn list(&self, site: Option<&str>, limit: i64) -> Result<Vec<Run>, AgentError> {
        let ids = sqlx::query_scalar!(
            r#"SELECT id as "id!" FROM agent_runs WHERE ?1 IS NULL OR site_name = ?1 ORDER BY id DESC LIMIT ?2"#,
            site,
            limit
        )
        .fetch_all(&self.inner.db)
        .await?;
        let mut runs = Vec::with_capacity(ids.len());
        for id in ids {
            runs.push(self.get(id).await?);
        }
        Ok(runs)
    }

    /// Stored events so far plus, if the run is still going, a live receiver.
    pub async fn subscribe(&self, run_id: i64) -> Result<Subscription, AgentError> {
        // Take the live receiver first so nothing falls between replay and live.
        let live = lock(&self.inner.active)
            .get(&run_id)
            .map(|a| a.events.subscribe());
        self.get(run_id).await?;
        let rows = sqlx::query!(
            "SELECT seq, data FROM agent_events WHERE run_id = ?1 ORDER BY seq",
            run_id
        )
        .fetch_all(&self.inner.db)
        .await?;
        let past = rows
            .into_iter()
            .filter_map(|r| Some((r.seq, serde_json::from_str(&r.data).ok()?)))
            .collect();
        Ok(Subscription { past, live })
    }

    /// Ask a run to stop. Returns false if it is not running.
    pub fn cancel(&self, run_id: i64) -> bool {
        match lock(&self.inner.active).get(&run_id) {
            Some(active) => {
                active.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// US dollars spent on runs so far today (UTC).
    pub async fn spent_today(&self) -> Result<f64, AgentError> {
        Ok(spent_today(&self.inner.db).await?)
    }

    /// Whether a site has a run in progress.
    pub fn is_busy(&self, site: &str) -> bool {
        lock(&self.inner.busy_sites).contains(site)
    }
}

/// US dollars spent on runs created since the start of the current UTC day.
async fn spent_today(db: &SqlitePool) -> Result<f64, sqlx::Error> {
    let day_start = now_ms().div_euclid(MS_PER_DAY) * MS_PER_DAY;
    let spent = sqlx::query_scalar!(
        r#"SELECT coalesce(sum(cost_usd), 0.0) as "spent!: f64" FROM agent_runs WHERE created_at >= ?1"#,
        day_start
    )
    .fetch_one(db)
    .await?;
    Ok(spent)
}

/// One run in progress.
struct Job {
    inner: Arc<Inner>,
    run_id: i64,
    site: String,
    may_create: bool,
    prompt: String,
    provider: Arc<dyn LlmProvider>,
    events: broadcast::Sender<(i64, RunEvent)>,
    cancel: CancellationToken,
    seq: i64,
    usage: Usage,
    turns: u32,
}

impl Job {
    async fn run(mut self) {
        self.emit(RunEvent::Started {
            site: self.site.clone(),
            provider: self.provider.id().to_owned(),
            model: self.provider.model().to_owned(),
        })
        .await;
        let (status, message) = self.drive().await;
        if status == RunStatus::Failed {
            tracing::warn!(run = self.run_id, site = %self.site, %message, "run failed");
        }

        let now = now_ms();
        let status_str = status.as_str();
        let saved = sqlx::query!(
            "UPDATE agent_runs SET status = ?1, message = ?2, finished_at = ?3 WHERE id = ?4",
            status_str,
            message,
            now,
            self.run_id
        )
        .execute(&self.inner.db)
        .await;
        if let Err(error) = saved {
            tracing::error!(%error, run = self.run_id, "could not save the run's outcome");
        }
        self.emit(RunEvent::Done {
            status: status_str.to_owned(),
            message,
        })
        .await;
        lock(&self.inner.active).remove(&self.run_id);
        lock(&self.inner.busy_sites).remove(&self.site);
    }

    /// The loop. Returns how the run ended and a message for the user.
    async fn drive(&mut self) -> (RunStatus, String) {
        let limits = self.inner.config.agent.clone();
        let tool_defs = tools::definitions();
        let mut ctx = ToolContext {
            store: self.inner.store.clone(),
            domains: self.inner.domains.clone(),
            skills: self.inner.skills.clone(),
            site: self.site.clone(),
            may_create: self.may_create,
            http_port: self
                .inner
                .config
                .domains
                .public_port
                .filter(|_| {
                    self.inner.config.domains.public_scheme == sw_core::config::Scheme::Http
                })
                .unwrap_or(80),
        };
        let system = match self.site_context().await {
            Ok(context) => system_prompt(&context),
            Err(e) => return (RunStatus::Failed, e.to_string()),
        };
        let mut messages = vec![Message::User {
            content: self.prompt.clone(),
        }];
        let mut last_text = String::new();

        loop {
            if self.cancel.is_cancelled() {
                return (RunStatus::Cancelled, "cancelled".into());
            }
            if self.turns >= limits.max_turns {
                return (
                    RunStatus::Limit,
                    format!(
                        "stopped after {} model calls (agent.max_turns)",
                        limits.max_turns
                    ),
                );
            }
            if self.usage.total() >= limits.max_run_tokens {
                return (
                    RunStatus::Limit,
                    format!(
                        "stopped: the run used {} tokens, over the limit of {} (agent.max_run_tokens)",
                        self.usage.total(),
                        limits.max_run_tokens
                    ),
                );
            }
            match spent_today(&self.inner.db).await {
                Ok(spent) if spent >= limits.daily_spend_cap_usd => {
                    return (
                        RunStatus::Limit,
                        format!(
                            "stopped: today's LLM spend (${spent:.2}) reached the daily cap (${:.2})",
                            limits.daily_spend_cap_usd
                        ),
                    );
                }
                Ok(_) => {}
                Err(e) => return (RunStatus::Failed, format!("database error: {e}")),
            }

            // One model call.
            self.turns += 1;
            let request = ChatRequest {
                system: system.clone(),
                messages: messages.clone(),
                tools: tool_defs.clone(),
            };
            let stream = tokio::select! {
                _ = self.cancel.cancelled() => return (RunStatus::Cancelled, "cancelled".into()),
                result = self.provider.stream(request) => result,
            };
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(e) => return (RunStatus::Failed, e.to_string()),
            };
            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut turn_usage = Usage::default();
            let mut stop = None;
            let mut raw = None;
            loop {
                let event = tokio::select! {
                    _ = self.cancel.cancelled() => return (RunStatus::Cancelled, "cancelled".into()),
                    event = stream.next() => event,
                };
                match event {
                    Some(Ok(Event::TextDelta(delta))) => {
                        text.push_str(&delta);
                        self.emit(RunEvent::TextDelta { delta }).await;
                    }
                    Some(Ok(Event::ToolCall(call))) => calls.push(call),
                    Some(Ok(Event::Usage(usage))) => turn_usage = usage,
                    Some(Ok(Event::Raw(value))) => raw = Some(value),
                    Some(Ok(Event::Stop(reason))) => stop = Some(reason),
                    Some(Err(e)) => {
                        self.record_usage(turn_usage).await;
                        return (RunStatus::Failed, e.to_string());
                    }
                    None => break,
                }
            }
            drop(stream);
            self.record_usage(turn_usage).await;
            if !text.trim().is_empty() {
                self.emit(RunEvent::Text { text: text.clone() }).await;
                last_text = text.clone();
            }

            if calls.is_empty() {
                return match stop {
                    Some(StopReason::MaxTokens) => (
                        RunStatus::Failed,
                        "the model's answer was cut off by its output token limit".into(),
                    ),
                    Some(StopReason::Other(reason)) => (
                        RunStatus::Failed,
                        format!("the model stopped unexpectedly ({reason})"),
                    ),
                    _ => (
                        RunStatus::Succeeded,
                        if last_text.trim().is_empty() {
                            "Done.".into()
                        } else {
                            last_text
                        },
                    ),
                };
            }

            messages.push(Message::Assistant {
                text,
                tool_calls: calls.clone(),
                raw,
            });
            for call in calls {
                self.emit(RunEvent::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: shorten_value(&call.arguments),
                })
                .await;
                // Tools are quick and must not be torn in half, so they are not cancelled mid-call.
                let outcome = tools::execute(&ctx, &call).await;
                match outcome.effect {
                    Some(Effect::Created) => ctx.may_create = false,
                    Some(Effect::ConfirmDelete) => {
                        self.emit(RunEvent::Confirmation {
                            action: "delete_site".into(),
                            site: self.site.clone(),
                            message: format!(
                                "The agent asks to delete the site {}. Type its name to confirm.",
                                self.site
                            ),
                        })
                        .await;
                    }
                    None => {}
                }
                self.emit(RunEvent::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    is_error: outcome.is_error,
                    content: shorten(&outcome.content),
                })
                .await;
                messages.push(Message::Tool {
                    call_id: call.id,
                    name: call.name,
                    content: outcome.content,
                    is_error: outcome.is_error,
                });
            }
        }
    }

    async fn site_context(&self) -> Result<SiteContext, SiteError> {
        let store = &self.inner.store;
        let name = SiteName::parse(&self.site)?;
        let url = store
            .domains()
            .public_url(&store.domains().site_hostname(&name));
        if self.may_create {
            let skills = self.inner.skills.index();
            return Ok(SiteContext {
                name: self.site.clone(),
                url,
                existing: None,
                skills,
            });
        }
        let info = store.info(&self.site).await?;
        let files = store.list_files(&self.site).await?;
        Ok(SiteContext {
            skills: self.inner.skills.index(),
            name: self.site.clone(),
            url: info.url,
            existing: Some(ExistingSite {
                title: info.site.title,
                files,
                releases: info.releases,
                current_release: info.site.current_release,
            }),
        })
    }

    /// Add one model call's usage to the run's totals, in memory and in the database.
    async fn record_usage(&mut self, usage: Usage) {
        self.usage += usage;
        let cost = self
            .inner
            .config
            .llm
            .price(self.provider.model())
            .map(|p| p.cost(self.usage.input_tokens, self.usage.output_tokens));
        let (input, output, turns) = (
            self.usage.input_tokens as i64,
            self.usage.output_tokens as i64,
            i64::from(self.turns),
        );
        let saved = sqlx::query!(
            "UPDATE agent_runs SET input_tokens = ?1, output_tokens = ?2, cost_usd = ?3, turns = ?4 WHERE id = ?5",
            input,
            output,
            cost,
            turns,
            self.run_id
        )
        .execute(&self.inner.db)
        .await;
        if let Err(error) = saved {
            tracing::error!(%error, run = self.run_id, "could not save usage");
        }
        self.emit(RunEvent::Usage {
            input_tokens: self.usage.input_tokens,
            output_tokens: self.usage.output_tokens,
            cost_usd: cost,
        })
        .await;
    }

    /// Store (unless it is a text delta) and broadcast an event.
    async fn emit(&mut self, event: RunEvent) {
        let mut seq = 0;
        if event.is_stored() {
            self.seq += 1;
            seq = self.seq;
            let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
            let now = now_ms();
            let saved = sqlx::query!(
                "INSERT INTO agent_events (run_id, seq, ts, data) VALUES (?1, ?2, ?3, ?4)",
                self.run_id,
                seq,
                now,
                data
            )
            .execute(&self.inner.db)
            .await;
            if let Err(error) = saved {
                tracing::error!(%error, run = self.run_id, "could not store a run event");
            }
        }
        // No subscribers is fine.
        let _ = self.events.send((seq, event));
    }
}
