//! Background jobs and scheduled tasks of site backends, through real workers: they run
//! with exactly the access the site's request handlers have.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sw_backend::{
    BackendRequest, BackendSource, DraftTask, Egress, JobRecord, LiveBackend, SiteBackends,
    WorkerCommand,
};
use sw_core::config::BackendsConfig;
use sw_core::{Config, DataDir, SiteName};
use sw_sites::{Author, ReleaseHooks, ReleasePlan, SiteStore};

struct Env {
    dir: tempfile::TempDir,
    backends: SiteBackends,
    db: sqlx::SqlitePool,
    site_id: i64,
    other_id: i64,
    name: SiteName,
    other: SiteName,
}

async fn env() -> Env {
    sw_core::crypto::install_default_provider();
    let dir = tempfile::tempdir().unwrap();
    let data = DataDir::new(dir.path());
    data.ensure().unwrap();
    let db = sw_core::db::open(&data.db_file()).await.unwrap();
    sw_core::db::migrate(&db, &data.backups_dir())
        .await
        .unwrap();
    let config = Config::from_toml("[domains]\nzones = [\"example.com\"]\n").unwrap();
    let store = SiteStore::open(db.clone(), data.clone(), &config)
        .await
        .unwrap();
    let site = store.create("news", "News", Author::User).await.unwrap();
    let other = store.create("other", "Other", Author::User).await.unwrap();
    let command = WorkerCommand {
        program: std::env::var_os("SW_BACKEND_TEST_WORKER")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_sw-backend-worker").into())
            .into(),
        args: Vec::new(),
    };
    let backends = SiteBackends::with_egress(
        &BackendsConfig::default(),
        data,
        db.clone(),
        command,
        Egress::for_tests(),
    );
    Env {
        dir,
        backends,
        db,
        site_id: site.id,
        other_id: other.id,
        name: site.name,
        other: other.name,
    }
}

const APP: &str = r#"
import { Router, json, db, jobs, secrets, llm, fetch } from "sitewright";
const app = new Router();

jobs.define("record", async (payload, info) => {
  await db.run("insert into log (what) values (?)", `record:${payload.n}:attempt ${info.attempt}`);
});
jobs.define("flaky", async (payload, info) => {
  if (info.attempt < 2) throw new Error("not yet");
  await db.run("insert into log (what) values ('flaky done')");
});
jobs.define("ask", async (payload) => {
  const answer = await llm.chat(payload.question);
  await db.run("insert into log (what) values (?)", `ask:${answer.text}`);
});
jobs.define("leak", async () => {
  throw new Error(`the key is ${await secrets.get("NEWS_KEY")}`);
});
jobs.define("outside", async () => {
  await fetch("https://not-approved.example.org/feed");
});
jobs.define("slow", async () => {
  const end = Date.now() + 1500;
  while (Date.now() < end) {}
});
jobs.define("chain", async () => {
  await jobs.enqueue("record", { n: "chained" });
});
jobs.schedule("tick", "*/5 * * * *", async (info) => {
  await db.run("insert into log (what) values (?)", `tick:${info.name}`);
});

app.post("/api/enqueue/:name", async (req) => {
  const { payload, attempts } = await req.json();
  return json(await jobs.enqueue(req.params.name, payload, { attempts }));
});
app.get("/api/log", async () => json((await db.query("select what from log order by id")).map((r) => r.what)));
app.get("/api/ask", async () => json(await llm.chat("from a request")));
app.get("/api/declare", async () => { jobs.define("late", async () => {}); return json({}); });
export default app;
"#;

const MIGRATION: &str = "CREATE TABLE log (id INTEGER PRIMARY KEY, what TEXT NOT NULL);";

fn write_release(root: &Path, index: &str) {
    std::fs::create_dir_all(root.join("backend/migrations")).unwrap();
    std::fs::write(root.join("backend/index.js"), index).unwrap();
    std::fs::write(root.join("backend/migrations/0001_log.sql"), MIGRATION).unwrap();
}

