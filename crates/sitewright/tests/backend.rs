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

#[tokio::test]
async fn secrets_and_hosts_are_managed_by_the_owner_and_only_named_to_the_agent() {
    use common::{llm_config, mock_model, text_turn, tool_turn, write_secrets};

    let model = mock_model(vec![
        tool_turn("backend_access", json!({})),
        tool_turn(
            "request_backend_host",
            json!({"host": "https://api.weather.test/v1", "reason": "x"}),
        ),
        tool_turn(
            "request_backend_host",
            json!({"host": "api.weather.test", "reason": "Forecasts for the front page"}),
        ),
        tool_turn(
            "request_backend_host",
            json!({"host": "api.partner.test", "reason": "Already allowed"}),
        ),
        tool_turn("backend_access", json!({})),
        text_turn("Please approve api.weather.test."),
    ])
    .await;
    let t = common::boot_with(&llm_config(&model)).await;
    write_secrets(&t);
    let dir = t.site_dir("v1", &[("index.html", INDEX)]);
    t.cli(&["site", "import", "shop", path_str(&dir)])
        .await
        .unwrap();
    let admin = t.admin();

    // The owner stores a secret and allows a host.
    let names: Value = admin
        .post(
            "/v1/sites/shop/secrets",
            &json!({"name": "PARTNER_KEY", "value": "pk-live-abcdef123456"}),
        )
        .await
        .unwrap();
    assert_eq!(names, json!(["PARTNER_KEY"]));
    assert!(
        admin
            .post::<Value>(
                "/v1/sites/shop/secrets",
                &json!({"name": "lowercase", "value": "x"})
            )
            .await
            .is_err()
    );
    t.cli(&["backend", "allow-host", "shop", "API.Partner.test"])
        .await
        .unwrap();
    assert!(
        t.cli(&["backend", "allow-host", "shop", "10.0.0.5"])
            .await
            .is_err()
    );
    t.cli(&["backend", "secrets", "shop"]).await.unwrap();

    // The value is on disk in the secrets file only, readable by the owner alone.
    use std::os::unix::fs::PermissionsExt;
    let file = t.daemon.data.site_secrets_file();
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("pk-live-abcdef123456")
    );

    // The agent sees names and hosts, can ask for a host, and cannot approve one.
    t.cli(&["prompt", "shop", "show the weather"])
        .await
        .unwrap();
    let results = tool_results(&t, 1).await;
    let outcome: Vec<bool> = results.iter().map(|(_, error, _)| *error).collect();
    assert_eq!(outcome, [false, true, false, false, false], "{results:#?}");
    assert_eq!(results[0].2["secrets"], json!(["PARTNER_KEY"]));
    assert_eq!(results[2].2["approved"], false);
    assert_eq!(results[3].2["approved"], true);
    let hosts = &results[4].2["hosts"];
    assert_eq!(
        hosts,
        &json!([
            {"host": "api.partner.test", "approved": true, "reason": null},
            {"host": "api.weather.test", "approved": false, "reason": "Forecasts for the front page"},
        ])
    );
    let everything = serde_json::to_string(&results).unwrap();
    assert!(!everything.contains("pk-live-abcdef123456"));

    // Removing works, and the lists are per site.
    let hosts: Value = admin
        .delete(
            "/v1/sites/shop/backend-hosts",
            &json!({"host": "api.weather.test"}),
        )
        .await
        .unwrap();
    assert_eq!(hosts.as_array().unwrap().len(), 1);
    let names: Value = admin
        .delete("/v1/sites/shop/secrets", &json!({"name": "PARTNER_KEY"}))
        .await
        .unwrap();
    assert_eq!(names, json!([]));
    assert!(
        admin
            .delete::<Value>("/v1/sites/shop/secrets", &json!({"name": "PARTNER_KEY"}))
            .await
            .is_err()
    );

    t.daemon.shutdown().await;
}

