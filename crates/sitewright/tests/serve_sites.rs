//! M1 end to end: import a site through the CLI, serve it by hostname, roll it back.
#![allow(clippy::unwrap_used)]

mod common;

use common::{ADMIN, boot, host, path_str};
use serde_json::Value;

#[tokio::test]
async fn unknown_hosts_get_a_plain_404() {
    let t = boot().await;
    for h in ["nope.sites.test", "example.org", "sites.test"] {
        let res = t.get(h, "/").await;
        assert_eq!(res.status(), 404, "{h}");
        assert!(
            res.text()
                .await
                .unwrap()
                .contains("no site at this address")
        );
    }
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn admin_host_answers_and_is_not_a_site() {
    let t = boot().await;
    let res = t.get(ADMIN, "/").await;
    assert_eq!(res.status(), 200);
    assert!(res.text().await.unwrap().contains("Sitewright"));
    assert_eq!(
        t.get(&format!("{ADMIN}:8443"), "/healthz").await.status(),
        200,
        "port in Host is ignored"
    );
    // The admin label can never become a site.
    assert!(t.cli(&["site", "create", "admin"]).await.is_err());
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn import_serve_reimport_and_rollback() {
    let t = boot().await;
    let v1 = t.site_dir(
        "v1",
        &[
            (
                "index.html",
                "<!doctype html><title>Hikes</title><h1>Version one</h1>",
            ),
            ("about.html", "<!doctype html><title>About</title>about"),
            (
                "trails/index.html",
                "<!doctype html><title>Trails</title>trails",
            ),
            ("css/site.css", "body { margin: 0 }"),
            (
                "404.html",
                "<!doctype html><title>Lost</title>custom not found",
            ),
            (".secret", "hidden"),
        ],
    );
    t.cli(&[
        "site",
        "import",
        "hikes",
        path_str(&v1),
        "--title",
        "My Hikes",
    ])
    .await
    .unwrap();
    let h = host("hikes");

    // Index, with the headers every site response carries.
    let res = t.get(&h, "/").await;
    assert_eq!(res.status(), 200);
    let headers = res.headers().clone();
    assert_eq!(headers["content-type"], "text/html");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(
        headers["referrer-policy"],
        "strict-origin-when-cross-origin"
    );
    assert_eq!(headers["content-security-policy"], "frame-ancestors 'self'");
    assert_eq!(headers["cache-control"], "no-cache");
    assert!(headers.contains_key("last-modified"));
    assert!(res.text().await.unwrap().contains("Version one"));

    // Assets, directories, clean URLs.
    let css = t.get(&h, "/css/site.css").await;
    assert_eq!(css.status(), 200);
    assert_eq!(css.headers()["content-type"], "text/css");
    assert!(
        css.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("max-age=300")
    );
    assert_eq!(
        t.get(&h, "/trails/").await.text().await.unwrap(),
        "<!doctype html><title>Trails</title>trails"
    );
    assert!(t.get(&h, "/trails").await.status().is_redirection());
    assert!(
        t.get(&h, "/about")
            .await
            .text()
            .await
            .unwrap()
            .contains("about")
    );

    // Misses use the site's 404.html; hidden files and traversal are never served.
    let missing = t.get(&h, "/nope").await;
    assert_eq!(missing.status(), 404);
    assert!(missing.text().await.unwrap().contains("custom not found"));
    assert_eq!(t.get(&h, "/.secret").await.status(), 404);
    assert_eq!(t.get(&h, "/..%2f..%2fdb/sitewright.db").await.status(), 404);
    assert_eq!(t.get(&h, "/%2e%2e/%2e%2e/config.toml").await.status(), 404);

    // Conditional requests and methods.
    let last_modified = headers["last-modified"].clone();
    let res = t
        .http
        .get(format!("http://{}/", t.daemon.addr))
        .header("host", &h)
        .header("if-modified-since", last_modified)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 304);
    let res = t
        .http
        .post(format!("http://{}/", t.daemon.addr))
        .header("host", &h)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 405);
    let res = t
        .http
        .head(format!("http://{}/", t.daemon.addr))
        .header("host", &h)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // Compression is negotiated.
    let res = t
        .http
        .get(format!("http://{}/", t.daemon.addr))
        .header("host", &h)
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.headers()
            .get("content-encoding")
            .map(|v| v.to_str().unwrap()),
        Some("gzip")
    );

    // A second import replaces the content at once; rollback switches back at once.
    let v2 = t.site_dir(
        "v2",
        &[(
            "index.html",
            "<!doctype html><title>Hikes</title><h1>Version two</h1>",
        )],
    );
    t.cli(&["site", "import", "hikes", path_str(&v2), "-m", "second"])
        .await
        .unwrap();
    assert!(
        t.get(&h, "/")
            .await
            .text()
            .await
            .unwrap()
            .contains("Version two")
    );
    assert_eq!(
        t.get(&h, "/about").await.status(),
        404,
        "about.html is gone in v2"
    );

    t.cli(&["site", "rollback", "hikes"]).await.unwrap();
    assert!(
        t.get(&h, "/")
            .await
            .text()
            .await
            .unwrap()
            .contains("Version one")
    );
    assert_eq!(t.get(&h, "/about").await.status(), 200);

    let info: Value = t.admin().get("/v1/sites/hikes").await.unwrap();
    assert_eq!(info["site"]["title"], "My Hikes");
    assert_eq!(info["url"], "http://hikes.sites.test");
    assert_eq!(info["releases"].as_array().unwrap().len(), 2);

    t.cli(&["site", "list"]).await.unwrap();
    t.cli(&["site", "info", "hikes"]).await.unwrap();
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn delete_needs_the_exact_name_and_restore_brings_it_back() {
    let t = boot().await;
    let dir = t.site_dir("s", &[("index.html", "hello")]);
    t.cli(&["site", "import", "blog", path_str(&dir)])
        .await
        .unwrap();
    let h = host("blog");
    assert_eq!(t.get(&h, "/").await.status(), 200);

    let wrong: anyhow::Result<Value> = t
        .admin()
        .post(
            "/v1/sites/blog/delete",
            &serde_json::json!({ "confirm": "blo" }),
        )
        .await;
    assert!(wrong.is_err());
    assert_eq!(t.get(&h, "/").await.status(), 200);

    t.cli(&["site", "delete", "blog", "--yes"]).await.unwrap();
    assert_eq!(t.get(&h, "/").await.status(), 404);
    t.cli(&["site", "restore", "blog"]).await.unwrap();
    assert_eq!(t.get(&h, "/").await.text().await.unwrap(), "hello");
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn requests_are_logged_without_ip_addresses() {
    let t = boot().await;
    let dir = t.site_dir("s", &[("index.html", "hello")]);
    t.cli(&["site", "import", "log", path_str(&dir)])
        .await
        .unwrap();
    let h = host("log");
    let ua = "Mozilla/5.0 (Linux; Android 14) Chrome/130.0 Mobile Safari/537.36";
    for (path, forwarded) in [
        ("/", "203.0.113.5"),
        ("/?q=secret", "203.0.113.5"),
        ("/missing", "203.0.113.99"),
    ] {
        t.http
            .get(format!("http://{}{path}", t.daemon.addr))
            .header("host", &h)
            .header("user-agent", ua)
            .header("referer", "https://news.example.org/story")
            .header("x-forwarded-for", forwarded)
            .send()
            .await
            .unwrap();
    }
    t.get("unknown.sites.test", "/").await;
    t.daemon.state.request_log.flush().await;

    let db = t.daemon.state.store.db();
    let rows: Vec<(String, i64, Option<String>, String, String)> = sqlx::query_as(
        "SELECT path, status, referrer_host, ua_class, visitor_hash FROM requests WHERE ua_class = 'browser' ORDER BY id",
    )
    .fetch_all(db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3, "unknown hosts are not logged");
    assert_eq!(rows[0].0, "/");
    assert_eq!(rows[1].0, "/", "query strings are not stored");
    assert_eq!((rows[2].0.as_str(), rows[2].1), ("/missing", 404));
    assert_eq!(rows[0].2.as_deref(), Some("news.example.org"));
    assert_eq!(rows[0].4, rows[1].4, "same visitor, same hash");
    assert_ne!(
        rows[0].4, rows[2].4,
        "different forwarded address, different hash"
    );
    let dump: Vec<(String,)> = sqlx::query_as(
        "SELECT group_concat(path || visitor_hash || coalesce(referrer_host, '')) FROM requests",
    )
    .fetch_all(db)
    .await
    .unwrap();
    assert!(
        !dump[0].0.contains("203.0.113"),
        "no raw IP anywhere in the log"
    );
    // The publish health check shows up as a bot request.
    let (bots,): (i64,) = sqlx::query_as("SELECT count(*) FROM requests WHERE ua_class = 'bot'")
        .fetch_one(db)
        .await
        .unwrap();
    assert!(bots >= 1);
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn admin_socket_is_owner_only_and_exclusive() {
    use std::os::unix::fs::PermissionsExt;
    let t = boot().await;
    let socket = t.daemon.data.admin_socket();
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(t.daemon.data.root())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert!(
        sw_web::socket::bind(&socket).await.is_err(),
        "a live socket is not stolen"
    );

    let health: Value = t.admin().get("/v1/health").await.unwrap();
    assert_eq!(health["status"], "ok");
    t.daemon.shutdown().await;
    assert!(!socket.exists(), "socket removed on shutdown");
}

#[tokio::test]
async fn cli_reports_a_missing_daemon_clearly() {
    use clap::Parser;
    let dir = tempfile::tempdir().unwrap();
    let cli = sitewright::cli::Cli::try_parse_from([
        "sitewright",
        "--data-dir",
        dir.path().to_str().unwrap(),
        "site",
        "list",
    ])
    .unwrap();
    let err = sitewright::cli::run(cli).await.unwrap_err();
    assert!(err.to_string().contains("cannot reach the daemon"), "{err}");
}
