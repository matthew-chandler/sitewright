//! Secrets and outbound requests of site backends, through real workers.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use sw_backend::{BackendRequest, BackendSource, Egress, SiteBackends, WorkerCommand};
use sw_core::config::BackendsConfig;
use sw_core::{Config, DataDir, SiteName};
use sw_sites::{Author, SiteStore};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Env {
    _dir: tempfile::TempDir,
    backends: SiteBackends,
    db: sqlx::SqlitePool,
    site_id: i64,
    other_id: i64,
    name: SiteName,
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
    let site = store.create("shop", "Shop", Author::User).await.unwrap();
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
        _dir: dir,
        backends,
        db,
        site_id: site.id,
        other_id: other.id,
        name: site.name,
    }
}

const APP: &str = r#"
import { Router, json, secrets, fetch } from "sitewright";
const app = new Router();
app.post("/api/call", async (req) => {
  const { url } = await req.json();
  const key = await secrets.get("SHOP_API_KEY");
  const res = await fetch(url, { method: "POST", headers: { authorization: `Bearer ${key}`, host: "evil.test" }, body: { hello: "world" } });
  console.log("called with", key);
  return json({ status: res.status, ok: res.ok, type: res.headers.get("content-type"), data: await res.json(), echo: key });
});
app.get("/api/missing", async () => json({ v: await secrets.get("NOPE") }));
export default app;
"#;

fn source() -> BackendSource {
    BackendSource {
        files: [("backend/index.js".to_owned(), APP.to_owned())].into(),
        migrations: Vec::new(),
    }
}

fn call(url: &str) -> BackendRequest {
    BackendRequest {
        method: "POST".into(),
        path: "/api/call".into(),
        query: String::new(),
        headers: vec![("content-type".into(), "application/json".into())],
        body: json!({ "url": url }).to_string(),
    }
}

#[tokio::test]
async fn a_backend_uses_a_secret_to_call_an_approved_host_and_nothing_else() {
    let env = env().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/orders"))
        .and(header("authorization", "Bearer sk-shop-123456"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"order": 7})))
        .mount(&server)
        .await;
    let url = format!("{}/v1/orders", server.uri());
    let host = "127.0.0.1";
    let b = &env.backends;
    let run = |request| b.test_draft(env.site_id, &env.name, source(), request, false);

    // No secret yet: a clear error, in the log, and a 500 for the caller.
    let out = run(call(&url)).await.unwrap();
    assert_eq!(out.status, 500);
    assert!(
        out.logs
            .iter()
            .any(|l| l.message.contains("no secret named SHOP_API_KEY"))
    );

    b.set_secret(env.site_id, "SHOP_API_KEY", "sk-shop-123456")
        .unwrap();
    b.set_secret(env.other_id, "SHOP_API_KEY", "the-other-sites-key")
        .unwrap();
    assert_eq!(b.secret_names(env.site_id).unwrap(), ["SHOP_API_KEY"]);

    // The host is not approved: refused, and the owner is asked.
    let out = run(call(&url)).await.unwrap();
    assert_eq!(out.status, 500);
    assert!(out.logs.iter().any(|l| {
        l.message
            .contains("not on this site's list of approved hosts")
    }));
    let hosts = b.hosts(env.site_id).await.unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!((hosts[0].host.as_str(), hosts[0].approved), (host, false));
    // Still refused while it waits; nothing reached the server.
    let out = run(call(&url)).await.unwrap();
    assert!(
        out.logs
            .iter()
            .any(|l| l.message.contains("waiting for the site owner's approval"))
    );
    assert!(server.received_requests().await.unwrap().is_empty());

    // Approved for this site only. (The test server is an IP address, which the
    // public API rightly refuses as a host name, so approve it in the table.)
    b.approve_host(env.site_id, host).await.unwrap_err();
    sqlx::query("UPDATE site_hosts SET status = 'approved' WHERE site_id = ?")
        .bind(env.site_id)
        .execute(&env.db)
        .await
        .unwrap();
    let out = run(call(&url)).await.unwrap();
    assert_eq!(out.status, 200, "{out:?}");
    let body: Value = serde_json::from_str(&out.body).unwrap();
    assert_eq!(body["status"], 201);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"], json!({"order": 7}));
    // The secret's value reached the service, but not this (agent-visible) outcome.
    assert_eq!(body["echo"], "[secret SHOP_API_KEY]");
    assert!(!out.body.contains("sk-shop-123456"));
    assert!(
        out.logs
            .iter()
            .any(|l| l.message == "called with [secret SHOP_API_KEY]")
    );
    assert!(
        b.logs_redacted(env.site_id, 50)
            .iter()
            .all(|l| !l.message.contains("sk-shop-123456"))
    );
    // The backend could not override the Host header.
    let received = &server.received_requests().await.unwrap()[0];
    assert_ne!(received.headers["host"], "evil.test");
    assert_eq!(received.headers["content-type"], "application/json");

    // The other site has neither this host nor this secret.
    assert!(b.hosts(env.other_id).await.unwrap().is_empty());
    assert!(b.remove_host(env.site_id, host).await.unwrap());
    let out = run(call(&url)).await.unwrap();
    assert_eq!(out.status, 500);
}