/// Publish `index` as the live release of a site, through the same hooks as a real
/// publish, and return what the scheduler would be given.
async fn publish(env: &Env, site_id: i64, site: &SiteName, sha: &str, index: &str) -> LiveBackend {
    let root = env.dir.path().join(format!("release-{site}-{sha}"));
    write_release(&root, index);
    let plan = ReleasePlan {
        site_id,
        site: site.clone(),
        sha: sha.to_owned(),
        root: root.clone(),
    };
    env.backends.prepare(&plan).await.unwrap();
    env.backends.commit(&plan).await;
    LiveBackend {
        site_id,
        site: site.clone(),
        sha: sha.to_owned(),
        root,
    }
}

fn request(method: &str, path: &str, body: Value) -> BackendRequest {
    BackendRequest {
        method: method.into(),
        path: path.into(),
        query: String::new(),
        headers: vec![("content-type".into(), "application/json".into())],
        body: if body.is_null() {
            String::new()
        } else {
            body.to_string()
        },
    }
}

async fn call(
    env: &Env,
    live: &LiveBackend,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let response = env
        .backends
        .serve(
            live.site_id,
            &live.site,
            &live.sha,
            &live.root,
            request(method, path, body),
        )
        .await
        .unwrap();
    let value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    (response.status, value)
}

async fn enqueue(
    env: &Env,
    live: &LiveBackend,
    name: &str,
    payload: Value,
    attempts: i64,
) -> (u16, Value) {
    call(
        env,
        live,
        "POST",
        &format!("/api/enqueue/{name}"),
        json!({ "payload": payload, "attempts": attempts }),
    )
    .await
}

