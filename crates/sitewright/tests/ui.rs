//! M3 end to end: passkey enrollment and sign-in with a software authenticator, the
//! session and CSRF rules, and the UI flows (create, watch, edit, roll back, delete).

#![allow(clippy::unwrap_used)]

mod common;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ciborium::Value as Cbor;
use common::{
    ADMIN, ORIGIN, PAGE, TestDaemon, boot_with, host, llm_config, mock_model, text_turn, tool_turn,
    write_secrets,
};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use rand::Rng;
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// ------------------------------------------------------------ a software authenticator

/// A minimal WebAuthn authenticator: one ES256 key, "none" attestation, user verified.
struct SoftKey {
    key: SigningKey,
    credential_id: Vec<u8>,
    counter: u32,
}

impl SoftKey {
    fn new() -> Self {
        let mut secret = [0u8; 32];
        let mut id = [0u8; 32];
        rand::rng().fill_bytes(&mut secret);
        rand::rng().fill_bytes(&mut id);
        Self {
            key: SigningKey::from_slice(&secret).unwrap(),
            credential_id: id.to_vec(),
            counter: 0,
        }
    }

    fn client_data(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
        json!({ "type": kind, "challenge": challenge, "origin": origin, "crossOrigin": false })
            .to_string()
            .into_bytes()
    }

    /// rpIdHash, flags, counter, and optionally the attested credential data.
    fn authenticator_data(&self, rp_id: &str, attested: bool) -> Vec<u8> {
        let mut data = Sha256::digest(rp_id.as_bytes()).to_vec();
        // UP (0x01) | UV (0x04), plus AT (0x40) when a credential is attached.
        data.push(if attested { 0x45 } else { 0x05 });
        data.extend_from_slice(&self.counter.to_be_bytes());
        if attested {
            data.extend_from_slice(&[0u8; 16]); // AAGUID
            data.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
            data.extend_from_slice(&self.credential_id);
            let point = self.key.verifying_key().to_sec1_point(false);
            let cose = Cbor::Map(vec![
                (Cbor::Integer(1.into()), Cbor::Integer(2.into())), // kty: EC2
                (Cbor::Integer(3.into()), Cbor::Integer((-7).into())), // alg: ES256
                (Cbor::Integer((-1).into()), Cbor::Integer(1.into())), // crv: P-256
                (
                    Cbor::Integer((-2).into()),
                    Cbor::Bytes(point.x().unwrap().to_vec()),
                ),
                (
                    Cbor::Integer((-3).into()),
                    Cbor::Bytes(point.y().unwrap().to_vec()),
                ),
            ]);
            ciborium::into_writer(&cose, &mut data).unwrap();
        }
        data
    }

    /// The credential `navigator.credentials.create` would return.
    fn create(&self, options: &Value, origin: &str) -> Value {
        let pk = &options["publicKey"];
        let auth_data = self.authenticator_data(pk["rp"]["id"].as_str().unwrap(), true);
        let attestation = Cbor::Map(vec![
            (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
            (Cbor::Text("attStmt".into()), Cbor::Map(vec![])),
            (Cbor::Text("authData".into()), Cbor::Bytes(auth_data)),
        ]);
        let mut attestation_object = Vec::new();
        ciborium::into_writer(&attestation, &mut attestation_object).unwrap();
        let client_data =
            Self::client_data("webauthn.create", pk["challenge"].as_str().unwrap(), origin);
        json!({
            "id": B64.encode(&self.credential_id),
            "rawId": B64.encode(&self.credential_id),
            "type": "public-key",
            "extensions": {},
            "response": { "attestationObject": B64.encode(attestation_object), "clientDataJSON": B64.encode(client_data) },
        })
    }

    /// The assertion `navigator.credentials.get` would return.
    fn get(&mut self, options: &Value, origin: &str) -> Value {
        let pk = &options["publicKey"];
        self.counter += 1;
        let auth_data = self.authenticator_data(pk["rpId"].as_str().unwrap(), false);
        let client_data =
            Self::client_data("webauthn.get", pk["challenge"].as_str().unwrap(), origin);
        let mut signed = auth_data.clone();
        signed.extend_from_slice(&Sha256::digest(&client_data));
        let signature: Signature = self.key.sign(&signed);
        json!({
            "id": B64.encode(&self.credential_id),
            "rawId": B64.encode(&self.credential_id),
            "type": "public-key",
            "extensions": {},
            "response": {
                "authenticatorData": B64.encode(auth_data),
                "clientDataJSON": B64.encode(client_data),
                "signature": B64.encode(signature.to_der().as_bytes()),
                "userHandle": null,
            },
        })
    }
}

// ------------------------------------------------------------ a tiny browser

struct Browser<'a> {
    t: &'a TestDaemon,
    cookie: Option<String>,
    csrf: String,
}

