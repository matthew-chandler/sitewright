//! ACME against Pebble, Let's Encrypt's test server: issue, serve by hostname, renew,
//! survive a restart, and never issue for names we were not asked about.
//!
//! Needs the `pebble` and `pebble-challtestsrv` binaries on PATH (`scripts/dev-setup.sh`
//! installs them; CI does too). Without them the tests print a notice and pass, unless
//! `SW_REQUIRE_PEBBLE` is set.

#![allow(clippy::unwrap_used)]

use std::net::{SocketAddr, TcpListener as StdListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use sw_tls::serve::{http_app, serve_https, server_config};
use sw_tls::{Acme, CertStore, Challenges, Manager, ManagerOptions};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

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

/// A running Pebble with its DNS helper, validating HTTP-01 against `http_port`.
struct Pebble {
    _dir: tempfile::TempDir,
    _pebble: Child,
    _dns: Child,
    directory: String,
    root: PathBuf,
}

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
    let (acme_port, management_port, dns_port, dns_management) =
        (free_port(), free_port(), free_port(), free_port());

    // Pebble's own HTTPS listener needs a certificate; ours is self-signed and is what
    // the ACME client is told to trust.
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .unwrap();
    let root = dir.path().join("pebble-root.pem");
    std::fs::write(&root, cert.cert.pem()).unwrap();
    std::fs::write(
        dir.path().join("pebble-key.pem"),
        cert.signing_key.serialize_pem(),
    )
    .unwrap();
    let config = serde_json::json!({ "pebble": {
        "listenAddress": format!("127.0.0.1:{acme_port}"),
        "managementListenAddress": format!("127.0.0.1:{management_port}"),
        "certificate": root,
        "privateKey": dir.path().join("pebble-key.pem"),
        "httpPort": http_port,
        "tlsPort": free_port(),
        "ocspResponderURL": "",
        "externalAccountBindingRequired": false,
        "retryAfter": { "authz": 1, "order": 1 },
        "profiles": { "default": { "description": "test", "validityPeriod": 7776000 } },
    }});
    let config_path = dir.path().join("pebble.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    // Every name resolves to 127.0.0.1, where the test's HTTP listener is.
    let dns = Command::new("pebble-challtestsrv")
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
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pebble = Command::new("pebble")
        .args([
            "-config",
            config_path.to_str().unwrap(),
            "-dnsserver",
            &format!("127.0.0.1:{dns_port}"),
        ])
        .env("PEBBLE_VA_NOSLEEP", "1")
        .env("PEBBLE_WFE_NONCEREJECT", "0")
        .env("PEBBLE_VA_ALWAYS_VALID", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // Wait until both accept connections.
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
        _dir: dir,
        _pebble: pebble,
        _dns: dns,
        directory: format!("https://localhost:{acme_port}/dir"),
        root,
    })
}

async fn manager(pebble: &Pebble, data: &Path, challenges: Challenges) -> Manager {
    let acme = Acme::open(
        &data.join("acme/account.json"),
        &pebble.directory,
        Some("admin@example.com"),
        Some(&pebble.root),
        challenges,
    )
    .await
    .unwrap();
    Manager::new(
        acme,
        CertStore::open(&data.join("certs")).unwrap(),
        ManagerOptions::default(),
    )
}

/// The names on the certificate a TLS server presents for `sni`, or `None` if the
/// handshake fails.
async fn served_names(addr: SocketAddr, sni: &str) -> Option<Vec<String>> {
    sw_core::crypto::install_default_provider();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // Pebble's issuing CA is random per run
        .tls_info(true)
        .resolve(sni, addr)
        .no_proxy()
        .build()
        .unwrap();
    let response = client
        .get(format!("https://{sni}:{}/", addr.port()))
        .send()
        .await
        .ok()?;
    assert_eq!(response.status(), 200);
    let info = response
        .extensions()
        .get::<reqwest::tls::TlsInfo>()?
        .peer_certificate()?
        .to_vec();
    assert_eq!(response.text().await.unwrap(), "secure hello");
    let (_, cert) = x509_parser::parse_x509_certificate(&info).ok()?;
    let san = cert.subject_alternative_name().ok()??;
    Some(
        san.value
            .general_names
            .iter()
            .filter_map(|n| match n {
                x509_parser::extensions::GeneralName::DNSName(d) => Some((*d).to_owned()),
                _ => None,
            })
            .collect(),
    )
}

