//! M5 end to end: the beacon, the request log feeding the default dashboard, uptime
//! status, and an AI dashboard saved (and bad ones refused) through the agent's tools.

#![allow(clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::{
    ADMIN, ORIGIN, TestDaemon, boot_with, host, llm_config, mock_model, path_str, text_turn,
    tool_turn, write_secrets,
};
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};

const BROWSER: &str =
    "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/130.0 Mobile Safari/537.36";
const PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
    <title>T</title><script src=\"/_sw/a.js\" defer></script></head><body><h1>Trails</h1></body></html>";

/// A signed-in admin, without going through a passkey ceremony.
struct Admin<'a> {
    t: &'a TestDaemon,
    cookie: String,
    csrf: String,
}

impl<'a> Admin<'a> {
    async fn new(t: &'a TestDaemon) -> Self {
        let session = t.daemon.state.auth.session_for_tests().await.unwrap();
        Self {
            t,
            cookie: format!("__Host-sw_session={}", session.token),
            csrf: session.session.csrf_token,
        }
    }

    async fn get(&self, path: &str) -> Response {
        self.t
            .http
            .get(format!("http://{}{path}", self.t.daemon.addr))
            .header("host", ADMIN)
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, fields: &[(&str, &str)]) -> Response {
        self.t
            .http
            .post(format!("http://{}{path}", self.t.daemon.addr))
            .header("host", ADMIN)
            .header("origin", ORIGIN)
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .form(fields)
            .send()
            .await
            .unwrap()
    }

    async fn dashboard(&self, site: &str) -> Value {
        self.get(&format!("/sites/{site}/dashboard/data?days=7"))
            .await
            .json()
            .await
            .unwrap()
    }
}

async fn visit(t: &TestDaemon, site: &str, path: &str, forwarded: &str) -> StatusCode {
    t.http
        .get(format!("http://{}{path}", t.daemon.addr))
        .header("host", host(site))
        .header("user-agent", BROWSER)
        .header("x-forwarded-for", forwarded)
        .send()
        .await
        .unwrap()
        .status()
}

async fn event(
    t: &TestDaemon,
    site: &str,
    body: &str,
    forwarded: &str,
    user_agent: &str,
) -> StatusCode {
    t.http
        .post(format!("http://{}/_sw/e", t.daemon.addr))
        .header("host", host(site))
        .header("user-agent", user_agent)
        .header("x-forwarded-for", forwarded)
        .header("content-type", "text/plain;charset=UTF-8")
        .body(body.to_owned())
        .send()
        .await
        .unwrap()
        .status()
}

fn panel<'a>(data: &'a Value, group: &str, id: &str) -> &'a Value {
    data[group]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap_or_else(|| panic!("no panel {id} in {group}"))
}

async fn import(t: &TestDaemon, name: &str) {
    let dir = t.site_dir(
        name,
        &[
            ("index.html", PAGE),
            ("trails/index.html", PAGE),
            ("trails/rainier.html", PAGE),
            ("css/site.css", "body{}"),
        ],
    );
    t.cli(&["site", "import", name, path_str(&dir)])
        .await
        .unwrap();
}

