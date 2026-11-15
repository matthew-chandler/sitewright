//! The read-only SQL sandbox for metrics queries written by the agent.
//!
//! Agent-written SQL is untrusted. It runs on its own connection that is opened
//! read-only, and four independent limits apply:
//!
//! 1. **An authorizer** (SQLite's compile-time access hook) allows only `SELECT`, only an
//!    allowlist of functions, and only reads that go through the metric views. Tables
//!    such as `sessions` or `api_tokens` cannot be named at all.
//! 2. **Site scoping by construction.** The metric views are temporary views on this
//!    connection that filter on a scope row set by the server before each query, so a
//!    query sees one site's data even if it forgets its `WHERE site_id = :site_id`.
//! 3. **A time limit**, enforced with a progress handler.
//! 4. **A row limit.**
//!
//! This is the one module in the workspace that needs `unsafe`: sqlx has no safe wrapper
//! for `sqlite3_set_authorizer`.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use libsqlite3_sys as ffi;
use serde::Serialize;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteRow};
use sqlx::{Column, ConnectOptions, Row, TypeInfo, ValueRef};
use tokio::sync::Mutex;

/// Longest a query may run.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Most rows a query may return.
pub const MAX_ROWS: usize = 1000;
/// Longest accepted query, in bytes.
pub const MAX_SQL_BYTES: usize = 4000;

/// The views a query may read.
pub const VIEWS: &[&str] = &["v_requests", "v_events", "v_checks", "v_daily"];

/// SQL functions a query may call. Everything else is refused.
const FUNCTIONS: &[&str] = &[
    "count",
    "sum",
    "total",
    "avg",
    "min",
    "max",
    "group_concat",
    "string_agg",
    "abs",
    "round",
    "ceil",
    "ceiling",
    "floor",
    "coalesce",
    "ifnull",
    "nullif",
    "iif",
    "length",
    "lower",
    "upper",
    "substr",
    "substring",
    "trim",
    "ltrim",
    "rtrim",
    "replace",
    "instr",
    "printf",
    "format",
    "like",
    "glob",
    "typeof",
    "date",
    "time",
    "datetime",
    "julianday",
    "strftime",
    "unixepoch",
    "json_extract",
    "json_type",
    "json_array_length",
    "json_valid",
    "->",
    "->>",
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    "percent_rank",
    "cume_dist",
];

/// The temporary views, created on the sandbox connection. `ts` is ISO-8601 UTC text so
/// that `date(ts)`, `strftime(...)` and comparisons with `:from` / `:to` just work.
const SETUP: &str = "
CREATE TEMP TABLE sw_scope (site_id INTEGER NOT NULL);
INSERT INTO sw_scope VALUES (-1);

CREATE TEMP VIEW v_requests AS
SELECT r.site_id AS site_id,
       strftime('%Y-%m-%dT%H:%M:%SZ', r.ts / 1000, 'unixepoch') AS ts,
       date(r.ts / 1000, 'unixepoch') AS day,
       CAST(strftime('%H', r.ts / 1000, 'unixepoch') AS INTEGER) AS hour,
       r.method AS method, r.path AS path, r.status AS status, r.bytes AS bytes,
       r.duration_ms AS duration_ms, r.referrer_host AS referrer_host, r.ua_class AS ua_class,
       r.visitor_hash AS visitor,
       (r.method = 'GET' AND r.status < 400 AND r.ua_class <> 'bot'
        AND (r.path LIKE '%/' OR r.path LIKE '%.html' OR replace(r.path, rtrim(r.path, replace(r.path, '/', '')), '') NOT LIKE '%.%')) AS is_page_view
FROM main.requests r WHERE r.site_id = (SELECT site_id FROM sw_scope);

CREATE TEMP VIEW v_events AS
SELECT e.site_id AS site_id,
       strftime('%Y-%m-%dT%H:%M:%SZ', e.ts / 1000, 'unixepoch') AS ts,
       date(e.ts / 1000, 'unixepoch') AS day,
       e.name AS name, e.path AS path, e.props AS props, e.visitor_hash AS visitor
FROM main.events e WHERE e.site_id = (SELECT site_id FROM sw_scope);

