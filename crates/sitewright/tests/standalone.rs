//! M6 end to end: standalone mode against Pebble. A site imported through the CLI gets a
//! certificate and is served over HTTPS; plain HTTP redirects; unknown names get nothing.

#![allow(clippy::unwrap_used)]

mod common;

use std::net::TcpListener as StdListener;
use std::process::Stdio;
use std::time::Duration;

use common::{ADMIN, boot_with_listen, path_str};
use tokio::process::{Child, Command};

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn have(binary: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
}

struct Pebble {
    dir: tempfile::TempDir,
    _children: Vec<Child>,
    directory: String,
}

/// Start Pebble and its DNS helper, validating HTTP-01 on `http_port` at 127.0.0.1.
async fn start_pebble(http_port: u16) -> Option<Pebble> {
    if !(have("pebble") && have("pebble-challtestsrv")) {
        assert!(
            std::env::var_os("SW_REQUIRE_PEBBLE").is_none(),
            "pebble is required but not installed"
        );
        eprintln!(
            "skipping: pebble and pebble-challtestsrv are not on PATH (run scripts/dev-setup.sh)"
        );
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let (acme_port, dns_port, dns_management) = (free_port(), free_port(), free_port());
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    std::fs::write(dir.path().join("root.pem"), cert.cert.pem()).unwrap();
    std::fs::write(dir.path().join("key.pem"), cert.signing_key.serialize_pem()).unwrap();
    let config = serde_json::json!({ "pebble": {
        "listenAddress": format!("127.0.0.1:{acme_port}"),
        "managementListenAddress": format!("127.0.0.1:{}", free_port()),
        "certificate": dir.path().join("root.pem"),
        "privateKey": dir.path().join("key.pem"),
        "httpPort": http_port,
        "tlsPort": free_port(),
        "ocspResponderURL": "",
        "externalAccountBindingRequired": false,
        "retryAfter": { "authz": 1, "order": 1 },
        "profiles": { "default": { "description": "test", "validityPeriod": 7776000 } },
    }});
    std::fs::write(dir.path().join("pebble.json"), config.to_string()).unwrap();
    let quiet = |command: &mut Command| {
        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let dns = quiet(
        Command::new("pebble-challtestsrv")
            .args([
                "-defaultIPv4",
                "127.0.0.1",
                "-defaultIPv6",
                "",
                "-http01",
                "",
                "-https01",
                "",
                "-tlsalpn01",
                "",
                "-doh",
                "",
            ])
            .args([
                "-dns01",
                &format!("127.0.0.1:{dns_port}"),
                "-management",
                &format!("127.0.0.1:{dns_management}"),
            ]),
    );
    let pebble = quiet(
        Command::new("pebble")
            .args([
                "-config",
                dir.path().join("pebble.json").to_str().unwrap(),
                "-dnsserver",
                &format!("127.0.0.1:{dns_port}"),
            ])
            .env("PEBBLE_VA_NOSLEEP", "1")
            .env("PEBBLE_WFE_NONCEREJECT", "0"),
    );
    for port in [acme_port, dns_management] {
        for attempt in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            assert!(attempt < 99, "pebble did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Some(Pebble {
        dir,
        _children: vec![dns, pebble],
        directory: format!("https://localhost:{acme_port}/dir"),
    })
}

#[tokio::test]
async fn a_site_gets_a_certificate_and_is_served_over_https() {
    let http_port = free_port();
    let Some(pebble) = start_pebble(http_port).await else {
        return;
    };
    let extra = format!(
        "[tls]\nhttps_listen = \"127.0.0.1:0\"\nacme_directory = \"{}\"\nacme_root_cert = \"{}\"\nprecheck = false\n\
         include_admin_host = false\nscan_interval_secs = 1\n[metrics]\nuptime_interval_secs = 0\n",
        pebble.directory,
        pebble.dir.path().join("root.pem").display()
    );
    let t = boot_with_listen(&extra, "standalone", &format!("127.0.0.1:{http_port}")).await;
    let https = t
        .daemon
        .https_addr
        .expect("standalone mode has an HTTPS listener");

    let dir = t.site_dir(
        "s",
        &[(
            "index.html",
            "<!doctype html><title>Hikes</title><h1>Over TLS</h1>",
        )],
    );
    t.cli(&["site", "import", "hikes", path_str(&dir)])
        .await
        .unwrap();
    let host = "hikes.sites.test";

    let secure = |sni: &'static str| {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .tls_info(true)
            .resolve(sni, https)
            .no_proxy()
            .build()
            .unwrap();
        async move {
            client
                .get(format!("https://{sni}:{}/", https.port()))
                .send()
                .await
        }
    };
    // The certificate arrives within a few scans.
    let mut response = None;
    for _ in 0..100 {
        if let Ok(ok) = secure(host).await {
            response = Some(ok);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let response = response.expect("the site never became reachable over HTTPS");
    assert_eq!(response.status(), 200);
    let der = response
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .unwrap()
        .peer_certificate()
        .unwrap()
        .to_vec();
    assert!(response.text().await.unwrap().contains("Over TLS"));
    let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
    let names = format!(
        "{:?}",
        cert.subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names
    );
    assert!(names.contains(host), "{names}");

    // Unknown names and the admin host (excluded here) get no certificate at all.
    assert!(secure("evil.sites.test").await.is_err());
    assert!(secure(ADMIN).await.is_err());
    assert_eq!(
        std::fs::read_dir(t.daemon.data.certs_dir())
            .unwrap()
            .count(),
        1
    );

    // Plain HTTP redirects to HTTPS; ACME and domain-check paths are exempt.
    let plain = t.get(host, "/trails?x=1").await;
    assert_eq!(plain.status(), 308);
    assert_eq!(
        plain.headers()["location"],
        format!("https://{host}:{}/trails?x=1", https.port())
    );
    assert_eq!(
        t.get(host, "/.well-known/acme-challenge/unknown")
            .await
            .status(),
        404
    );
    assert_eq!(
        t.get(host, "/.well-known/sitewright/unknown")
            .await
            .status(),
        404
    );

    // The request over HTTPS was logged like any other, and Settings shows the state.
    t.daemon.state.request_log.flush().await;
    let (logged,): (i64,) = sqlx::query_as("SELECT count(*) FROM requests WHERE path = '/'")
        .fetch_one(t.daemon.state.store.db())
        .await
        .unwrap();
    assert!(
        logged >= 2,
        "the health check and the HTTPS request are both logged"
    );
    let status = t.daemon.state.tls.get().unwrap().status();
    assert_eq!(status.len(), 1);
    assert!(status[0].certificate.is_some() && status[0].last_error.is_none());

    // Deleting the site removes its certificate.
    t.cli(&["site", "delete", "hikes", "--yes"]).await.unwrap();
    for _ in 0..50 {
        if secure(host).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        secure(host).await.is_err(),
        "a deleted site's certificate is no longer served"
    );
    t.daemon.shutdown().await;
}