impl<'a> Browser<'a> {
    fn new(t: &'a TestDaemon) -> Self {
        Self {
            t,
            cookie: None,
            csrf: String::new(),
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut builder = self
            .t
            .http
            .request(
                method.clone(),
                format!("http://{}{path}", self.t.daemon.addr),
            )
            .header("host", ADMIN);
        if method != Method::GET {
            builder = builder.header("origin", ORIGIN);
        }
        if let Some(cookie) = &self.cookie {
            builder = builder.header("cookie", format!("__Host-sw_session={cookie}"));
        }
        if !self.csrf.is_empty() && method != Method::GET {
            builder = builder.header("x-csrf-token", &self.csrf);
        }
        builder
    }

    async fn get(&self, path: &str) -> Response {
        self.request(Method::GET, path).send().await.unwrap()
    }

    async fn json(&self, path: &str, body: Value) -> Response {
        self.request(Method::POST, path)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn form(&self, path: &str, fields: &[(&str, &str)]) -> Response {
        self.request(Method::POST, path)
            .form(fields)
            .send()
            .await
            .unwrap()
    }

    /// Keep the session cookie from a response and read the CSRF token from the home page.
    async fn adopt_session(&mut self, response: &Response) {
        let set_cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(set_cookie.starts_with("__Host-sw_session="), "{set_cookie}");
        for attribute in ["Secure", "HttpOnly", "SameSite=Strict", "Path=/"] {
            assert!(
                set_cookie.contains(attribute),
                "{attribute} missing in {set_cookie}"
            );
        }
        let token = set_cookie
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_owned();
        self.cookie = Some(token);
        let home = self.get("/").await.text().await.unwrap();
        let marker = "name=\"csrf-token\" content=\"";
        let start = home.find(marker).unwrap() + marker.len();
        self.csrf = home[start..].split('"').next().unwrap().to_owned();
        assert!(self.csrf.len() > 20);
    }

    async fn enroll(&mut self, key: &SoftKey, invite: &str) -> Response {
        let start: Value = self
            .json("/auth/register/start", json!({ "invite": invite }))
            .await
            .json()
            .await
            .unwrap();
        if start.get("options").is_none() {
            // Refused (for example a used invite): hand back the refusal itself.
            return self
                .json("/auth/register/start", json!({ "invite": invite }))
                .await;
        }
        let credential = key.create(&start["options"], ORIGIN);
        self.json("/auth/register/finish", json!({ "ceremony": start["ceremony"], "credential": credential, "name": "Test phone" })).await
    }

    async fn sign_in(&mut self, key: &mut SoftKey) -> Response {
        let start: Value = self
            .json("/auth/login/start", json!({}))
            .await
            .json()
            .await
            .unwrap();
        if start.get("options").is_none() {
            return self.json("/auth/login/start", json!({})).await;
        }
        let credential = key.get(&start["options"], ORIGIN);
        self.json(
            "/auth/login/finish",
            json!({ "ceremony": start["ceremony"], "credential": credential }),
        )
        .await
    }
}

async fn invite(t: &TestDaemon) -> String {
    let invite: Value = t
        .admin()
        .post("/v1/admin/invite", &json!({}))
        .await
        .unwrap();
    let url = invite["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{ORIGIN}/enroll/")), "{url}");
    url.rsplit('/').next().unwrap().to_owned()
}

fn hx(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_owned())
}

/// Read a run's SSE stream to its end and return the event types.
async fn follow_run(browser: &Browser<'_>, id: &str) -> Vec<String> {
    let body = browser
        .get(&format!("/runs/{id}/events"))
        .await
        .text()
        .await
        .unwrap();
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .map(|v| v["type"].as_str().unwrap().to_owned())
        .collect()
}

