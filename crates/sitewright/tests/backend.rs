//! Site backends end to end: publish a site with a backend, call its API over HTTP.
#![allow(clippy::unwrap_used)]

mod common;

use common::{boot, host, path_str};
use serde_json::{Value, json};

const INDEX: &str = "<!doctype html><html lang=\"en\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Guestbook</title><h1>Guestbook</h1>";

const BACKEND: &str = r#"
import { Router, json, db, HttpError } from "sitewright";
const app = new Router();
app.get("/api/entries", async () => json(await db.query("select id, name from entries order by id")));
app.post("/api/entries", async (req) => {
  const { name } = await req.data();
  if (!name) throw new HttpError(400, "name is required");
  const { lastInsertRowid } = await db.run("insert into entries (name) values (?)", name);
  console.log("signed", name);
  return json({ id: lastInsertRowid }, 201);
});
app.get("/api/whoami", (req) => json({
  visitor: req.headers.get("x-sitewright-visitor"),
  forwarded: req.headers.get("x-forwarded-for"),
  host: req.headers.get("host"),
}));
export default app;
"#;

const MIGRATION: &str = "CREATE TABLE entries (id INTEGER PRIMARY KEY, name TEXT NOT NULL);";

async fn post_json(
    t: &common::TestDaemon,
    site: &str,
    path: &str,
    body: Value,
) -> reqwest::Response {
    t.http
        .post(format!("http://{}{path}", t.daemon.addr))
        .header("host", host(site))
        .header("x-forwarded-for", "203.0.113.9")
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_published_backend_answers_api_requests_and_keeps_its_data() {
    let t = boot().await;
    let v1 = t.site_dir(
        "v1",
        &[
            ("index.html", INDEX),
            ("backend/index.js", BACKEND),
            ("backend/migrations/0001_entries.sql", MIGRATION),
        ],
    );
    t.cli(&["site", "import", "guestbook", path_str(&v1)])
        .await
        .unwrap();

    // Static files as before; the backend's source is never served.
    assert_eq!(t.get(&host("guestbook"), "/").await.status(), 200);
    for hidden in [
        "/backend/index.js",
        "/backend/migrations/0001_entries.sql",
        "/backend",
        "/backend/",
    ] {
        assert_eq!(
            t.get(&host("guestbook"), hidden).await.status(),
            404,
            "{hidden}"
        );
    }

    // The API.
    let empty = t.get(&host("guestbook"), "/api/entries").await;
    assert_eq!(empty.status(), 200);
    assert_eq!(empty.headers()["cache-control"], "no-store");
    assert_eq!(empty.headers()["x-content-type-options"], "nosniff");
    assert_eq!(empty.json::<Value>().await.unwrap(), json!([]));
    let made = post_json(&t, "guestbook", "/api/entries", json!({"name": "Ada"})).await;
    assert_eq!(made.status(), 201);
    assert_eq!(made.json::<Value>().await.unwrap(), json!({"id": 1}));
    let bad = post_json(&t, "guestbook", "/api/entries", json!({})).await;
    assert_eq!(bad.status(), 400);
    // A form post works too.
    let form = t
        .http
        .post(format!("http://{}/api/entries", t.daemon.addr))
        .header("host", host("guestbook"))
        .form(&[("name", "Bo & Co")])
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), 201);
    let listed: Value = t
        .get(&host("guestbook"), "/api/entries")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed,
        json!([{"id": 1, "name": "Ada"}, {"id": 2, "name": "Bo & Co"}])
    );
    assert_eq!(
        t.get(&host("guestbook"), "/api/missing").await.status(),
        404
    );

    // The backend sees a visitor hash, not an address.
    assert_eq!(
        post_json(&t, "guestbook", "/api/whoami", json!({}))
            .await
            .status(),
        405,
        "POST is not routed to a GET handler"
    );
    let who: Value = t
        .http
        .get(format!("http://{}/api/whoami", t.daemon.addr))
        .header("host", host("guestbook"))
        .header("x-forwarded-for", "203.0.113.9")
        .header("x-sitewright-visitor", "spoofed")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(who["forwarded"], Value::Null);
    assert_eq!(who["host"], host("guestbook"));
    let visitor = who["visitor"].as_str().unwrap();
    assert!(visitor.len() >= 16 && visitor != "spoofed", "{visitor}");

    // Console output is kept for the portal and the agent.
    let site = t.daemon.state.store.get("guestbook").await.unwrap();
    let logs = t.daemon.state.backend.logs(site.id, 10);
    assert!(
        logs.iter().any(|l| l.message == "signed Ada" && !l.draft),
        "{logs:?}"
    );

    // A release whose code does not load is refused; the old one keeps serving.
    let broken = t.site_dir(
        "broken",
        &[
            ("index.html", INDEX),
            ("backend/index.js", "export default {"),
            ("backend/migrations/0001_entries.sql", MIGRATION),
        ],
    );
    let refused = t
        .cli(&["site", "import", "guestbook", path_str(&broken)])
        .await
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("failed to load"),
        "{refused:#}"
    );
    // So is one whose migration fails, or rewrites history; the data is untouched.
    for (name, sql) in [
        (
            "0002_bad.sql",
            "CREATE TABLE half (a); CREATE TABLE entries (dup);",
        ),
        (
            "0001_entries.sql",
            "CREATE TABLE entries (id INTEGER PRIMARY KEY);",
        ),
    ] {
        let mut files = vec![("index.html", INDEX), ("backend/index.js", BACKEND)];
        if name != "0001_entries.sql" {
            files.push(("backend/migrations/0001_entries.sql", MIGRATION));
        }
        let path = format!("backend/migrations/{name}");
        files.push((path.as_str(), sql));
        let dir = t.site_dir(name, &files);
        assert!(
            t.cli(&["site", "import", "guestbook", path_str(&dir)])
                .await
                .is_err(),
            "{name}"
        );
    }
    let listed: Value = t
        .get(&host("guestbook"), "/api/entries")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2);

    // A second release adds a column; the data carries over.
    let v2 = t.site_dir(
        "v2",
        &[
            ("index.html", INDEX),
            (
                "backend/index.js",
                &BACKEND.replace("select id, name from", "select id, name, likes from"),
            ),
            ("backend/migrations/0001_entries.sql", MIGRATION),
            (
                "backend/migrations/0002_likes.sql",
                "ALTER TABLE entries ADD COLUMN likes INTEGER NOT NULL DEFAULT 0;",
            ),
        ],
    );
    t.cli(&["site", "import", "guestbook", path_str(&v2)])
        .await
        .unwrap();
    let listed: Value = t
        .get(&host("guestbook"), "/api/entries")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed[1], json!({"id": 2, "name": "Bo & Co", "likes": 0}));

    // Renaming moves the database with the site.
    t.cli(&["site", "rename", "guestbook", "visitors"])
        .await
        .unwrap();
    let listed: Value = t
        .get(&host("visitors"), "/api/entries")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(
        t.get(&host("guestbook"), "/api/entries").await.status(),
        308
    );

    // A site without a backend treats /api like any other path.
    let plain = t.site_dir("plain", &[("index.html", INDEX)]);
    t.cli(&["site", "import", "plain", path_str(&plain)])
        .await
        .unwrap();
    assert_eq!(t.get(&host("plain"), "/api/entries").await.status(), 404);
    assert_eq!(
        post_json(&t, "plain", "/api/entries", json!({}))
            .await
            .status(),
        405
    );

    t.daemon.shutdown().await;
}