#[tokio::test]
async fn visits_and_beacon_events_reach_the_default_dashboard() {
    let t = boot_with("[metrics]\nuptime_interval_secs = 0\n").await;
    import(&t, "hikes").await;
    import(&t, "other").await;

    // The beacon script is served on the site's own host: tiny, cacheable, no cookies.
    let script = t.get(&host("hikes"), "/_sw/a.js").await;
    assert_eq!(script.status(), StatusCode::OK);
    assert!(
        script.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert!(script.headers().get("set-cookie").is_none());
    let body = script.text().await.unwrap();
    assert!(body.len() < 1024 && body.contains("/_sw/e"));

    // Two visitors browse; one page is missing; another site gets traffic too.
    for (path, who) in [
        ("/", "203.0.113.1"),
        ("/trails/", "203.0.113.1"),
        ("/trails/rainier", "203.0.113.1"),
        ("/", "203.0.113.2"),
        ("/css/site.css", "203.0.113.2"),
    ] {
        assert_eq!(
            visit(&t, "hikes", path, who).await,
            StatusCode::OK,
            "{path}"
        );
    }
    assert_eq!(
        visit(&t, "hikes", "/nope", "203.0.113.2").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(visit(&t, "other", "/", "203.0.113.9").await, StatusCode::OK);

    // Beacon events: accepted, validated, rate limited, and silent for bots.
    assert_eq!(
        event(
            &t,
            "hikes",
            r#"{"n":"pageview","u":"/","p":{"ref":"news.example.org","screen":"small"}}"#,
            "203.0.113.1",
            BROWSER
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        event(
            &t,
            "hikes",
            r#"{"n":"search","u":"/trails/","p":{"q":"rainier","results":2}}"#,
            "203.0.113.1",
            BROWSER
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        event(
            &t,
            "hikes",
            r#"{"n":"search","u":"/trails/","p":{"q":"si","results":0}}"#,
            "203.0.113.2",
            BROWSER
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        event(&t, "hikes", "not json", "203.0.113.1", BROWSER).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        event(&t, "hikes", r#"{"n":"<script>"}"#, "203.0.113.1", BROWSER).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        event(
            &t,
            "hikes",
            &format!(r#"{{"n":"x","u":"/{}"}}"#, "a".repeat(3000)),
            "203.0.113.1",
            BROWSER
        )
        .await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        event(
            &t,
            "hikes",
            r#"{"n":"search"}"#,
            "203.0.113.3",
            "curl/8.5.0"
        )
        .await,
        StatusCode::NO_CONTENT,
        "bots are ignored quietly"
    );
    assert_eq!(
        t.get(&host("hikes"), "/_sw/e").await.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        event(&t, "nosuchsite", r#"{"n":"x"}"#, "203.0.113.1", BROWSER).await,
        StatusCode::NOT_FOUND
    );
    let mut limited = false;
    for _ in 0..70 {
        limited |= event(&t, "hikes", r#"{"n":"spam"}"#, "203.0.113.50", BROWSER).await
            == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(limited, "a single visitor cannot flood the event log");

    t.daemon.state.request_log.flush().await;
    let db = t.daemon.state.store.db();
    let (beacon_requests,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM requests WHERE path LIKE '/_sw/%'")
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(beacon_requests, 0, "the beacon is not counted as traffic");
    let (searches, spam, bots): (i64, i64, i64) = sqlx::query_as(
        "SELECT sum(name = 'search'), sum(name = 'spam'), (SELECT count(*) FROM events WHERE visitor_hash = '') FROM events",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!((searches, spam, bots), (2, 60, 0));

    // The default dashboard shows it straight away, for this site only.
    let admin = Admin::new(&t).await;
    let data = admin.dashboard("hikes").await;
    assert_eq!(data["standard"].as_array().unwrap().len(), 6);
    assert!(data["custom"].as_array().unwrap().is_empty());
    for p in data["standard"].as_array().unwrap() {
        assert!(p["error"].is_null(), "{}: {}", p["id"], p["error"]);
        assert!(
            p["sql"].as_str().unwrap().starts_with("SELECT"),
            "each panel shows its SQL"
        );
    }
    // Four page views (/, /trails/, /trails/rainier, /) by two visitors; the stylesheet,
    // the 404 and the import health check (a bot) are not views.
    assert_eq!(panel(&data, "standard", "views")["rows"][0]["views"], 4);
    assert_eq!(
        panel(&data, "standard", "visitors")["rows"][0]["visitors"],
        2
    );
    let top = panel(&data, "standard", "top-pages")["rows"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(top[0], json!({"path": "/", "views": 2}));
    assert!(
        !top.iter()
            .any(|r| r["path"] == "/css/site.css" || r["path"] == "/nope")
    );
    let classes: Vec<String> = panel(&data, "standard", "status")["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["class"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(classes, ["2xx", "4xx"]);

    // The dashboard page loads the chart libraries from this origin only.
    let page = admin
        .get("/sites/hikes/dashboard")
        .await
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("Design with AI")
            && page.contains("/static/vendor/vega-embed.min.js")
            && !page.contains("https://cdn")
    );
    for asset in [
        "vega.min.js",
        "vega-lite.min.js",
        "vega-embed.min.js",
        "vega-interpreter.min.js",
    ] {
        assert_eq!(
            admin.get(&format!("/static/vendor/{asset}")).await.status(),
            StatusCode::OK,
            "{asset}"
        );
    }
    assert_eq!(
        admin.get("/static/dashboard.js").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        admin.get("/sites/nope/dashboard/data").await.status(),
        StatusCode::NOT_FOUND
    );
    // Without a session the data is not available.
    let anonymous = t.get(ADMIN, "/sites/hikes/dashboard/data").await;
    assert_eq!(anonymous.status(), StatusCode::SEE_OTHER);
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn a_site_that_stops_answering_is_shown_as_down() {
    let t = boot_with("[metrics]\nuptime_interval_secs = 1\nuptime_resolve_local = true\n").await;
    import(&t, "hikes").await;
    let admin = Admin::new(&t).await;

    let wait_for = |needle: &'static str| {
        let admin = &admin;
        async move {
            for _ in 0..80 {
                let home = admin.get("/").await.text().await.unwrap();
                if home.contains(needle) {
                    return home;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            panic!("the home page never showed {needle:?}");
        }
    };
    let home = wait_for("dot up").await;
    assert!(home.contains("100.0% up") && home.contains("views in 7 days"));

    // Break the live release behind the daemon's back: the front page now fails.
    let routes = t.daemon.state.store.routes();
    std::fs::remove_file(routes.get(&host("hikes")).unwrap().root.join("index.html")).unwrap();
    let home = wait_for("dot down").await;
    assert!(!home.contains("100.0% up"));

    let db = t.daemon.state.store.db();
    let (failed, with_status): (i64, i64) =
        sqlx::query_as("SELECT sum(ok = 0), sum(status = 404) FROM checks")
            .fetch_one(db)
            .await
            .unwrap();
    assert!(failed >= 2 && with_status >= 2);
    // Uptime checks are not page views.
    t.daemon.state.request_log.flush().await;
    let (views,): (i64,) = sqlx::query_as("SELECT count(*) FROM requests WHERE ua_class <> 'bot'")
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(views, 0);
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn the_agent_designs_a_dashboard_and_bad_specs_are_refused() {
    let good = json!({"version": 1, "panels": [{
        "id": "trail-searches", "title": "Trail searches per day", "description": "How often visitors search the trail list",
        "sql": "SELECT day, count(*) AS n FROM v_events WHERE site_id = :site_id AND name = 'search' AND ts BETWEEN :from AND :to GROUP BY day",
        "vega_lite": {"mark": "line", "encoding": {"x": {"field": "day", "type": "temporal"}, "y": {"field": "n", "type": "quantitative"}}},
    }]});
    let mut with_data_url = good.clone();
    with_data_url["panels"][0]["vega_lite"]["data"] =
        json!({"url": "https://evil.example/steal.json"});
    let mut forbidden_sql = good.clone();
    forbidden_sql["panels"][0]["sql"] = json!("SELECT token_hash AS day, 1 AS n FROM sessions");
    let mut writes = good.clone();
    writes["panels"][0]["sql"] = json!("DELETE FROM requests");

    let model = mock_model(vec![
        tool_turn("metrics_schema", json!({})),
        tool_turn(
            "run_readonly_query",
            json!({"sql": "SELECT name, count(*) AS n FROM v_events GROUP BY name"}),
        ),
        tool_turn(
            "run_readonly_query",
            json!({"sql": "SELECT * FROM api_tokens"}),
        ),
        tool_turn("save_dashboard", json!({"spec": with_data_url})),
        tool_turn("save_dashboard", json!({"spec": forbidden_sql})),
        tool_turn("save_dashboard", json!({"spec": writes})),
        tool_turn("save_dashboard", json!({"spec": good})),
        text_turn("Saved a dashboard with one panel."),
    ])
    .await;
    let t = boot_with(&format!(
        "{}[metrics]\nuptime_interval_secs = 0\n",
        llm_config(&model)
    ))
    .await;
    write_secrets(&t);
    import(&t, "hikes").await;
    assert_eq!(
        event(
            &t,
            "hikes",
            r#"{"n":"search","p":{"q":"rainier"}}"#,
            "203.0.113.1",
            BROWSER
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let admin = Admin::new(&t).await;

    // The Regenerate button starts an ordinary agent run.
    let started = admin.post("/sites/hikes/dashboard/regenerate", &[]).await;
    let run_url = started.headers()["hx-redirect"]
        .to_str()
        .unwrap()
        .to_owned();
    let events = admin
        .get(&format!("{run_url}/events"))
        .await
        .text()
        .await
        .unwrap();
    let results: Vec<Value> = events
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter(|e| e["type"] == "tool_result")
        .collect();
    let outcome = |i: usize| {
        (
            results[i]["is_error"].as_bool().unwrap(),
            results[i]["content"].as_str().unwrap().to_owned(),
        )
    };
    assert!(
        !outcome(0).0 && outcome(0).1.contains("v_requests"),
        "the schema describes the views"
    );
    assert!(
        !outcome(1).0 && outcome(1).1.contains("\"search\""),
        "exploring the data works"
    );
    assert!(
        outcome(2).0 && outcome(2).1.contains("not allowed"),
        "other tables cannot be read"
    );
    assert!(
        outcome(3).0 && outcome(3).1.contains("\\\"data\\\""),
        "a data URL is rejected: {}",
        outcome(3).1
    );
    assert!(
        outcome(4).0 && outcome(4).1.contains("not allowed"),
        "forbidden SQL is rejected: {}",
        outcome(4).1
    );
    assert!(outcome(5).0, "a write is rejected");
    assert!(!outcome(6).0 && outcome(6).1.contains("\"saved\":true"));
    assert!(events.contains("\"status\":\"succeeded\""));

    // Nothing leaked, nothing was changed, and only the good spec was stored.
    t.daemon.state.request_log.flush().await;
    let db = t.daemon.state.store.db();
    assert!(
        !events.contains(&admin.cookie[20..]),
        "no session token in tool output"
    );
    let (dashboards, requests): (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM dashboards), (SELECT count(*) FROM requests)")
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(dashboards, 1);
    assert!(requests >= 1, "the DELETE did nothing");

    // The site's dashboard now has a panel specific to it, with data.
    let data = admin.dashboard("hikes").await;
    let custom = panel(&data, "custom", "trail-searches");
    assert_eq!(custom["title"], "Trail searches per day");
    assert_eq!(custom["rows"][0]["n"], 1);
    assert!(custom["error"].is_null());
    let page = admin
        .get("/sites/hikes/dashboard")
        .await
        .text()
        .await
        .unwrap();
    assert!(page.contains("Regenerate") && page.contains("Panels chosen by the agent"));
    t.daemon.shutdown().await;
}

const GUESTBOOK: &str = r#"
import { Router, json, db } from "sitewright";
const app = new Router();
app.post("/api/entries", async (req) => {
  const { name } = await req.data();
  await db.run("insert into entries (name, created_at) values (?, ?)", name, Date.now());
  return json({ ok: true }, 201);
});
app.get("/api/entries", async () => json(await db.query("select name from entries order by id")));
export default app;
"#;

/// A dashboard panel can chart rows of the site's own database: read-only, that site
/// only, and a site without a database gets an error instead of a database.
#[tokio::test]
async fn the_agent_charts_a_sites_own_data_read_only() {
    const SECRET: &str = "sk-partner-0123456789abcdef";
    let per_day = "SELECT date(created_at / 1000, 'unixepoch') AS day, count(*) AS n FROM entries \
                   WHERE created_at BETWEEN :from AND :to GROUP BY day ORDER BY day";
    let chart = json!({"mark": "bar", "encoding": {"x": {"field": "day", "type": "temporal"}, "y": {"field": "n", "type": "quantitative"}}});
    let site_panel = |sql: &str| json!({"id": "entries", "title": "Guestbook entries per day", "source": "site", "sql": sql, "vega_lite": chart});
    let spec = |sql: &str| {
        json!({"version": 1, "panels": [
            {"id": "views", "title": "Views per day", "sql": "SELECT day, sum(is_page_view) AS n FROM v_requests WHERE ts BETWEEN :from AND :to GROUP BY day", "vega_lite": chart},
            site_panel(sql),
        ]})
    };
    let site_query =
        |sql: &str| tool_turn("run_readonly_query", json!({"sql": sql, "source": "site"}));

    let model = mock_model(vec![
        // Run 1, on the site with a backend.
        tool_turn("metrics_schema", json!({})),
        site_query(per_day),
        site_query("SELECT name FROM entries ORDER BY id"),
        site_query("DELETE FROM entries"),
        site_query("SELECT count(*) AS n FROM sw_sessions"),
        site_query("SELECT * FROM sw_users"),
        site_query("SELECT count(*) AS n FROM v_requests"),
        site_query("SELECT count(*) AS n FROM entries WHERE :site_id = 1"),
        tool_turn(
            "run_readonly_query",
            json!({"sql": "SELECT count(*) AS n FROM entries"}),
        ),
        tool_turn(
            "run_readonly_query",
            json!({"sql": "SELECT 1 AS n", "source": "main"}),
        ),
        tool_turn(
            "save_dashboard",
            json!({"spec": spec("DELETE FROM entries")}),
        ),
        tool_turn(
            "save_dashboard",
            json!({"spec": spec("SELECT expires_at AS day, 1 AS n FROM sw_sessions")}),
        ),
        tool_turn("save_dashboard", json!({"spec": spec(per_day)})),
        text_turn("Saved a dashboard with the guestbook's entries."),
        // Run 2, on a site without one.
        site_query("SELECT 1 AS n"),
        tool_turn(
            "save_dashboard",
            json!({"spec": spec("SELECT 1 AS day, 2 AS n")}),
        ),
        text_turn("This site has no database."),
    ])
    .await;
    let t = boot_with(&format!(
        "{}[metrics]\nuptime_interval_secs = 0\n",
        llm_config(&model)
    ))
    .await;
    write_secrets(&t);
    import(&t, "hikes").await;
    let dir = t.site_dir(
        "guestbook-src",
        &[
            ("index.html", PAGE),
            ("backend/index.js", GUESTBOOK),
            (
                "backend/migrations/0001_entries.sql",
                "CREATE TABLE entries (id INTEGER PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL);",
            ),
        ],
    );
    t.cli(&["site", "import", "guestbook", path_str(&dir)])
        .await
        .unwrap();
    let state = &t.daemon.state;
    let guestbook = state.store.get("guestbook").await.unwrap();
    let hikes = state.store.get("hikes").await.unwrap();
    state
        .backend
        .set_secret(guestbook.id, "PARTNER_KEY", SECRET)
        .unwrap();
    for name in ["Ada", SECRET] {
        let made = t
            .http
            .post(format!("http://{}/api/entries", t.daemon.addr))
            .header("host", host("guestbook"))
            .json(&json!({"name": name}))
            .send()
            .await
            .unwrap();
        assert_eq!(made.status(), StatusCode::CREATED);
    }
    assert!(state.backend.database_size(&guestbook.name).is_some());
    assert!(state.backend.database_size(&hikes.name).is_none());

    let admin = Admin::new(&t).await;
    let run = |site: &'static str| {
        let admin = &admin;
        async move {
            let started = admin
                .post(&format!("/sites/{site}/dashboard/regenerate"), &[])
                .await;
            let run_url = started.headers()["hx-redirect"]
                .to_str()
                .unwrap()
                .to_owned();
            let events = admin
                .get(&format!("{run_url}/events"))
                .await
                .text()
                .await
                .unwrap();
            assert!(events.contains("\"status\":\"succeeded\""), "{events}");
            assert!(!events.contains(SECRET), "a secret value reached the model");
            events
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .filter_map(|d| serde_json::from_str::<Value>(d).ok())
                .filter(|e| e["type"] == "tool_result")
                .map(|e| {
                    (
                        e["is_error"].as_bool().unwrap(),
                        e["content"].as_str().unwrap().to_owned(),
                    )
                })
                .collect::<Vec<(bool, String)>>()
        }
    };

    let results = run("guestbook").await;
    let refused = |i: usize, why: &str| {
        let (is_error, content) = &results[i];
        assert!(*is_error && content.contains(why), "result {i}: {content}");
    };
    assert!(
        !results[0].0 && results[0].1.contains("\"source\""),
        "the schema names the source field (the event stream shortens the rest): {:?}",
        results[0]
    );
    let per_day_result: Value = serde_json::from_str(&results[1].1).unwrap();
    assert!(!results[1].0, "{}", results[1].1);
    assert_eq!(per_day_result["columns"], json!(["day", "n"]));
    assert_eq!(per_day_result["rows"][0]["n"], 2);
    assert_eq!(per_day_result["row_count"], 1);
    assert!(per_day_result["range"]["from_ms"].as_i64().unwrap() > 1_700_000_000_000);
    let names: Value = serde_json::from_str(&results[2].1).unwrap();
    assert_eq!(
        names["rows"],
        json!([{"name": "Ada"}, {"name": "[secret PARTNER_KEY]"}]),
        "stored secret values are redacted for the agent"
    );
    refused(3, "only SELECT");
    refused(4, "not allowed");
    refused(5, "not allowed");
    refused(6, "v_requests");
    refused(7, ":site_id");
    refused(8, "entries");
    refused(9, "unknown source");
    refused(10, "only SELECT");
    refused(11, "not allowed");
    assert!(
        !results[12].0 && results[12].1.contains("\"saved\":true"),
        "{}",
        results[12].1
    );

    // The owner's dashboard shows the site's rows next to its traffic.
    let data = admin.dashboard("guestbook").await;
    let entries = panel(&data, "custom", "entries");
    assert!(entries["error"].is_null(), "{entries}");
    assert_eq!(entries["source"], "site");
    assert_eq!(entries["rows"].as_array().unwrap().len(), 1);
    assert_eq!(entries["rows"][0]["n"], 2);
    let views = panel(&data, "custom", "views");
    assert!(views["error"].is_null() && views.get("source").is_none());

    // Nothing was written: both entries are still there.
    let listed: Value = t
        .get(&host("guestbook"), "/api/entries")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2);

    // A site without a database: the query and the dashboard are refused...
    let results = run("hikes").await;
    assert!(
        results[0].0 && results[0].1.contains("no database yet"),
        "{}",
        results[0].1
    );
    assert!(
        results[1].0 && results[1].1.contains("no database yet"),
        "{}",
        results[1].1
    );
    // ...and a stored one (say, from before the backend was removed) shows the error on
    // that panel only. Guestbook's panel, stored for hikes, reads hikes' data: none.
    let stored = serde_json::to_string(&spec(per_day)).unwrap();
    sqlx::query("INSERT INTO dashboards (site_id, spec, version, updated_at) VALUES (?, ?, 1, 0)")
        .bind(hikes.id)
        .bind(stored)
        .execute(state.store.db())
        .await
        .unwrap();
    let data = admin.dashboard("hikes").await;
    let entries = panel(&data, "custom", "entries");
    assert!(
        entries["error"]
            .as_str()
            .unwrap()
            .contains("no database yet"),
        "{entries}"
    );
    assert_eq!(entries["rows"], json!([]));
    assert!(panel(&data, "custom", "views")["error"].is_null());
    assert!(
        state.backend.database_size(&hikes.name).is_none(),
        "asking did not create a database"
    );
    t.daemon.shutdown().await;
}