#[tokio::test]
async fn host_lists_take_names_only() {
    let env = env().await;
    let b = &env.backends;
    assert_eq!(
        b.request_host(env.site_id, "API.Example.com.", "weather data")
            .await
            .unwrap(),
        "api.example.com"
    );
    for bad in [
        "10.0.0.1",
        "localhost",
        "https://api.example.com",
        "*.example.com",
        "a/b",
    ] {
        assert!(b.request_host(env.site_id, bad, "").await.is_err(), "{bad}");
        assert!(b.approve_host(env.site_id, bad).await.is_err(), "{bad}");
    }
    assert_eq!(
        b.approve_host(env.site_id, "api.example.com")
            .await
            .unwrap(),
        "api.example.com"
    );
    let hosts = b.hosts(env.site_id).await.unwrap();
    assert_eq!(hosts.len(), 1);
    assert!(hosts[0].approved);
    assert_eq!(hosts[0].reason.as_deref(), Some("weather data"));
    // Asking again does not downgrade an approved host.
    b.request_host(env.site_id, "api.example.com", "again")
        .await
        .unwrap();
    assert!(b.hosts(env.site_id).await.unwrap()[0].approved);
}

const ACCOUNTS: &str = r#"
import { Router, json, auth, db } from "sitewright";
const app = new Router();
app.post("/api/signup", async (req) => {
  const { user, cookie } = await auth.signUp(await req.json());
  return json({ user }, 201, { "set-cookie": cookie });
});
app.post("/api/signin", async (req) => {
  const { user, cookie } = await auth.signIn(await req.json());
  return json({ user }, 200, { "set-cookie": cookie });
});
app.post("/api/signout", async (req) => {
  const { cookie } = await auth.signOut(req);
  return json({ ok: true }, 200, { "set-cookie": cookie });
});
app.get("/api/me", async (req) => json({ user: await auth.user(req) }));
app.get("/api/notes", async (req) => {
  const user = await auth.requireUser(req);
  return json(await db.query("select body from notes where user_id = ? order by id", user.id));
});
app.post("/api/notes", async (req) => {
  const user = await auth.requireUser(req);
  const { body } = await req.json();
  await db.run("insert into notes (user_id, body) values (?, ?)", user.id, body);
  return json({ ok: true }, 201);
});
export default app;
"#;

fn accounts() -> BackendSource {
    BackendSource {
        files: [("backend/index.js".to_owned(), ACCOUNTS.to_owned())].into(),
        migrations: vec![sw_backend::Migration {
            name: "0001_notes.sql".into(),
            sql: "CREATE TABLE notes (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES sw_users (id), body TEXT NOT NULL);".into(),
        }],
    }
}

