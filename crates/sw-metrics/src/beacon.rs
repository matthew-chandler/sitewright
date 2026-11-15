//! The event beacon: a tiny script every site can include, and the endpoint it posts to.
//!
//! No cookies, no local storage, no fingerprinting. A visitor is told apart only by the
//! same daily-rotating hash the request log uses, and the script stays silent for
//! browsers that send Do Not Track or Global Privacy Control.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use sqlx::SqlitePool;
use sw_core::time::now_ms;

/// Path of the script on every site host.
pub const SCRIPT_PATH: &str = "/_sw/a.js";
/// Path events are posted to on every site host.
pub const EVENT_PATH: &str = "/_sw/e";
/// Largest accepted event body, in bytes.
pub const MAX_BODY_BYTES: usize = 2048;
/// Events accepted per visitor per minute.
pub const EVENTS_PER_MINUTE: usize = 60;

/// The beacon script. `sw("name", {props})` records a custom event; a `pageview` is sent
/// on load. Sent as text/plain so the browser needs no CORS preflight.
pub const SCRIPT: &str = r#"(function(){var n=navigator;if(n.globalPrivacyControl||n.doNotTrack=="1")return window.sw=function(){};
function s(e,p){try{var b=JSON.stringify({n:e,u:location.pathname,p:p||{}});
n.sendBeacon?n.sendBeacon("/_sw/e",b):fetch("/_sw/e",{method:"POST",body:b,keepalive:true})}catch(_){}}
window.sw=s;var r="";try{r=new URL(document.referrer).hostname;if(r==location.hostname)r=""}catch(_){}
var w=innerWidth;s("pageview",{ref:r,screen:w<600?"small":w<1000?"medium":"large"})})();
"#;

/// A validated event, ready to store.
#[derive(Debug, Clone, PartialEq)]
pub struct BeaconEvent {
    pub name: String,
    pub path: String,
    /// JSON object text.
    pub props: String,
}

/// Parse and validate an event body. Anything unexpected is rejected rather than stored:
/// this endpoint is open to the internet.
pub fn parse_event(body: &[u8]) -> Result<BeaconEvent, &'static str> {
    if body.len() > MAX_BODY_BYTES {
        return Err("event too large");
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| "not JSON")?;
    let object = value.as_object().ok_or("not an object")?;
    let name = object
        .get("n")
        .and_then(Value::as_str)
        .ok_or("missing name")?;
    let name_ok = !name.is_empty()
        && name.len() <= 40
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        });
    if !name_ok {
        return Err("bad event name");
    }
    let path = object.get("u").and_then(Value::as_str).unwrap_or("");
    if path.len() > 512 || (!path.is_empty() && !path.starts_with('/')) {
        return Err("bad path");
    }
    // Properties: a flat object of short scalars.
    let mut props = Map::new();
    if let Some(given) = object.get("p") {
        let given = given.as_object().ok_or("props must be an object")?;
        if given.len() > 10 {
            return Err("too many props");
        }
        for (key, value) in given {
            let key_ok = !key.is_empty()
                && key.len() <= 40
                && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            let value_ok = match value {
                Value::String(s) => s.chars().count() <= 200,
                Value::Number(_) | Value::Bool(_) | Value::Null => true,
                _ => false,
            };
            if !key_ok || !value_ok {
                return Err("bad prop");
            }
            props.insert(key.clone(), value.clone());
        }
    }
    Ok(BeaconEvent {
        name: name.to_owned(),
        path: path.to_owned(),
        props: Value::Object(props).to_string(),
    })
}

/// Per-visitor rate limiter for the event endpoint.
#[derive(Debug, Default)]
pub struct BeaconLimiter {
    seen: Mutex<HashMap<String, (Instant, usize)>>,
}

impl BeaconLimiter {
    /// Count an event from `visitor`; false when the visitor is over the limit.
    pub fn allow(&self, visitor: &str) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let window = Duration::from_secs(60);
        if seen.len() > 50_000 {
            seen.retain(|_, (start, _)| now.duration_since(*start) < window);
        }
        let entry = seen.entry(visitor.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= EVENTS_PER_MINUTE
    }
}

/// Store one event.
pub async fn record(
    db: &SqlitePool,
    site_id: i64,
    visitor_hash: &str,
    event: &BeaconEvent,
) -> Result<(), sqlx::Error> {
    let now = now_ms();
    sqlx::query!(
        "INSERT INTO events (ts, site_id, name, path, props, visitor_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        now,
        site_id,
        event.name,
        event.path,
        event.props,
        visitor_hash
    )
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_is_tiny_and_stores_nothing_on_the_device() {
        assert!(SCRIPT.len() < 1024, "{} bytes", SCRIPT.len());
        for forbidden in [
            "cookie",
            "localStorage",
            "sessionStorage",
            "indexedDB",
            "http://",
            "https://",
        ] {
            assert!(!SCRIPT.contains(forbidden), "{forbidden}");
        }
        assert!(SCRIPT.contains("globalPrivacyControl") && SCRIPT.contains("doNotTrack"));
    }

    #[test]
    fn events_are_validated_strictly() {
        let ok = parse_event(
            br#"{"n":"search","u":"/trails/","p":{"q":"rainier","results":3,"ok":true}}"#,
        )
        .unwrap();
        assert_eq!((ok.name.as_str(), ok.path.as_str()), ("search", "/trails/"));
        assert_eq!(
            serde_json::from_str::<Value>(&ok.props).unwrap()["results"],
            3
        );
        assert_eq!(parse_event(br#"{"n":"pageview"}"#).unwrap().props, "{}");

        let long = format!(r#"{{"n":"x","p":{{"a":"{}"}}}}"#, "y".repeat(201));
        let many = format!(
            r#"{{"n":"x","p":{{{}}}}}"#,
            (0..11)
                .map(|i| format!("\"k{i}\":1"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let big = format!(r#"{{"n":"x","u":"/{}"}}"#, "a".repeat(3000));
        for bad in [
            "not json",
            "[]",
            "{}",
            r#"{"n":""}"#,
            r#"{"n":"Has Space"}"#,
            r#"{"n":"<script>"}"#,
            r#"{"n":"x","u":"https://evil"}"#,
            r#"{"n":"x","p":[1]}"#,
            r#"{"n":"x","p":{"a":{"nested":1}}}"#,
            r#"{"n":"x","p":{"bad key":1}}"#,
            &long,
            &many,
            &big,
        ] {
            assert!(parse_event(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_limiter_caps_events_per_visitor() {
        let limiter = BeaconLimiter::default();
        for _ in 0..EVENTS_PER_MINUTE {
            assert!(limiter.allow("a"));
        }
        assert!(!limiter.allow("a"));
        assert!(limiter.allow("b"));
    }
}