#[tokio::test]
async fn backends_can_be_turned_off() {
    let t = common::boot_with("[backends]\nenabled = false\n").await;
    let dir = t.site_dir(
        "v1",
        &[("index.html", INDEX), ("backend/index.js", BACKEND)],
    );
    let refused = t
        .cli(&["site", "import", "guestbook", path_str(&dir)])
        .await
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("backends are turned off"),
        "{refused:#}"
    );
    t.daemon.shutdown().await;
}

/// The tool results of a finished run, by tool name, in order.
async fn tool_results(t: &common::TestDaemon, run: i64) -> Vec<(String, bool, Value)> {
    let body = t
        .admin()
        .stream(&format!("/v1/runs/{run}/events"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter(|v| v["type"] == "tool_result")
        .map(|v| {
            (
                v["name"].as_str().unwrap().to_owned(),
                v["is_error"].as_bool().unwrap(),
                serde_json::from_str(v["content"].as_str().unwrap()).unwrap_or(Value::Null),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_agent_builds_tests_and_publishes_a_backend() {
    use common::{llm_config, mock_model, text_turn, tool_turn, write_secrets};

    let model = mock_model(vec![
        tool_turn(
            "create_site",
            json!({"name": "guestbook", "title": "Guestbook"}),
        ),
        tool_turn(
            "write_file",
            json!({"path": "index.html", "content": INDEX}),
        ),
        // SQL belongs in migrations, and migrations have a naming rule.
        tool_turn(
            "write_file",
            json!({"path": "schema.sql", "content": MIGRATION}),
        ),
        tool_turn(
            "write_file",
            json!({"path": "backend/migrations/entries.sql", "content": MIGRATION}),
        ),
        tool_turn(
            "write_file",
            json!({"path": "backend/migrations/0001_entries.sql", "content": MIGRATION}),
        ),
        // Nothing to test before the entry point exists.
        tool_turn(
            "test_backend",
            json!({"method": "GET", "path": "/api/entries"}),
        ),
        tool_turn(
            "write_file",
            json!({"path": "backend/index.js", "content": BACKEND}),
        ),
        tool_turn(
            "test_backend",
            json!({"method": "POST", "path": "/api/entries", "body": "{\"name\":\"Draft Dan\"}"}),
        ),
        tool_turn(
            "test_backend",
            json!({"method": "GET", "path": "/api/entries"}),
        ),
        tool_turn(
            "test_backend",
            json!({"method": "GET", "path": "/elsewhere"}),
        ),
        tool_turn("backend_schema", json!({})),
        tool_turn(
            "publish_site",
            json!({"message": "Guestbook with a backend"}),
        ),
        tool_turn("backend_schema", json!({})),
        tool_turn("backend_logs", json!({})),
        text_turn("Done."),
    ])
    .await;
    let t = common::boot_with(&llm_config(&model)).await;
    write_secrets(&t);
    t.cli(&["prompt", "--new", "guestbook", "a guestbook"])
        .await
        .unwrap();

    let results = tool_results(&t, 1).await;
    let outcome: Vec<(&str, bool)> = results.iter().map(|(n, e, _)| (n.as_str(), *e)).collect();
    assert_eq!(
        outcome,
        [
            ("create_site", false),
            ("write_file", false),
            ("write_file", true),
            ("write_file", true),
            ("write_file", false),
            ("test_backend", true),
            ("write_file", false),
            ("test_backend", false),
            ("test_backend", false),
            ("test_backend", true),
            ("backend_schema", false),
            ("publish_site", false),
            ("backend_schema", false),
            ("backend_logs", false),
        ],
        "{results:#?}"
    );
    // The draft ran against its own database: the POST was visible to the next test...
    let posted = &results[7].2;
    assert_eq!(posted["status"], 201);
    assert_eq!(posted["migrations_applied"], json!(["0001_entries.sql"]));
    assert_eq!(posted["console"], json!(["[info] signed Draft Dan"]));
    let listed = &results[8].2;
    assert!(listed["body"].as_str().unwrap().contains("Draft Dan"));
    assert_eq!(listed["migrations_applied"], json!([]));
    // ...the live database did not exist before publishing, and has the table after.
    assert_eq!(results[10].2["objects"], json!([]));
    assert_eq!(results[12].2["objects"][0]["name"], "entries");
    // ...and the draft's data never reached the live site.
    let live: Value = t
        .get(&host("guestbook"), "/api/entries")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(live, json!([]));
    assert!(
        results[13].2["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["draft"] == true)
    );

    t.daemon.shutdown().await;
}