CREATE TEMP VIEW v_checks AS
SELECT c.site_id AS site_id,
       strftime('%Y-%m-%dT%H:%M:%SZ', c.ts / 1000, 'unixepoch') AS ts,
       date(c.ts / 1000, 'unixepoch') AS day,
       c.ok AS ok, c.latency_ms AS latency_ms, c.status AS status, c.cert_days AS cert_days
FROM main.checks c WHERE c.site_id = (SELECT site_id FROM sw_scope);

CREATE TEMP VIEW v_daily AS
SELECT d.site_id AS site_id, d.day AS day, d.views AS views, d.visitors AS visitors,
       d.errors AS errors, d.p95_ms AS p95_ms, d.uptime_pct AS uptime_pct
FROM main.daily_rollups d WHERE d.site_id = (SELECT site_id FROM sw_scope);
";

/// The result of a sandboxed query.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// One object per row, keyed by column name.
    pub rows: Vec<Value>,
    /// True when more than [`MAX_ROWS`] rows matched and the rest were dropped.
    pub truncated: bool,
}

/// The sandbox. One connection, used by one query at a time.
#[derive(Debug)]
pub struct Sandbox {
    conn: Mutex<SqliteConnection>,
    /// Shared with the authorizer callback: when false, everything is allowed (used
    /// only while the server itself sets the scope).
    enforcing: Arc<AtomicBool>,
}

impl Sandbox {
    /// Open the sandbox on the database at `path`.
    pub async fn open(path: &Path) -> Result<Self, sqlx::Error> {
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .connect()
            .await?;
        sqlx::raw_sql(SETUP).execute(&mut conn).await?;
        let enforcing = Arc::new(AtomicBool::new(true));
        {
            let mut handle = conn.lock_handle().await?;
            // SAFETY: `handle` is a valid, locked connection. The user-data pointer is
            // the `AtomicBool` inside `enforcing`, which this struct keeps alive for as
            // long as the connection exists, and the callback only reads it.
            let rc = unsafe {
                ffi::sqlite3_set_authorizer(
                    handle.as_raw_handle().as_ptr(),
                    Some(authorize),
                    Arc::as_ptr(&enforcing) as *mut c_void,
                )
            };
            if rc != ffi::SQLITE_OK {
                return Err(sqlx::Error::Protocol(format!(
                    "could not install the SQL authorizer (code {rc})"
                )));
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
            enforcing,
        })
    }

    /// Run one `SELECT` for `site_id`. `:site_id`, `:from` and `:to` in the SQL are
    /// replaced by the server; `from` and `to` are ISO-8601 UTC timestamps.
    pub async fn query(
        &self,
        site_id: i64,
        from: &str,
        to: &str,
        sql: &str,
    ) -> Result<QueryResult, String> {
        let sql = prepare_sql(sql, site_id, from, to)?;
        let mut conn = self.conn.lock().await;

        // Set the scope with the authorizer relaxed, then enforce for the query itself.
        self.enforcing.store(false, Ordering::SeqCst);
        let scoped = sqlx::query("UPDATE temp.sw_scope SET site_id = ?")
            .bind(site_id)
            .execute(&mut *conn)
            .await;
        self.enforcing.store(true, Ordering::SeqCst);
        scoped.map_err(|e| format!("could not prepare the query: {e}"))?;

        let deadline = Instant::now() + QUERY_TIMEOUT;
        conn.lock_handle()
            .await
            .map_err(|e| e.to_string())?
            .set_progress_handler(1000, move || Instant::now() < deadline);

        let outcome = collect(&mut conn, &sql).await;
        if let Ok(mut handle) = conn.lock_handle().await {
            handle.remove_progress_handler();
        }
        outcome.map_err(|e| explain(&e, Instant::now() >= deadline))
    }
}

async fn collect(conn: &mut SqliteConnection, sql: &str) -> Result<QueryResult, sqlx::Error> {
    let mut columns: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;
    // The text is untrusted by design: the authorizer, not string checks, is what makes
    // running it safe. `persistent(false)`: one-off queries stay out of the cache.
    let mut stream = sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
        .persistent(false)
        .fetch(conn);
    while let Some(row) = stream.try_next().await? {
        if columns.is_empty() {
            columns = row.columns().iter().map(|c| c.name().to_owned()).collect();
        }
        if rows.len() >= MAX_ROWS {
            truncated = true;
            break;
        }
        rows.push(row_to_json(&row));
    }
    Ok(QueryResult {
        columns,
        rows,
        truncated,
    })
}

