//! Backends end to end: real worker processes, in their jail, driven by the manager.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sw_backend::{
    BackendError, BackendRequest, BackendResponse, Backends, Host, Limits, Target, WorkerCommand,
};

/// A host that records calls and logs, and answers `echo` and `db.*` with canned data.
#[derive(Default)]
struct TestHost {
    calls: Mutex<Vec<(String, String, Value)>>,
    logs: Mutex<Vec<(String, String)>>,
}

impl Host for TestHost {
    fn call<'a>(
        &'a self,
        target: &'a Target,
        op: &'a str,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((target.site.clone(), op.to_owned(), args.clone()));
            match op {
                "db.query" => Ok(json!([{"id": 1, "name": "Ada"}, {"id": 2, "name": "Bo"}])),
                "db.run" => Ok(json!({"changes": 1, "lastInsertRowid": 3})),
                _ => Err(format!("unknown operation {op}")),
            }
        })
    }

    fn log(&self, _target: &Target, level: &str, message: &str) {
        self.logs
            .lock()
            .unwrap()
            .push((level.to_owned(), message.to_owned()));
    }
}

fn backends_with(limits: Limits) -> (Backends, Arc<TestHost>) {
    let host = Arc::new(TestHost::default());
    let command = WorkerCommand {
        // SW_BACKEND_TEST_WORKER runs the suite against another build of the worker,
        // for example the static musl one that releases ship.
        program: std::env::var_os("SW_BACKEND_TEST_WORKER")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_sw-backend-worker").into())
            .into(),
        args: Vec::new(),
    };
    (Backends::new(command, limits, host.clone()), host)
}

fn backends() -> (Backends, Arc<TestHost>) {
    backends_with(Limits::default())
}

fn target(site_id: i64, version: &str) -> Target {
    Target {
        site_id,
        site: format!("site{site_id}"),
        version: version.into(),
        draft: false,
    }
}

fn files(index: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("backend/index.js".to_owned(), index.to_owned())])
}

fn request(method: &str, url: &str, body: &str) -> BackendRequest {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    BackendRequest {
        method: method.into(),
        path: path.into(),
        query: query.into(),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("cookie".into(), "sid=abc%20def; theme=dark".into()),
        ],
        body: body.into(),
    }
}

async fn send(
    backends: &Backends,
    target: &Target,
    source: &str,
    req: BackendRequest,
) -> Result<BackendResponse, BackendError> {
    backends.handle(target, || Ok(files(source)), req).await
}

fn header<'a>(response: &'a BackendResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

const APP: &str = r#"
import { Router, json, text, redirect, db, HttpError, crypto } from "sitewright";
import { greet } from "./lib/greet.js";

let hits = 0;
const app = new Router();
app.use(async (req, next) => {
  const res = await next();
  res.headers.set("x-backend", "yes");
  return res;
});
app.get("/api/hello/:name", (req) => json({ hello: greet(req.params.name), q: req.query.q, sid: req.cookies.sid, hits: ++hits }));
app.post("/api/people", async (req) => {
  const body = await req.json();
  if (!body.name) throw new HttpError(400, "name is required");
  const result = await db.run("insert into people (name) values (?)", body.name);
  console.log("added", { name: body.name });
  return json({ id: result.lastInsertRowid }, 201);
});
app.get("/api/people", async () => await db.query("select * from people"));
app.get("/api/first", async () => await db.get("select * from people where id = ?", [1]));
app.get("/api/text", () => "plain");
app.get("/api/nothing", () => {});
app.get("/api/go", () => redirect("/thanks.html"));
app.get("/api/boom", () => { throw new Error("secret detail"); });
app.get("/api/uuid", () => text(crypto.randomUUID()));
app.get("/api/unknown-op", async () => { await db.batch([["x"]]); });
app.get("/api/files/*", (req) => text(req.params["*"]));
export default app;
"#;

fn app_files() -> BTreeMap<String, String> {
    let mut all = files(APP);
    all.insert(
        "backend/lib/greet.js".into(),
        "export const greet = (name) => `hi ${name}`;".into(),
    );
    all
}