/// One scheduler pass, waiting until the work it started has finished.
async fn pass(env: &Env, live: &[LiveBackend]) {
    env.backends.run_due(live.to_vec()).await;
    let start = Instant::now();
    while live.iter().any(|b| env.backends.jobs_running(b.site_id)) {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "background work did not finish"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn log(env: &Env, live: &LiveBackend) -> Vec<String> {
    let (_, value) = call(env, live, "GET", "/api/log", Value::Null).await;
    serde_json::from_value(value).unwrap()
}

async fn record(env: &Env, site_id: i64, name: &str) -> JobRecord {
    env.backends
        .jobs(site_id, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.name == name)
        .unwrap_or_else(|| panic!("no job named {name}"))
}

#[tokio::test]
async fn queued_jobs_and_schedules_run_in_the_background() {
    let env = env().await;
    let live = publish(&env, env.site_id, &env.name, "v1", APP).await;
    let all = [live.clone()];

    // The schedule is known from the publish, with its next run.
    let schedules = env.backends.schedules(env.site_id).await;
    assert_eq!(schedules.len(), 1);
    assert_eq!(
        (schedules[0].name.as_str(), schedules[0].cron.as_str()),
        ("tick", "*/5 * * * *")
    );
    assert!(schedules[0].next_at.unwrap() > sw_core::time::now_ms());

    // A request queues a job; it runs on the next pass, not during the request.
    let (status, queued) = enqueue(&env, &live, "record", json!({"n": 1}), 3).await;
    assert_eq!(status, 200, "{queued}");
    assert!(queued["id"].as_i64().unwrap() > 0);
    assert!(log(&env, &live).await.is_empty());
    assert_eq!(env.backends.pending_jobs(env.site_id).await.unwrap(), 1);
    pass(&env, &all).await;
    assert_eq!(log(&env, &live).await, ["record:1:attempt 1"]);
    let done = record(&env, env.site_id, "record").await;
    assert_eq!(
        (done.kind.as_str(), done.status.as_str(), done.attempts),
        ("job", "done", 1)
    );
    assert_eq!(env.backends.pending_jobs(env.site_id).await.unwrap(), 0);

    // Jobs can queue jobs.
    enqueue(&env, &live, "chain", Value::Null, 1).await;
    pass(&env, &all).await;
    pass(&env, &all).await;
    assert!(
        log(&env, &live)
            .await
            .contains(&"record:chained:attempt 1".to_owned())
    );

    // A schedule runs when due, and is recorded like a job.
    pass(&env, &all).await;
    assert!(!log(&env, &live).await.iter().any(|l| l.starts_with("tick")));
    env.backends.make_schedules_due(env.site_id).await;
    pass(&env, &all).await;
    assert!(log(&env, &live).await.contains(&"tick:tick".to_owned()));
    let tick = record(&env, env.site_id, "tick").await;
    assert_eq!(
        (tick.kind.as_str(), tick.status.as_str()),
        ("schedule", "done")
    );
    // It is not run again until its next time.
    pass(&env, &all).await;
    let ticks = log(&env, &live)
        .await
        .iter()
        .filter(|l| l.starts_with("tick"))
        .count();
    assert_eq!(ticks, 1);

    // Failures are retried with a delay, then succeed.
    enqueue(&env, &live, "flaky", Value::Null, 2).await;
    pass(&env, &all).await;
    let flaky = record(&env, env.site_id, "flaky").await;
    assert_eq!(flaky.status, "queued");
    assert_eq!(flaky.error.as_deref(), Some("not yet"));
    assert!(
        flaky.run_at > sw_core::time::now_ms() + 20_000,
        "retried after a delay"
    );
    sqlx::query("UPDATE site_jobs SET run_at = 0 WHERE status = 'queued'")
        .execute(&env.db)
        .await
        .unwrap();
    pass(&env, &all).await;
    let flaky = record(&env, env.site_id, "flaky").await;
    assert_eq!((flaky.status.as_str(), flaky.attempts), ("done", 2));
    assert!(log(&env, &live).await.contains(&"flaky done".to_owned()));
}

#[tokio::test]
async fn jobs_have_the_same_access_as_requests_and_no_more() {
    use sw_llm::mock::{ScriptedProvider, Turn};

    let env = env().await;
    let live = publish(&env, env.site_id, &env.name, "v1", APP).await;
    let all = [live.clone()];
    let models = Arc::new(Models(Arc::new(ScriptedProvider::repeating(Turn::text(
        "the model's answer",
    )))));
    env.backends.llm().set_models(models.clone());

    // AI models: off for the site, so a job is refused exactly as a request is.
    let (status, _) = call(&env, &live, "GET", "/api/ask", Value::Null).await;
    assert_eq!(status, 500);
    enqueue(&env, &live, "ask", json!({"question": "hi"}), 1).await;
    pass(&env, &all).await;
    let ask = record(&env, env.site_id, "ask").await;
    assert_eq!(ask.status, "failed");
    assert!(
        ask.error
            .as_deref()
            .unwrap()
            .contains("not turned on for this site"),
        "{ask:?}"
    );
    assert!(models.0.requests().is_empty());

    // The owner turns them on: the job reaches the model, and the call counts against
    // the site's limits and usage like any other.
    let mut settings = env.backends.llm().settings(env.site_id).await.unwrap();
    settings.enabled = true;
    env.backends
        .llm()
        .set_settings(env.site_id, &settings)
        .await
        .unwrap();
    enqueue(
        &env,
        &live,
        "ask",
        json!({"question": "summarise the news"}),
        1,
    )
    .await;
    pass(&env, &all).await;
    assert!(
        log(&env, &live)
            .await
            .contains(&"ask:the model's answer".to_owned())
    );
    assert_eq!(models.0.requests().len(), 1);
    assert_eq!(
        env.backends
            .llm()
            .usage(env.site_id)
            .await
            .unwrap()
            .requests,
        1
    );
    assert_eq!(
        env.backends
            .llm()
            .usage(env.other_id)
            .await
            .unwrap()
            .requests,
        0
    );

    // Outside hosts: only approved ones, and asking adds a request for the owner.
    enqueue(&env, &live, "outside", Value::Null, 1).await;
    pass(&env, &all).await;
    let outside = record(&env, env.site_id, "outside").await;
    assert!(
        outside
            .error
            .as_deref()
            .unwrap()
            .contains("not on this site's list of approved hosts"),
        "{outside:?}"
    );
    let hosts = env.backends.hosts(env.site_id).await.unwrap();
    assert_eq!(
        (hosts[0].host.as_str(), hosts[0].approved),
        ("not-approved.example.org", false)
    );

    // Secrets: the site's own, and a recorded error never shows the value.
    env.backends
        .set_secret(env.site_id, "NEWS_KEY", "sk-news-0123456789")
        .unwrap();
    env.backends
        .set_secret(env.other_id, "NEWS_KEY", "sk-other-0123456789")
        .unwrap();
    enqueue(&env, &live, "leak", Value::Null, 1).await;
    pass(&env, &all).await;
    let leak = record(&env, env.site_id, "leak").await;
    let error = leak.error.unwrap();
    assert!(error.contains("[secret NEWS_KEY]"), "{error}");
    assert!(!error.contains("sk-news"), "{error}");

    // Another site's jobs, data and queue are its own.
    let other = publish(&env, env.other_id, &env.other, "o1", APP).await;
    enqueue(&env, &other, "record", json!({"n": "other"}), 1).await;
    pass(&env, &[live.clone(), other.clone()]).await;
    assert_eq!(log(&env, &other).await, ["record:other:attempt 1"]);
    assert!(!log(&env, &live).await.iter().any(|l| l.contains("other")));
    assert_eq!(env.backends.jobs(env.other_id, 100).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_long_job_does_not_hold_up_visitors() {
    let env = env().await;
    let live = publish(&env, env.site_id, &env.name, "v1", APP).await;
    enqueue(&env, &live, "slow", Value::Null, 1).await;
    env.backends.run_due(vec![live.clone()]).await;
    // Wait until the job is running, then ask the site something.
    let start = Instant::now();
    while record(&env, env.site_id, "slow").await.status != "running" {
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let asked = Instant::now();
    let (status, _) = call(&env, &live, "GET", "/api/log", Value::Null).await;
    assert_eq!(status, 200);
    assert!(
        asked.elapsed() < Duration::from_millis(1000),
        "a request waited {:?} for a background job",
        asked.elapsed()
    );
    assert!(env.backends.jobs_running(env.site_id));
    while env.backends.jobs_running(env.site_id) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(record(&env, env.site_id, "slow").await.status, "done");
}

#[tokio::test]
async fn mistakes_are_caught_early() {
    let env = env().await;
    let live = publish(&env, env.site_id, &env.name, "v1", APP).await;

    // Enqueueing a job the code does not declare fails at once.
    let (status, _) = enqueue(&env, &live, "missing", Value::Null, 1).await;
    assert_eq!(status, 500);
    let logs = env.backends.logs(env.site_id, 50);
    assert!(
        logs.iter()
            .any(|l| l.message.contains("there is no job named missing")),
        "{logs:?}"
    );
    assert!(env.backends.jobs(env.site_id, 10).await.unwrap().is_empty());

    // Bad arguments.
    let (status, _) = enqueue(&env, &live, "record", Value::Null, 99).await;
    assert_eq!(status, 500);

    // Declaring outside the top level.
    let (status, _) = call(&env, &live, "GET", "/api/declare", Value::Null).await;
    assert_eq!(status, 500);
    assert!(
        env.backends
            .logs(env.site_id, 50)
            .iter()
            .any(|l| l.message.contains("top level"))
    );

    // A publish with an invalid schedule is refused, with the reason.
    let bad = APP.replace("*/5 * * * *", "*/5 * * *");
    let root = env.dir.path().join("bad");
    write_release(&root, &bad);
    let plan = ReleasePlan {
        site_id: env.site_id,
        site: env.name.clone(),
        sha: "bad".into(),
        root,
    };
    let error = env.backends.prepare(&plan).await.unwrap_err();
    assert!(
        error.contains("schedule tick") && error.contains("five fields"),
        "{error}"
    );
    env.backends.abandon(&plan).await;

    // A queued job whose handler a later release removed fails without running.
    enqueue(&env, &live, "record", json!({"n": 2}), 3).await;
    let without = APP.replace("jobs.define(\"record\"", "jobs.define(\"recorded\"");
    let v2 = publish(&env, env.site_id, &env.name, "v2", &without).await;
    pass(&env, &[v2]).await;
    let orphan = record(&env, env.site_id, "record").await;
    assert_eq!(orphan.status, "failed");
    assert!(orphan.error.unwrap().contains("no job named record"));
}

#[tokio::test]
async fn the_agent_can_try_jobs_on_a_draft() {
    let env = env().await;
    let source = || BackendSource {
        files: [("backend/index.js".to_owned(), APP.to_owned())].into(),
        migrations: vec![sw_backend::Migration {
            name: "0001_log.sql".into(),
            sql: MIGRATION.into(),
        }],
    };
    let b = &env.backends;

    // A request that enqueues: the job is reported, not queued for real.
    let out = b
        .test_draft(
            env.site_id,
            &env.name,
            source(),
            request(
                "POST",
                "/api/enqueue/record",
                json!({"payload": {"n": 5}, "attempts": 1}),
            ),
            false,
        )
        .await
        .unwrap();
    assert_eq!(out.status, 200, "{out:?}");
    assert_eq!(out.jobs_enqueued.len(), 1);
    assert_eq!(out.jobs_enqueued[0]["name"], "record");
    assert_eq!(out.jobs_enqueued[0]["payload"], json!({"n": 5}));
    assert_eq!(out.tasks.schedules[0].name, "tick");
    assert!(out.tasks.jobs.contains(&"record".to_owned()));
    assert!(out.tasks_problem.is_none());
    assert!(b.jobs(env.site_id, 10).await.unwrap().is_empty());

    // Run the job and the schedule by hand, against the draft's data.
    let out = b
        .test_draft_task(
            env.site_id,
            &env.name,
            source(),
            DraftTask::Job {
                name: "record".into(),
                payload: json!({"n": 5}),
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(out.status, 200, "{out:?}");
    let out = b
        .test_draft_task(
            env.site_id,
            &env.name,
            source(),
            DraftTask::Schedule {
                name: "tick".into(),
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(out.status, 200, "{out:?}");
    let out = b
        .test_draft(
            env.site_id,
            &env.name,
            source(),
            request("GET", "/api/log", Value::Null),
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&out.body).unwrap(),
        json!(["record:5:attempt 1", "tick:tick"])
    );

    // A failing job reports its error and console output.
    let out = b
        .test_draft_task(
            env.site_id,
            &env.name,
            source(),
            DraftTask::Job {
                name: "flaky".into(),
                payload: Value::Null,
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!((out.status, out.body.as_str()), (500, "not yet"));
    assert!(
        out.logs
            .iter()
            .any(|l| l.message.contains("job flaky failed"))
    );

    // A broken schedule is pointed out before any publish.
    let broken = || BackendSource {
        files: [(
            "backend/index.js".to_owned(),
            APP.replace("*/5 * * * *", "61 * * * *"),
        )]
        .into(),
        migrations: source().migrations,
    };
    let out = b
        .test_draft(
            env.site_id,
            &env.name,
            broken(),
            request("GET", "/api/log", Value::Null),
            false,
        )
        .await
        .unwrap();
    assert!(out.tasks_problem.unwrap().contains("minute"));
}

struct Models(Arc<sw_llm::mock::ScriptedProvider>);

impl sw_backend::ModelSource for Models {
    fn list(
        &self,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<sw_backend::ModelInfo>, String>> {
        Box::pin(async {
            Ok(vec![sw_backend::ModelInfo {
                name: "fast".into(),
                kind: "mock".into(),
                model: "mock-model".into(),
                is_default: true,
            }])
        })
    }

    fn provider<'a>(
        &'a self,
        name: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<Arc<dyn sw_llm::LlmProvider>, String>> {
        Box::pin(async move {
            match name {
                "fast" => Ok(self.0.clone() as Arc<dyn sw_llm::LlmProvider>),
                other => Err(format!("no model {other}")),
            }
        })
    }

    fn price(&self, _model: &str) -> Option<(f64, f64)> {
        None
    }
}