fn row_to_json(row: &SqliteRow) -> Value {
    let mut object = serde_json::Map::new();
    for (i, column) in row.columns().iter().enumerate() {
        let kind = row
            .try_get_raw(i)
            .map(|v| v.type_info().name().to_owned())
            .unwrap_or_default();
        let value = match kind.as_str() {
            "INTEGER" | "BOOLEAN" => row
                .try_get::<i64, _>(i)
                .map(Value::from)
                .unwrap_or(Value::Null),
            "REAL" => row
                .try_get::<f64, _>(i)
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            "TEXT" | "DATE" | "TIME" | "DATETIME" => row
                .try_get::<String, _>(i)
                .map(Value::from)
                .unwrap_or(Value::Null),
            _ => Value::Null, // NULL and BLOB
        };
        object.insert(column.name().to_owned(), value);
    }
    Value::Object(object)
}

/// Check the SQL's shape and substitute the three server-provided parameters.
fn prepare_sql(sql: &str, site_id: i64, from: &str, to: &str) -> Result<String, String> {
    let sql = sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return Err("the query is empty".into());
    }
    if sql.len() > MAX_SQL_BYTES {
        return Err(format!("the query is longer than {MAX_SQL_BYTES} bytes"));
    }
    if sql.contains(';') {
        return Err("only one statement is allowed (remove the ';')".into());
    }
    let head = sql
        .trim_start_matches('(')
        .trim_start()
        .to_ascii_lowercase();
    if !(head.starts_with("select") || head.starts_with("with")) {
        return Err("only SELECT queries are allowed".into());
    }
    let timestamp_ok = |t: &str| {
        t.len() <= 32
            && t.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | ' ' | '.'))
    };
    if !timestamp_ok(from) || !timestamp_ok(to) {
        return Err("the time range is not valid".into());
    }

    // Replace `:name` tokens that are not part of a longer identifier.
    let mut out = String::with_capacity(sql.len() + 64);
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let rest = &sql[i + 1..];
            let name_len = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            let name = &rest[..name_len];
            let replacement = match name {
                "site_id" => Some(site_id.to_string()),
                "from" => Some(format!("'{from}'")),
                "to" => Some(format!("'{to}'")),
                _ => None,
            };
            match replacement {
                Some(value) => {
                    out.push_str(&value);
                    i += 1 + name_len;
                    continue;
                }
                None if name_len > 0 => {
                    return Err(format!(
                        "unknown parameter :{name}; only :site_id, :from and :to are available"
                    ));
                }
                None => {}
            }
        }
        // Advance one whole character.
        let ch_len = sql[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&sql[i..i + ch_len]);
        i += ch_len;
    }
    Ok(out)
}

/// Turn a database error into something the agent (or a person) can act on.
fn explain(error: &sqlx::Error, timed_out: bool) -> String {
    let text = error.to_string();
    if timed_out || text.contains("interrupted") {
        return format!(
            "the query took longer than {} seconds and was stopped; make it cheaper",
            QUERY_TIMEOUT.as_secs()
        );
    }
    if text.contains("not authorized") || text.contains("prohibited") {
        return format!(
            "not allowed: queries may only SELECT from {} using ordinary functions ({text})",
            VIEWS.join(", ")
        );
    }
    format!("SQL error: {text}")
}