#[tokio::test]
async fn routes_params_bodies_host_calls_and_logs() {
    let (backends, host) = backends();
    let t = target(1, "v1");
    let run = |req| backends.handle(&t, || Ok(app_files()), req);

    let res = run(request("GET", "/api/hello/Ada%20L?q=a+b", ""))
        .await
        .unwrap();
    assert_eq!(res.status, 200);
    assert_eq!(header(&res, "content-type"), Some("application/json"));
    assert_eq!(header(&res, "x-backend"), Some("yes"));
    let body: Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(
        body,
        json!({"hello": "hi Ada L", "q": "a b", "sid": "abc def", "hits": 1})
    );
    // The same worker serves the next request.
    let res = run(request("GET", "/api/hello/x", "")).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&res.body).unwrap()["hits"], 2);
    assert_eq!(backends.alive(), 1);

    let res = run(request("POST", "/api/people", r#"{"name":"Cy"}"#))
        .await
        .unwrap();
    assert_eq!((res.status, res.body.as_str()), (201, r#"{"id":3}"#));
    let res = run(request("POST", "/api/people", "{}")).await.unwrap();
    assert_eq!(
        (res.status, res.body.as_str()),
        (400, r#"{"error":"name is required"}"#)
    );
    let res = run(request("POST", "/api/people", "not json"))
        .await
        .unwrap();
    assert_eq!(res.status, 400);
    let res = run(request("GET", "/api/people", "")).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&res.body).unwrap()[1]["name"],
        "Bo"
    );
    let res = run(request("GET", "/api/first", "")).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&res.body).unwrap()["name"],
        "Ada"
    );

    let calls = host.calls.lock().unwrap().clone();
    assert_eq!(calls[0].0, "site1");
    assert_eq!(calls[0].1, "db.run");
    assert_eq!(
        calls[0].2,
        json!({"sql": "insert into people (name) values (?)", "params": ["Cy"]})
    );
    assert_eq!(calls.last().unwrap().2["params"], json!([1]));
    assert!(
        host.logs
            .lock()
            .unwrap()
            .contains(&("info".into(), r#"added {"name":"Cy"}"#.into()))
    );

    // Conveniences and errors.
    let res = run(request("GET", "/api/text", "")).await.unwrap();
    assert_eq!((res.status, res.body.as_str()), (200, "plain"));
    assert_eq!(
        run(request("GET", "/api/nothing", ""))
            .await
            .unwrap()
            .status,
        204
    );
    let res = run(request("GET", "/api/go", "")).await.unwrap();
    assert_eq!(
        (res.status, header(&res, "location")),
        (303, Some("/thanks.html"))
    );
    assert_eq!(
        run(request("GET", "/api/missing", ""))
            .await
            .unwrap()
            .status,
        404
    );
    assert_eq!(
        run(request("DELETE", "/api/people", ""))
            .await
            .unwrap()
            .status,
        405
    );
    let res = run(request("GET", "/api/files/a/b.txt", "")).await.unwrap();
    assert_eq!(res.body, "a/b.txt");
    let uuid = run(request("GET", "/api/uuid", "")).await.unwrap().body;
    assert_eq!(uuid.len(), 36);
    assert_eq!(&uuid[14..15], "4");

    // An unexpected error is a plain 500 for the visitor; the detail goes to the log.
    let res = run(request("GET", "/api/boom", "")).await.unwrap();
    assert_eq!(
        (res.status, res.body.as_str()),
        (500, r#"{"error":"internal error"}"#)
    );
    assert!(
        host.logs
            .lock()
            .unwrap()
            .iter()
            .any(|(level, m)| level == "error" && m.contains("secret detail"))
    );
    // So is an error from the host.
    assert_eq!(
        run(request("GET", "/api/unknown-op", ""))
            .await
            .unwrap()
            .status,
        500
    );
    assert!(
        host.logs
            .lock()
            .unwrap()
            .iter()
            .any(|(_, m)| m.contains("unknown operation db.batch"))
    );
}

#[tokio::test]
async fn code_that_does_not_load_is_reported() {
    let (backends, _) = backends();
    for (source, expect) in [
        ("export default {", "backend/index.js:1"),
        (
            "import x from './missing.js'; export default x;",
            "missing.js",
        ),
        ("import fs from 'fs'; export default fs;", "fs"),
        (
            "import x from '../index.html'; export default x;",
            "index.html",
        ),
        ("throw new Error('at load'); export default 1;", "at load"),
        ("export const nothing = 1;", "default"),
    ] {
        let err = send(
            &backends,
            &target(1, source),
            source,
            request("GET", "/api/x", ""),
        )
        .await
        .unwrap_err();
        match err {
            BackendError::Load(message) => assert!(message.contains(expect), "{source}: {message}"),
            other => panic!("{source}: unexpected {other:?}"),
        }
    }
    assert_eq!(backends.alive(), 0);
    // A default export that is not a handler fails per request, clearly, in the log.
    let res = send(
        &backends,
        &target(2, "v"),
        "export default 5;",
        request("GET", "/api/x", ""),
    )
    .await
    .unwrap();
    assert_eq!(res.status, 500);
}

#[tokio::test]
async fn host_calls_are_refused_while_loading() {
    let (backends, host) = backends();
    let source = "import { db } from 'sitewright'; const rows = await db.query('select 1'); export default () => rows;";
    let err = send(
        &backends,
        &target(1, "v"),
        source,
        request("GET", "/api/x", ""),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BackendError::Load(m) if m.contains("while the backend is loading")));
    assert!(host.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn runaway_code_is_stopped_and_the_worker_replaced() {
    let (backends, _) = backends_with(Limits {
        request_timeout: Duration::from_millis(300),
        memory_bytes: 16 * 1024 * 1024,
        ..Limits::default()
    });
    let source = r#"
import { Router, text } from "sitewright";
const app = new Router();
let n = 0;
app.get("/api/n", () => text(String(++n)));
app.get("/api/spin", () => { for (;;) {} });
app.get("/api/eat", () => { const a = []; for (;;) a.push("x".repeat(1024 * 1024) + a.length); });
app.get("/api/deep", () => { const f = () => f(); return f(); });
export default app;
"#;
    let t = target(1, "v");
    assert_eq!(
        send(&backends, &t, source, request("GET", "/api/n", ""))
            .await
            .unwrap()
            .body,
        "1"
    );

    let started = std::time::Instant::now();
    let err = send(&backends, &t, source, request("GET", "/api/spin", ""))
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::Timeout), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    // A fresh worker: the counter starts again.
    assert_eq!(
        send(&backends, &t, source, request("GET", "/api/n", ""))
            .await
            .unwrap()
            .body,
        "1"
    );

    // Out of memory and stack overflow are errors, not crashes of anything else.
    let res = send(&backends, &t, source, request("GET", "/api/eat", "")).await;
    assert!(
        matches!(&res, Ok(r) if r.status == 500)
            || matches!(
                &res,
                Err(BackendError::Failed(_) | BackendError::Crashed(_))
            ),
        "{res:?}"
    );
    let res = send(&backends, &t, source, request("GET", "/api/deep", ""))
        .await
        .unwrap();
    assert_eq!(res.status, 500);
    assert!(
        send(&backends, &t, source, request("GET", "/api/n", ""))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn the_jail_denies_files_network_and_processes() {
    // QuickJS has no file or socket API to call, so check the jail from the outside:
    // the worker's seccomp status as the kernel reports it, and that a worker which
    // tries anything else simply has nothing to try with.
    let (backends, _) = backends();
    let source = r#"
import { Router, json } from "sitewright";
const app = new Router();
app.get("/api/globals", () => json({
  names: Object.getOwnPropertyNames(globalThis).sort(),
  host: typeof globalThis.__host,
}));
export default app;
"#;
    let t = target(1, "v");
    let res = send(&backends, &t, source, request("GET", "/api/globals", ""))
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(
        body["host"], "undefined",
        "the host object is not reachable from site code"
    );
    let names: Vec<&str> = body["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for forbidden in [
        "require",
        "process",
        "std",
        "os",
        "Deno",
        "XMLHttpRequest",
        "WebSocket",
    ] {
        assert!(!names.contains(&forbidden), "{forbidden} is exposed");
    }

    // The kernel's view: every live worker runs with a seccomp filter (mode 2) and
    // cannot gain privileges.
    let mut checked = 0;
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let status = entry.path().join("status");
        let Ok(text) = std::fs::read_to_string(&status) else {
            continue;
        };
        let field = |name: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(name))
                .map(|v| v.trim().to_owned())
        };
        // Workers are this test process's children.
        if field("PPid:") != Some(std::process::id().to_string()) {
            continue;
        }
        // Other tests' workers may still be starting; only judge the settled ones.
        if field("Seccomp:").as_deref() == Some("2") {
            assert_eq!(field("NoNewPrivs:").as_deref(), Some("1"));
            checked += 1;
        }
    }
    assert!(checked >= 1, "no jailed worker found in /proc");
}

#[tokio::test]
async fn workers_are_per_site_replaced_on_new_code_capped_and_reaped() {
    let (backends, _) = backends_with(Limits {
        max_workers: 2,
        idle: Duration::from_millis(50),
        ..Limits::default()
    });
    let v = |tag: &str| {
        format!(
            "import {{ text }} from 'sitewright'; let n = 0; export default () => text('{tag}' + (++n));"
        )
    };
    let get = || request("GET", "/api/x", "");

    assert_eq!(
        send(&backends, &target(1, "a"), &v("a"), get())
            .await
            .unwrap()
            .body,
        "a1"
    );
    assert_eq!(
        send(&backends, &target(1, "a"), &v("a"), get())
            .await
            .unwrap()
            .body,
        "a2"
    );
    // New version of the code: a new worker.
    assert_eq!(
        send(&backends, &target(1, "b"), &v("b"), get())
            .await
            .unwrap()
            .body,
        "b1"
    );
    // A draft runs beside the live worker, not instead of it.
    let draft = Target {
        draft: true,
        ..target(1, "c")
    };
    assert_eq!(
        send(&backends, &draft, &v("c"), get()).await.unwrap().body,
        "c1"
    );
    assert_eq!(
        send(&backends, &target(1, "b"), &v("b"), get())
            .await
            .unwrap()
            .body,
        "b2"
    );
    assert_eq!(backends.alive(), 2);

    // A third worker pushes out the one idle longest (the draft).
    assert_eq!(
        send(&backends, &target(2, "a"), &v("s2-"), get())
            .await
            .unwrap()
            .body,
        "s2-1"
    );
    assert_eq!(backends.alive(), 2);
    assert_eq!(
        send(&backends, &target(1, "b"), &v("b"), get())
            .await
            .unwrap()
            .body,
        "b3"
    );

    backends.stop(2).await;
    assert_eq!(backends.alive(), 1);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(backends.reap_idle(), 1);
    assert_eq!(backends.alive(), 0);
    assert_eq!(
        send(&backends, &target(1, "b"), &v("b"), get())
            .await
            .unwrap()
            .body,
        "b1"
    );
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let (backends, _) = backends_with(Limits {
        max_request_body: 10,
        max_response_body: 100,
        ..Limits::default()
    });
    let source = "import { text } from 'sitewright'; export default (req) => text('x'.repeat(Number(req.query.n)));";
    let t = target(1, "v");
    assert!(matches!(
        send(
            &backends,
            &t,
            source,
            request("POST", "/api/x", "01234567890")
        )
        .await,
        Err(BackendError::TooLarge)
    ));
    assert!(
        send(&backends, &t, source, request("GET", "/api/x?n=100", ""))
            .await
            .is_ok()
    );
    assert!(matches!(
        send(&backends, &t, source, request("GET", "/api/x?n=101", "")).await,
        Err(BackendError::TooLarge)
    ));
}