fn req(method: &str, path: &str, body: Value, cookie: Option<&str>) -> BackendRequest {
    let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    if let Some(cookie) = cookie {
        headers.push(("cookie".to_owned(), cookie.to_owned()));
    }
    BackendRequest {
        method: method.into(),
        path: path.into(),
        query: String::new(),
        headers,
        body: if body.is_null() {
            String::new()
        } else {
            body.to_string()
        },
    }
}

#[tokio::test]
async fn visitors_sign_up_sign_in_and_see_only_their_own_data() {
    let env = env().await;
    let b = &env.backends;
    let run = |r| async {
        b.test_draft(env.site_id, &env.name, accounts(), r, false)
            .await
            .unwrap()
    };
    let cookie_of = |out: &sw_backend::DraftOutcome| {
        let set = &out
            .headers
            .iter()
            .find(|(k, _)| k == "set-cookie")
            .unwrap()
            .1;
        set.split(';').next().unwrap().to_owned()
    };

    assert_eq!(
        run(req("GET", "/api/notes", Value::Null, None))
            .await
            .status,
        401
    );
    let ada = run(req(
        "POST",
        "/api/signup",
        json!({"email": "ada@example.com", "password": "correct horse"}),
        None,
    ))
    .await;
    assert_eq!(ada.status, 201, "{ada:?}");
    let ada_cookie = cookie_of(&ada);
    assert!(ada_cookie.starts_with("__Host-sw_session="));
    let bo = run(req(
        "POST",
        "/api/signup",
        json!({"email": "bo@example.com", "password": "battery staple"}),
        None,
    ))
    .await;
    let bo_cookie = cookie_of(&bo);

    // Visitor-facing errors arrive with their status and message.
    let dup = run(req(
        "POST",
        "/api/signup",
        json!({"email": "ADA@example.com", "password": "something else"}),
        None,
    ))
    .await;
    assert_eq!(dup.status, 409);
    let weak = run(req(
        "POST",
        "/api/signup",
        json!({"email": "c@example.com", "password": "x"}),
        None,
    ))
    .await;
    assert_eq!(
        (weak.status, weak.body.contains("at least 8 characters")),
        (400, true)
    );
    let wrong = run(req(
        "POST",
        "/api/signin",
        json!({"email": "ada@example.com", "password": "nope nope"}),
        None,
    ))
    .await;
    assert_eq!(wrong.status, 401);

    run(req(
        "POST",
        "/api/notes",
        json!({"body": "ada's note"}),
        Some(&ada_cookie),
    ))
    .await;
    run(req(
        "POST",
        "/api/notes",
        json!({"body": "bo's note"}),
        Some(&bo_cookie),
    ))
    .await;
    let mine = run(req("GET", "/api/notes", Value::Null, Some(&ada_cookie))).await;
    assert_eq!(mine.body, r#"[{"body":"ada's note"}]"#);
    let me: Value = serde_json::from_str(
        &run(req("GET", "/api/me", Value::Null, Some(&bo_cookie)))
            .await
            .body,
    )
    .unwrap();
    assert_eq!(me["user"]["email"], "bo@example.com");
    assert!(me["user"].get("password_hash").is_none());
    // A made-up cookie is nobody.
    let forged = format!("__Host-sw_session={}", "a".repeat(64));
    assert_eq!(
        run(req("GET", "/api/notes", Value::Null, Some(&forged)))
            .await
            .status,
        401
    );

    // Signing out ends that session only.
    let out = run(req("POST", "/api/signout", Value::Null, Some(&ada_cookie))).await;
    assert!(
        out.headers
            .iter()
            .any(|(k, v)| k == "set-cookie" && v.contains("Max-Age=0"))
    );
    assert_eq!(
        run(req("GET", "/api/notes", Value::Null, Some(&ada_cookie)))
            .await
            .status,
        401
    );
    assert_eq!(
        run(req("GET", "/api/notes", Value::Null, Some(&bo_cookie)))
            .await
            .status,
        200
    );
    let back = run(req(
        "POST",
        "/api/signin",
        json!({"email": "Ada@Example.com", "password": "correct horse"}),
        None,
    ))
    .await;
    assert_eq!(back.status, 200);
    assert_ne!(cookie_of(&back), ada_cookie);
}