/// SQLite's authorizer callback. Called while a statement is compiled, once per thing
/// the statement wants to do.
unsafe extern "C" fn authorize(
    user_data: *mut c_void,
    action: c_int,
    arg1: *const c_char,
    arg2: *const c_char,
    database: *const c_char,
    accessor: *const c_char,
) -> c_int {
    // SAFETY: `user_data` is the `AtomicBool` registered in `Sandbox::open`, alive as
    // long as the connection.
    let enforcing = unsafe { &*(user_data as *const AtomicBool) };
    if !enforcing.load(Ordering::SeqCst) {
        return ffi::SQLITE_OK;
    }
    // SAFETY: SQLite passes NUL-terminated strings or null pointers.
    let text = |ptr: *const c_char| {
        if ptr.is_null() {
            ""
        } else {
            unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or("\u{0}")
        }
    };
    let allowed = match action {
        ffi::SQLITE_SELECT | ffi::SQLITE_RECURSIVE => true,
        // arg1 is the table, `accessor` the view the read comes through (if any). Real
        // tables always come with a database name ("main", "temp"); a read without one
        // is a common table expression or subquery defined by the query itself.
        ffi::SQLITE_READ => {
            let table = text(arg1);
            let via = text(accessor);
            let own_cte = text(database).is_empty();
            VIEWS.contains(&table)
                || VIEWS.contains(&via)
                || own_cte
                || matches!(table, "json_each" | "json_tree")
        }
        // arg2 is the function name.
        ffi::SQLITE_FUNCTION => FUNCTIONS.contains(&text(arg2).to_ascii_lowercase().as_str()),
        _ => false,
    };
    if allowed {
        ffi::SQLITE_OK
    } else {
        ffi::SQLITE_DENY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "2020-01-01T00:00:00Z";
    const TO: &str = "2100-01-01T00:00:00Z";

    async fn sandbox() -> (tempfile::TempDir, sqlx::SqlitePool, Sandbox) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = sw_core::db::open(&path).await.unwrap();
        sw_core::db::migrate(&db, &dir.path().join("b"))
            .await
            .unwrap();
        // Two sites' worth of requests, an event, a secret, at a fixed time.
        let ts = 1_700_000_000_000i64;
        for (site, path, status, ua) in [
            (1, "/", 200, "browser"),
            (1, "/about", 200, "browser"),
            (1, "/site.css", 200, "browser"),
            (1, "/x", 404, "bot"),
            (2, "/secret-page", 200, "browser"),
        ] {
            sqlx::query("INSERT INTO requests (ts, site_id, method, path, status, bytes, duration_ms, ua_class, visitor_hash) VALUES (?, ?, 'GET', ?, ?, 10, 5, ?, 'v1')")
                .bind(ts).bind(site).bind(path).bind(status).bind(ua).execute(&db).await.unwrap();
        }
        sqlx::query("INSERT INTO events (ts, site_id, name, path, props, visitor_hash) VALUES (?, 1, 'search', '/', '{\"q\":\"rainier\"}', 'v1')").bind(ts).execute(&db).await.unwrap();
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES ('visitor_salt', 'TOPSECRET', 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        let sandbox = Sandbox::open(&path).await.unwrap();
        (dir, db, sandbox)
    }

    #[tokio::test]
    async fn selects_from_views_with_server_parameters() {
        let (_dir, _db, sandbox) = sandbox().await;
        let result = sandbox
            .query(1, FROM, TO, "SELECT day, count(*) AS n FROM v_requests WHERE site_id = :site_id AND ts BETWEEN :from AND :to AND is_page_view GROUP BY day;")
            .await
            .unwrap();
        assert_eq!(result.columns, ["day", "n"]);
        assert_eq!(
            result.rows,
            [serde_json::json!({"day": "2023-11-14", "n": 2})],
            "pages only: no assets, no bots"
        );

        let events = sandbox
            .query(
                1,
                FROM,
                TO,
                "SELECT name, json_extract(props, '$.q') AS q, ts FROM v_events",
            )
            .await
            .unwrap();
        assert_eq!(events.rows[0]["q"], "rainier");
        assert_eq!(events.rows[0]["ts"], "2023-11-14T22:13:20Z");

        let cte = sandbox.query(1, FROM, TO, "WITH t AS (SELECT status FROM v_requests) SELECT status, count(*) AS n FROM t GROUP BY status ORDER BY status").await.unwrap();
        assert_eq!(cte.rows.len(), 2);
    }

    #[tokio::test]
    async fn a_query_only_ever_sees_its_own_site() {
        let (_dir, _db, sandbox) = sandbox().await;
        // No WHERE clause at all: still only site 1.
        let all = sandbox
            .query(1, FROM, TO, "SELECT path FROM v_requests ORDER BY path")
            .await
            .unwrap();
        assert_eq!(all.rows.len(), 4);
        assert!(!all.rows.iter().any(|r| r["path"] == "/secret-page"));
        // Asking for another site by number finds nothing.
        let other = sandbox
            .query(
                1,
                FROM,
                TO,
                "SELECT count(*) AS n FROM v_requests WHERE site_id = 2",
            )
            .await
            .unwrap();
        assert_eq!(other.rows[0]["n"], 0);
        // The scope follows the caller.
        let two = sandbox
            .query(2, FROM, TO, "SELECT path FROM v_requests")
            .await
            .unwrap();
        assert_eq!(two.rows, [serde_json::json!({"path": "/secret-page"})]);
    }

    #[tokio::test]
    async fn everything_but_reading_the_views_is_refused() {
        let (_dir, db, sandbox) = sandbox().await;
        let forbidden = [
            "SELECT * FROM requests",
            "SELECT * FROM main.requests",
            "SELECT value FROM settings",
            "SELECT * FROM sessions",
            "SELECT * FROM sqlite_master",
            "SELECT * FROM sw_scope",
            "SELECT (SELECT value FROM settings) FROM v_requests",
            "SELECT * FROM v_requests UNION SELECT key, value, 1,1,1,1,1,1,1,1,1,1,1 FROM settings",
            "DELETE FROM requests",
            "UPDATE requests SET status = 500",
            "INSERT INTO requests (ts) VALUES (1)",
            "DROP TABLE requests",
            "DROP VIEW v_requests",
            "CREATE TABLE x (a)",
            "CREATE TEMP TABLE x AS SELECT * FROM requests",
            "UPDATE sw_scope SET site_id = 2",
            "PRAGMA table_info(requests)",
            "PRAGMA writable_schema = 1",
            "ATTACH DATABASE '/tmp/x.db' AS x",
            "VACUUM INTO '/tmp/copy.db'",
            "SELECT load_extension('x')",
            "SELECT randomblob(1000000000)",
            "SELECT sqlite_version()",
            "SELECT 1; DROP TABLE requests",
            "SELECT * FROM v_requests WHERE path = :other",
            "EXPLAIN SELECT 1",
            "",
        ];
        for sql in forbidden {
            let result = sandbox.query(1, FROM, TO, sql).await;
            assert!(result.is_err(), "{sql:?} should be refused, got {result:?}");
        }
        let leak = sandbox
            .query(1, FROM, TO, "SELECT value FROM settings")
            .await
            .unwrap_err();
        assert!(!leak.contains("TOPSECRET"));
        assert!(
            leak.contains("v_requests"),
            "the error says what is allowed: {leak}"
        );
        // Nothing was changed.
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM requests")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 5);
        // And the sandbox still works afterwards.
        assert!(sandbox.query(1, FROM, TO, "SELECT 1 AS one").await.is_ok());
    }

    #[tokio::test]
    async fn row_and_time_limits_hold() {
        let (_dir, _db, sandbox) = sandbox().await;
        let many = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000) SELECT i FROM n";
        let result = sandbox.query(1, FROM, TO, many).await.unwrap();
        assert_eq!(result.rows.len(), MAX_ROWS);
        assert!(result.truncated);

        let started = Instant::now();
        let endless = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n";
        let err = sandbox.query(1, FROM, TO, endless).await.unwrap_err();
        assert!(err.contains("stopped"), "{err}");
        assert!(started.elapsed() < QUERY_TIMEOUT + Duration::from_secs(3));
        assert!(
            sandbox.query(1, FROM, TO, "SELECT 1 AS one").await.is_ok(),
            "usable after a timeout"
        );
    }

    #[test]
    fn parameters_are_substituted_safely() {
        let sql = prepare_sql(
            "SELECT :site_id, :from, :to, ':site_idx', 'a:b', time('12:30') FROM v_requests;",
            7,
            FROM,
            TO,
        )
        .unwrap_err();
        assert!(sql.contains(":site_idx"), "{sql}");
        let ok = prepare_sql(
            "SELECT :site_id AS s, :from AS f, :to AS t, 'caf\u{e9} 12:' AS x",
            7,
            FROM,
            TO,
        )
        .unwrap();
        assert_eq!(
            ok,
            format!("SELECT 7 AS s, '{FROM}' AS f, '{TO}' AS t, 'caf\u{e9} 12:' AS x")
        );
        assert!(prepare_sql("SELECT 1", 1, "2020'; DROP", TO).is_err());
        assert!(prepare_sql(&format!("SELECT {}", "1,".repeat(3000)), 1, FROM, TO).is_err());
    }
}