#[tokio::test]
async fn issues_serves_renews_and_reloads_certificates() {
    // The HTTP side: answers challenges, redirects the rest to HTTPS.
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http.local_addr().unwrap().port();
    let Some(pebble) = start_pebble(http_port).await else {
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let challenges = Challenges::default();
    let https = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let https_addr = https.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let plain = http_app(challenges.clone(), Some(https_addr.port()), Router::new());
    tokio::spawn(async move { axum::serve(http, plain).await.unwrap() });

    let manager = manager(&pebble, data.path(), challenges.clone()).await;
    let app = Router::new().fallback(|| async { "secure hello" });
    tokio::spawn(serve_https(
        https,
        server_config(manager.store().clone()),
        app,
        cancel.clone(),
    ));

    // Before any certificate exists, no hostname completes a handshake.
    assert_eq!(served_names(https_addr, "hikes.sites.test").await, None);

    // Three site hostnames get certificates.
    let hosts: Vec<String> = ["hikes", "blog", "shop"]
        .iter()
        .map(|n| format!("{n}.sites.test"))
        .collect();
    let issued = manager.ensure(&hosts).await;
    assert_eq!(issued, hosts, "{:?}", manager.status());
    for host in &hosts {
        assert_eq!(
            served_names(https_addr, host).await,
            Some(vec![host.clone()]),
            "{host}"
        );
        let meta = manager.store().meta(host).unwrap();
        assert_eq!(meta.issuer_directory, pebble.directory);
        assert!(meta.not_after - meta.not_before > 80 * 86_400);
    }
    // Private keys are owner-only on disk; the account is stored for reuse.
    use std::os::unix::fs::PermissionsExt;
    for file in ["certs/hikes.sites.test/key.pem", "acme/account.json"] {
        assert_eq!(
            std::fs::metadata(data.path().join(file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{file}"
        );
    }

    // A name that is not ours gets no certificate, however often it is asked for.
    for _ in 0..3 {
        assert_eq!(served_names(https_addr, "evil.sites.test").await, None);
    }
    assert_eq!(manager.store().hostnames().len(), 3);
    assert!(!data.path().join("certs/evil.sites.test").exists());

    // Nothing is due: a second pass issues nothing.
    assert!(manager.ensure(&hosts).await.is_empty());

    // The plain-HTTP side redirects to HTTPS but leaves /.well-known/ alone.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();
    let redirect = client
        .get(format!("http://127.0.0.1:{http_port}/trails?x=1"))
        .header("host", "hikes.sites.test")
        .send()
        .await
        .unwrap();
    assert_eq!(redirect.status(), 308);
    assert_eq!(
        redirect.headers()["location"],
        format!("https://hikes.sites.test:{}/trails?x=1", https_addr.port())
    );
    let unknown_challenge = client
        .get(format!(
            "http://127.0.0.1:{http_port}/.well-known/acme-challenge/nope"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown_challenge.status(), 404);

    // Renewal: age the stored certificate so that under a third of its life remains.
    let meta_path = data.path().join("certs/blog.sites.test/meta.json");
    let before = manager.store().meta("blog.sites.test").unwrap();
    let mut aged: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    aged["not_before"] = (now - 80 * 86_400).into();
    aged["not_after"] = (now + 10 * 86_400).into();
    std::fs::write(&meta_path, aged.to_string()).unwrap();
    let reloaded = Manager::new(
        Acme::open(
            &data.path().join("acme/account.json"),
            &pebble.directory,
            None,
            Some(&pebble.root),
            challenges.clone(),
        )
        .await
        .unwrap(),
        CertStore::open(&data.path().join("certs")).unwrap(),
        ManagerOptions::default(),
    );
    assert_eq!(
        reloaded.store().hostnames().len(),
        3,
        "certificates survive a restart"
    );
    assert_eq!(
        reloaded.ensure(&hosts).await,
        ["blog.sites.test"],
        "only the ageing certificate is renewed"
    );
    let after = reloaded.store().meta("blog.sites.test").unwrap();
    assert_ne!(after.serial, before.serial);
    assert!(after.not_after > now + 80 * 86_400);

    // A hostname that is no longer served loses its certificate.
    reloaded.ensure(&hosts[..2]).await;
    assert_eq!(
        reloaded.store().hostnames(),
        ["blog.sites.test", "hikes.sites.test"]
    );
    assert!(!data.path().join("certs/shop.sites.test").exists());

    // Renewal information is asked for without error (Pebble supports ARI).
    reloaded.check_renewal_information().await;
    cancel.cancel();
}

#[tokio::test]
async fn a_failed_validation_backs_off_and_can_be_prechecked() {
    // Nothing listens on the validation port, so HTTP-01 cannot succeed.
    let Some(pebble) = start_pebble(free_port()).await else {
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let manager = manager(&pebble, data.path(), Challenges::default()).await;
    let hosts = vec!["down.sites.test".to_owned()];
    assert!(manager.ensure(&hosts).await.is_empty());
    let status = manager.status();
    assert!(status[0].certificate.is_none());
    assert!(
        status[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("down.sites.test"),
        "{status:?}"
    );
    // The next pass does not hammer the certificate authority: it waits out the back-off.
    let started = std::time::Instant::now();
    assert!(manager.ensure(&hosts).await.is_empty());
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the retry was skipped, not attempted"
    );

    // With a precheck that fails, the certificate authority is not contacted at all.
    let options = ManagerOptions {
        precheck: Some(Arc::new(|host| {
            Box::pin(async move { Err(format!("{host} does not reach this server")) })
        })),
        ..ManagerOptions::default()
    };
    let acme = Acme::open(
        &data.path().join("acme/account.json"),
        &pebble.directory,
        None,
        Some(&pebble.root),
        Challenges::default(),
    )
    .await
    .unwrap();
    let guarded = Manager::new(
        acme,
        CertStore::open(&data.path().join("certs2")).unwrap(),
        options,
    );
    assert!(guarded.ensure(&hosts).await.is_empty());
    assert!(
        guarded.status()[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("not asking for a certificate yet")
    );
}