// ------------------------------------------------------------ tests

#[tokio::test]
async fn nothing_is_reachable_without_signing_in() {
    let t = boot_with("").await;
    let browser = Browser::new(&t);
    for path in ["/", "/new", "/settings", "/sites/x", "/runs/1"] {
        let res = browser.get(path).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{path}");
        assert_eq!(res.headers()["location"], "/login");
    }
    assert_eq!(
        browser.get("/runs/1/events").await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        browser
            .form("/new", &[("prompt", "x"), ("name", "x")])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        browser
            .form("/sites/x/delete", &[("confirm", "x")])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        browser.get("/api/v1/sites").await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        browser
            .json("/api/v1/runs", json!({"site": "x", "prompt": "x"}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // The socket-only invite endpoint is not on the network at all.
    assert_eq!(
        browser
            .json("/api/v1/admin/invite", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // The sign-in page explains how to start when no passkey exists, and is locked down.
    let login = browser.get("/login").await;
    assert_eq!(login.status(), StatusCode::OK);
    let csp = login.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(login.headers()["x-frame-options"], "DENY");
    assert!(
        login
            .text()
            .await
            .unwrap()
            .contains("sitewright admin invite")
    );
    // Signing in is impossible before enrollment.
    assert_eq!(
        browser.json("/auth/login/start", json!({})).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        browser
            .json("/auth/register/start", json!({"invite": "made-up"}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        browser
            .json("/auth/register/start", json!({}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        browser
            .get("/enroll/made-up")
            .await
            .text()
            .await
            .unwrap()
            .contains("expired or was already used")
    );

    // Static assets and PWA files are public and cacheable by ETag.
    let css = browser.get("/static/app.css").await;
    assert_eq!(css.headers()["content-type"], "text/css");
    let etag = css.headers()["etag"].clone();
    let again = browser
        .request(Method::GET, "/static/app.css")
        .header("if-none-match", etag)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
    let manifest: Value = browser
        .get("/manifest.webmanifest")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(manifest["display"], "standalone");
    assert!(manifest["icons"].as_array().unwrap().len() >= 3);
    for path in [
        "/sw.js",
        "/static/icon-192.png",
        "/static/icon-512.png",
        "/static/vendor/htmx.min.js",
        "/static/passkey.js",
        "/static/run.js",
    ] {
        assert_eq!(browser.get(path).await.status(), StatusCode::OK, "{path}");
    }
    assert_eq!(
        browser.get("/static/nope.css").await.status(),
        StatusCode::NOT_FOUND
    );
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn passkey_enrollment_sign_in_csrf_and_sign_out() {
    let t = boot_with("").await;
    let mut key = SoftKey::new();
    let token = invite(&t).await;

    let mut browser = Browser::new(&t);
    assert!(
        browser
            .get(&format!("/enroll/{token}"))
            .await
            .text()
            .await
            .unwrap()
            .contains("Create passkey")
    );
    let enrolled = browser.enroll(&key, &token).await;
    assert_eq!(
        enrolled.status(),
        StatusCode::OK,
        "{:?}",
        enrolled.text().await
    );
    browser.adopt_session(&enrolled).await;
    assert!(
        browser
            .get("/")
            .await
            .text()
            .await
            .unwrap()
            .contains("<h1>Sites</h1>")
    );
    assert_eq!(
        browser.get("/login").await.status(),
        StatusCode::SEE_OTHER,
        "already signed in"
    );

    // An invite works once.
    let mut other = Browser::new(&t);
    assert_eq!(
        other.enroll(&SoftKey::new(), &token).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        other
            .get(&format!("/enroll/{token}"))
            .await
            .text()
            .await
            .unwrap()
            .contains("already used")
    );

    // State-changing requests need the CSRF token and the admin origin.
    let real_csrf = std::mem::take(&mut browser.csrf);
    let no_csrf = browser.form("/settings/tokens", &[("name", "x")]).await;
    assert_eq!(no_csrf.status(), StatusCode::FORBIDDEN);
    browser.csrf = "guess".into();
    let wrong_csrf = browser.form("/settings/tokens", &[("name", "x")]).await;
    assert_eq!(wrong_csrf.status(), StatusCode::FORBIDDEN);
    browser.csrf = real_csrf;
    let cross_origin = t
        .http
        .post(format!("http://{}/settings/tokens", t.daemon.addr))
        .header("host", ADMIN)
        .header("origin", "http://hikes.sites.test")
        .header(
            "cookie",
            format!("__Host-sw_session={}", browser.cookie.as_deref().unwrap()),
        )
        .header("x-csrf-token", &browser.csrf)
        .form(&[("name", "x")])
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        t.daemon.state.auth.list_api_tokens().await.unwrap().len(),
        0
    );

    // Sign in on a second browser with the same passkey; a different key is refused.
    let mut second = Browser::new(&t);
    let signed_in = second.sign_in(&mut key).await;
    assert_eq!(
        signed_in.status(),
        StatusCode::OK,
        "{:?}",
        signed_in.text().await
    );
    second.adopt_session(&signed_in).await;
    assert_ne!(second.cookie, browser.cookie);
    let mut stranger_key = SoftKey::new();
    stranger_key.credential_id = key.credential_id.clone();
    assert_eq!(
        Browser::new(&t).sign_in(&mut stranger_key).await.status(),
        StatusCode::FORBIDDEN
    );
    // A replayed ceremony is refused too.
    let start: Value = second
        .json("/auth/login/start", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let credential = key.get(&start["options"], ORIGIN);
    let body = json!({ "ceremony": start["ceremony"], "credential": credential });
    assert_eq!(
        Browser::new(&t)
            .json("/auth/login/finish", body.clone())
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        Browser::new(&t)
            .json("/auth/login/finish", body)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // A passkey made for another origin (phishing) is refused.
    let start: Value = second
        .json("/auth/login/start", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let phished = key.get(&start["options"], "https://admin.sites.test.evil.example");
    let res = Browser::new(&t)
        .json(
            "/auth/login/finish",
            json!({ "ceremony": start["ceremony"], "credential": phished }),
        )
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Settings: passkeys, API tokens.
    let settings = browser.get("/settings").await.text().await.unwrap();
    assert!(settings.contains("Test phone") && settings.contains("Add passkey"));
    assert!(
        !settings.contains("/settings/passkeys/1/delete"),
        "the only passkey cannot be removed"
    );
    let created = browser
        .form("/settings/tokens", &[("name", "ci")])
        .await
        .text()
        .await
        .unwrap();
    let api_token = created
        .split("class=\"token\">")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_owned();
    assert!(api_token.starts_with("swt_"));
    let api = |token: &str| {
        t.http
            .get(format!("http://{}/api/v1/sites", t.daemon.addr))
            .header("host", ADMIN)
            .header("authorization", format!("Bearer {token}"))
    };
    assert_eq!(
        api(&api_token).send().await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        api("swt_wrong").send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    // An API token is for the API only, and cannot mint enrollment links.
    let page = t
        .http
        .get(format!("http://{}/settings", t.daemon.addr))
        .header("host", ADMIN)
        .header("authorization", format!("Bearer {api_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::SEE_OTHER);
    let mint = t
        .http
        .post(format!("http://{}/api/v1/admin/invite", t.daemon.addr))
        .header("host", ADMIN)
        .header("authorization", format!("Bearer {api_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(mint.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        hx(
            &browser.form("/settings/tokens/1/delete", &[]).await,
            "hx-refresh"
        )
        .as_deref(),
        Some("true")
    );
    assert_eq!(
        api(&api_token).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    // A signed-in user can add a second passkey, then remove one.
    let laptop = SoftKey::new();
    let start: Value = browser
        .json("/auth/register/start", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let credential = laptop.create(&start["options"], ORIGIN);
    let added = browser
        .json(
            "/auth/register/finish",
            json!({ "ceremony": start["ceremony"], "credential": credential, "name": "Laptop" }),
        )
        .await;
    assert_eq!(added.status(), StatusCode::OK);
    assert!(
        added.headers().get("set-cookie").is_none(),
        "adding a passkey does not start a new session"
    );
    assert_eq!(t.daemon.state.auth.list_passkeys().await.unwrap().len(), 2);
    assert_eq!(
        hx(
            &browser.form("/settings/passkeys/1/delete", &[]).await,
            "hx-refresh"
        )
        .as_deref(),
        Some("true")
    );
    assert!(
        browser
            .form("/settings/passkeys/2/delete", &[])
            .await
            .text()
            .await
            .unwrap()
            .contains("last passkey")
    );

    // Sign out ends this session only.
    let out = browser.form("/logout", &[]).await;
    assert!(
        out.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(hx(&out, "hx-redirect").as_deref(), Some("/login"));
    assert_eq!(browser.get("/").await.status(), StatusCode::SEE_OTHER);
    assert_eq!(second.get("/").await.status(), StatusCode::OK);

    // A site's hostname never serves the admin UI, and the cookie is not sent there
    // by a browser (host-only); even if it were, sites have no session endpoints.
    assert_eq!(
        t.get("hikes.sites.test", "/login").await.status(),
        StatusCode::NOT_FOUND
    );
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn create_watch_edit_roll_back_and_delete_from_the_ui() {
    let model = mock_model(vec![
        tool_turn("create_site", json!({"name": "hikes", "title": "Washington Hikes"})),
        tool_turn("write_file", json!({"path": "index.html", "content": PAGE})),
        tool_turn("publish_site", json!({"message": "First version"})),
        text_turn("Your site is live."),
        tool_turn("write_file", json!({"path": "index.html", "content": PAGE.replace("Washington trails", "Second edition")})),
        tool_turn("publish_site", json!({"message": "Second edition"})),
        text_turn("Updated."),
        tool_turn("delete_site", json!({})),
        text_turn("Please confirm."),
    ])
    .await;
    let t = boot_with(&llm_config(&model)).await;
    write_secrets(&t);
    let mut browser = Browser::new(&t);
    let enrolled = browser.enroll(&SoftKey::new(), &invite(&t).await).await;
    browser.adopt_session(&enrolled).await;

    // New site.
    let new_page = browser.get("/new").await.text().await.unwrap();
    assert!(new_page.contains("What should the site be?") && new_page.contains(".sites.test"));
    let bad = browser
        .form("/new", &[("prompt", "x"), ("name", "Not Valid")])
        .await
        .text()
        .await
        .unwrap();
    assert!(
        bad.contains("msg error") && bad.contains("invalid site name"),
        "{bad}"
    );
    let started = browser
        .form("/new", &[("prompt", "a hiking log"), ("name", "hikes")])
        .await;
    let run_url = hx(&started, "hx-redirect").unwrap();
    assert_eq!(run_url, "/runs/1");

    // Watch it build.
    let run_page = browser.get(&run_url).await.text().await.unwrap();
    assert!(run_page.contains("a hiking log") && run_page.contains("data-run=\"1\""));
    let kinds = follow_run(&browser, "1").await;
    assert_eq!(kinds.first().map(String::as_str), Some("started"));
    assert_eq!(kinds.last().map(String::as_str), Some("done"));
    assert!(kinds.iter().filter(|k| *k == "tool_call").count() == 3);
    assert!(
        t.get(&host("hikes"), "/")
            .await
            .text()
            .await
            .unwrap()
            .contains("Washington trails")
    );

    // Home shows the site and the run.
    let home = browser.get("/").await.text().await.unwrap();
    assert!(
        home.contains("Washington Hikes")
            && home.contains("dot live")
            && home.contains("http://hikes.sites.test")
    );
    assert!(home.contains("href=\"/runs/1\"") && home.contains("succeeded"));

    // Edit with a prompt.
    let edit = browser
        .form(
            "/sites/hikes/prompt",
            &[("prompt", "second edition please")],
        )
        .await;
    assert_eq!(hx(&edit, "hx-redirect").as_deref(), Some("/runs/2"));
    follow_run(&browser, "2").await;
    assert!(
        t.get(&host("hikes"), "/")
            .await
            .text()
            .await
            .unwrap()
            .contains("Second edition")
    );

    // The site page lists both releases with a change summary; roll back to the first.
    let site_page = browser.get("/sites/hikes").await.text().await.unwrap();
    assert!(site_page.contains("First version") && site_page.contains("Second edition"));
    assert!(site_page.contains("1 files: 1 changed") && site_page.contains("Open site"));
    let first_sha = site_page
        .split("name=\"release\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(first_sha.len(), 40);
    let rolled = browser
        .form("/sites/hikes/rollback", &[("release", &first_sha)])
        .await;
    assert_eq!(hx(&rolled, "hx-refresh").as_deref(), Some("true"));
    assert!(
        t.get(&host("hikes"), "/")
            .await
            .text()
            .await
            .unwrap()
            .contains("Washington trails")
    );

    // The agent can only ask for deletion; the user confirms by typing the name.
    let ask = browser
        .form("/sites/hikes/prompt", &[("prompt", "delete this site")])
        .await;
    let kinds = follow_run(
        &browser,
        hx(&ask, "hx-redirect").unwrap().rsplit('/').next().unwrap(),
    )
    .await;
    assert!(kinds.contains(&"confirmation".to_owned()));
    assert_eq!(
        t.get(&host("hikes"), "/").await.status(),
        StatusCode::OK,
        "still live"
    );
    let wrong = browser
        .form("/sites/hikes/delete", &[("confirm", "hike")])
        .await
        .text()
        .await
        .unwrap();
    assert!(wrong.contains("type hikes exactly"));
    let deleted = browser
        .form("/sites/hikes/delete", &[("confirm", "hikes")])
        .await;
    assert_eq!(hx(&deleted, "hx-redirect").as_deref(), Some("/"));
    assert_eq!(
        t.get(&host("hikes"), "/").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        browser.get("/sites/hikes").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        browser.get("/runs/999").await.status(),
        StatusCode::NOT_FOUND
    );
    t.daemon.shutdown().await;
}

#[tokio::test]
async fn providers_can_be_added_tested_and_removed_in_settings() {
    let model = mock_model(vec![
        tool_turn("ping", json!({"value": "pong"})),
        text_turn("no tool call"),
    ])
    .await;
    let t = boot_with("").await;
    let mut browser = Browser::new(&t);
    let enrolled = browser.enroll(&SoftKey::new(), &invite(&t).await).await;
    browser.adopt_session(&enrolled).await;

    assert!(
        browser
            .get("/new")
            .await
            .text()
            .await
            .unwrap()
            .contains("No LLM provider is configured")
    );
    let base = format!("{}/v1", model.uri());
    let fields = [
        ("id", "mock"),
        ("kind", "openai_compat"),
        ("base_url", base.as_str()),
        ("model", "mock-mini"),
        ("auth", "api_key_header"),
        ("api_key", "test-key"),
    ];
    assert_eq!(
        hx(
            &browser.form("/settings/providers", &fields).await,
            "hx-refresh"
        )
        .as_deref(),
        Some("true")
    );
    let settings = browser.get("/settings").await.text().await.unwrap();
    assert!(settings.contains("mock-mini") && settings.contains("key saved"));
    assert!(
        !settings.contains("test-key"),
        "the key is never sent back to the browser"
    );

    let ok = browser
        .form("/settings/providers/mock/test", &[])
        .await
        .text()
        .await
        .unwrap();
    assert!(ok.contains("msg ok") && ok.contains("Works"), "{ok}");
    let not_ok = browser
        .form("/settings/providers/mock/test", &[])
        .await
        .text()
        .await
        .unwrap();
    assert!(
        not_ok.contains("msg error") && not_ok.contains("did not call the test tool"),
        "{not_ok}"
    );

    let invalid = browser
        .form(
            "/settings/providers",
            &[("id", "Bad Id"), ("kind", "openai_compat"), ("model", "m")],
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(invalid.contains("msg error"));
    let dns = browser
        .form(
            "/settings/dns-check",
            &[("hostname", "does-not-exist.invalid")],
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(dns.contains("does not resolve"));

    assert_eq!(
        hx(
            &browser.form("/settings/providers/mock/delete", &[]).await,
            "hx-refresh"
        )
        .as_deref(),
        Some("true")
    );
    assert!(
        sw_core::Secrets::load(&t.daemon.data.secrets_file())
            .unwrap()
            .key("mock")
            .is_none()
    );
    t.daemon.shutdown().await;
}