#[tokio::test]
async fn a_site_is_containerized_at_creation_or_later_and_packaged_without_secrets() {
    use common::{llm_config, mock_model, text_turn, tool_turn, write_secrets};

    let model = mock_model(vec![
        tool_turn(
            "create_site",
            json!({"name": "shop", "title": "Corner Shop"}),
        ),
        tool_turn(
            "write_file",
            json!({"path": "index.html", "content": INDEX}),
        ),
        tool_turn(
            "write_file",
            json!({"path": "backend/index.js", "content": BACKEND}),
        ),
        tool_turn(
            "write_file",
            json!({"path": "backend/migrations/0001_entries.sql", "content": MIGRATION}),
        ),
        tool_turn("publish_site", json!({"message": "Shop"})),
        text_turn("Done."),
    ])
    .await;
    let t = common::boot_with(&llm_config(&model)).await;
    write_secrets(&t);
    let admin = t.admin();
    let store = &t.daemon.state.store;

    // Chosen at creation: the run that creates the site marks it.
    let run: Value = admin
        .post(
            "/v1/runs",
            &json!({"site": "shop", "prompt": "a shop", "new": true, "containerize": true}),
        )
        .await
        .unwrap();
    let id = run["id"].as_i64().unwrap();
    tool_results(&t, id).await; // reads the stream to its end
    assert!(store.containerized("shop").await.unwrap());

    // Chosen later, and undone again.
    let dir = t.site_dir("plain", &[("index.html", INDEX)]);
    t.cli(&["site", "import", "plain", path_str(&dir)])
        .await
        .unwrap();
    assert!(!store.containerized("plain").await.unwrap());
    t.cli(&["site", "containerize", "plain"]).await.unwrap();
    assert!(store.containerized("plain").await.unwrap());
    t.cli(&["site", "containerize", "plain", "--off"])
        .await
        .unwrap();
    assert!(!store.containerized("plain").await.unwrap());

    // The shop uses a secret, an outside host and an AI model.
    let _: Value = admin
        .post(
            "/v1/sites/shop/secrets",
            &json!({"name": "PAY_KEY", "value": "pk-live-topsecret-123"}),
        )
        .await
        .unwrap();
    t.cli(&["backend", "allow-host", "shop", "api.pay.test"])
        .await
        .unwrap();
    t.cli(&["backend", "ai", "shop", "--enable", "--monthly", "250"])
        .await
        .unwrap();

    // The guide names every variable and holds no value.
    let guide = String::from_utf8(
        admin
            .get_bytes("/v1/sites/shop/container/guide")
            .await
            .unwrap(),
    )
    .unwrap();
    for expected in [
        "# Running Corner Shop in a container",
        "docker load -i shop-image.tar",
        "| `PAY_KEY` |",
        "| `LLM_MOCK_API_KEY` |",
        "--env-file shop.env",
        "-v shop-data:/data",
        "- `api.pay.test`",
        "250 per month",
        "import-data /in/shop-data.db",
    ] {
        assert!(
            guide.contains(expected),
            "missing {expected:?} in:\n{guide}"
        );
    }
    assert!(!guide.contains("pk-live-topsecret-123"));

    // The image: a docker archive whose site layer has the files, the manifest and the
    // guide, and no secret anywhere.
    let out = t.dir.path().join("out");
    t.cli(&["site", "image", "shop", "-o", path_str(&out)])
        .await
        .unwrap();
    let image = std::fs::read(out.join("shop-image.tar")).unwrap();
    assert!(out.join("shop-SETUP.md").is_file() && out.join("shop-data.db").is_file());
    let contains = |needle: &[u8]| image.windows(needle.len()).any(|w| w == needle);
    assert!(contains(b"manifest.json") && contains(b"sitewright-site/shop:"));
    assert!(contains(b"site/backend/index.js") && contains(b"site/index.html"));
    assert!(contains(br#""secrets": ["#) || contains(b"\"PAY_KEY\""));
    assert!(contains(b"LLM_MOCK_API_KEY"), "the guide is in the image");
    assert!(!contains(b"pk-live-topsecret-123"));
    // The provider key from secrets.toml is not in it either.
    let provider_key = std::fs::read_to_string(t.daemon.data.secrets_file()).unwrap();
    let key = provider_key.split('"').nth(1).unwrap();
    assert!(
        key.len() >= 6 && !contains(key.as_bytes()),
        "a provider key is in the image"
    );

    // A site that was never published has nothing to package.
    t.cli(&["site", "create", "empty"]).await.unwrap();
    assert!(
        admin
            .get_bytes("/v1/sites/empty/container/image")
            .await
            .is_err()
    );
    // A static site packages too, with no data and no secrets section.
    let guide = String::from_utf8(
        admin
            .get_bytes("/v1/sites/plain/container/guide")
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(guide.contains("This site needs no secrets.") && !guide.contains("/data"));
    assert!(
        admin
            .get_bytes("/v1/sites/plain/container/data")
            .await
            .is_err()
    );

    t.daemon.shutdown().await;
}
