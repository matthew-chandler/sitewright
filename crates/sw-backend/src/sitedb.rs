//! A site's own SQLite database.
//!
//! Each site with a backend gets one database file. The SQL that runs on it is written
//! by the agent (handlers and migrations) and reaches the daemon as text, so it is
//! untrusted and cannot use the compile-time-checked `sqlx` macros the rest of the code
//! base requires. What keeps it in bounds:
//!
//! 1. **Its own file and connection.** The connection has no access to Sitewright's
//!    database, and an authorizer refuses `ATTACH`, `PRAGMA` and extension loading, so
//!    it cannot reach another file either.
//! 2. **A mode per statement.** Handlers may read and write rows. Only migrations may
//!    change the schema. Dashboards may only read. Nothing may touch the bookkeeping
//!    table or manage transactions itself.
//! 3. **Limits**: a time limit per statement (a progress handler), a row limit, and a
//!    size limit on the file.
//!
//! Like `sw-metrics::sandbox`, this needs `unsafe` for `sqlite3_set_authorizer`, which
//! sqlx does not wrap.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use libsqlite3_sys as ffi;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqliteRow, SqliteSynchronous,
};
use sqlx::{Column, ConnectOptions, Row, TypeInfo, ValueRef};
use tokio::sync::Mutex;

/// The table that records which migrations ran. Site SQL cannot read or write it.
const MIGRATIONS_TABLE: &str = "_sitewright_migrations";
/// Visitor accounts, managed by Sitewright (see `auth.rs`). Site SQL may read them, except
/// the password hash, so that its own tables can refer to users; it cannot change them.
pub const USERS_TABLE: &str = "sw_users";
/// Visitor sessions. Site SQL cannot touch these at all.
pub const SESSIONS_TABLE: &str = "sw_sessions";
const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone)]
pub struct DbLimits {
    /// Largest the database file may grow.
    pub max_bytes: u64,
    /// Longest one statement may run.
    pub statement_timeout: Duration,
    /// Most rows one query may return.
    pub max_rows: usize,
    pub max_sql_bytes: usize,
}

impl Default for DbLimits {
    fn default() -> Self {
        Self {
            max_bytes: 100 * 1024 * 1024,
            statement_timeout: Duration::from_secs(5),
            max_rows: 5000,
            max_sql_bytes: 20_000,
        }
    }
}

/// What the statement being compiled is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Mode {
    /// Sitewright's own statements: no restrictions.
    Internal = 0,
    /// A request handler: read and write rows.
    Handler = 1,
    /// A migration: also change the schema.
    Migration = 2,
    /// A dashboard query: read only.
    ReadOnly = 3,
}

/// The outcome of a statement that changes rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RunResult {
    pub changes: u64,
    #[serde(rename = "lastInsertRowid")]
    pub last_insert_rowid: i64,
}

/// The rows of a read-only query, for a dashboard panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadRows {
    /// Column names in the query's order. Empty when the query returned no rows.
    pub columns: Vec<String>,
    /// One object per row, keyed by column name.
    pub rows: Vec<Value>,
    /// True when more rows matched than were asked for and the rest were dropped.
    pub truncated: bool,
}

/// One migration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// File name, for example `0001_create_entries.sql`. Migrations run in name order.
    pub name: String,
    pub sql: String,
}

/// One site's database. One statement runs at a time.
#[derive(Debug)]
pub struct SiteDb {
    conn: Mutex<SqliteConnection>,
    /// Shared with the authorizer callback.
    mode: Arc<AtomicU8>,
    limits: DbLimits,
}

impl SiteDb {
    /// Open (creating it if needed) the database at `path`.
    pub async fn open(path: &Path, limits: DbLimits) -> Result<Self, String> {
        Self::open_with(path, limits, false).await
    }

    /// Open an existing database for reading only.
    pub async fn open_read_only(path: &Path, limits: DbLimits) -> Result<Self, String> {
        Self::open_with(path, limits, true).await
    }

    async fn open_with(path: &Path, limits: DbLimits, read_only: bool) -> Result<Self, String> {
        let failed = |e: sqlx::Error| format!("could not open the site database: {e}");
        let mut options = SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        options = if read_only {
            options.read_only(true)
        } else {
            options
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
        };
        let mut conn = options.connect().await.map_err(failed)?;
        if !read_only {
            let pages = (limits.max_bytes / PAGE_SIZE).max(16);
            // PRAGMA values cannot be bound; `pages` is a number we computed.
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "PRAGMA max_page_count = {pages};
                 CREATE TABLE IF NOT EXISTS {MIGRATIONS_TABLE} (
                     name TEXT PRIMARY KEY, sha256 TEXT NOT NULL, applied_at INTEGER NOT NULL
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS {USERS_TABLE} (
                     id INTEGER PRIMARY KEY,
                     email TEXT NOT NULL UNIQUE COLLATE NOCASE,
                     password_hash TEXT NOT NULL,
                     created_at INTEGER NOT NULL
                 ) STRICT;
                 CREATE TABLE IF NOT EXISTS {SESSIONS_TABLE} (
                     token_hash TEXT PRIMARY KEY,
                     user_id INTEGER NOT NULL REFERENCES {USERS_TABLE} (id) ON DELETE CASCADE,
                     created_at INTEGER NOT NULL,
                     expires_at INTEGER NOT NULL
                 ) STRICT;
                 CREATE INDEX IF NOT EXISTS sw_sessions_user ON {SESSIONS_TABLE} (user_id);"
            )))
            .execute(&mut conn)
            .await
            .map_err(failed)?;
        }
        let mode = Arc::new(AtomicU8::new(Mode::Internal as u8));
        {
            let mut handle = conn.lock_handle().await.map_err(failed)?;
            // SAFETY: `handle` is a valid, locked connection. The user-data pointer is
            // the `AtomicU8` inside `mode`, which this struct keeps alive for as long
            // as the connection exists, and the callback only reads it.
            let rc = unsafe {
                ffi::sqlite3_set_authorizer(
                    handle.as_raw_handle().as_ptr(),
                    Some(authorize),
                    Arc::as_ptr(&mode) as *mut c_void,
                )
            };
            if rc != ffi::SQLITE_OK {
                return Err(format!("could not install the SQL authorizer (code {rc})"));
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
            mode,
            limits,
        })
    }

    fn check_sql(&self, sql: &str) -> Result<(), String> {
        if sql.trim().is_empty() {
            return Err("the SQL is empty".into());
        }
        if sql.len() > self.limits.max_sql_bytes {
            return Err(format!(
                "the SQL is longer than {} bytes",
                self.limits.max_sql_bytes
            ));
        }
        Ok(())
    }

    /// Run a statement from a handler and return its rows as objects.
    pub async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, String> {
        self.check_sql(sql)?;
        let mut conn = self.conn.lock().await;
        {
            let deadline = self.arm(&mut conn, Mode::Handler).await;
            let outcome = fetch(&mut conn, sql, params, self.limits.max_rows).await;
            self.disarm(&mut conn, outcome, deadline).await
        }
    }

    /// Run a read-only query (for dashboards).
    pub async fn query_read_only(&self, sql: &str, params: &[Value]) -> Result<Vec<Value>, String> {
        self.check_sql(sql)?;
        let mut conn = self.conn.lock().await;
        {
            let deadline = self.arm(&mut conn, Mode::ReadOnly).await;
            let outcome = fetch(&mut conn, sql, params, self.limits.max_rows).await;
            self.disarm(&mut conn, outcome, deadline).await
        }
    }

    /// Run a read-only query for a dashboard panel: like [`Self::query_read_only`], but
    /// the column names come back in the order the query gives them, and a result
    /// longer than `max_rows` is cut short and marked instead of being an error.
    pub async fn query_read_only_with_columns(
        &self,
        sql: &str,
        params: &[Value],
        max_rows: usize,
    ) -> Result<ReadRows, String> {
        self.check_sql(sql)?;
        let max_rows = max_rows.min(self.limits.max_rows);
        let mut conn = self.conn.lock().await;
        let deadline = self.arm(&mut conn, Mode::ReadOnly).await;
        let outcome = fetch_with_columns(&mut conn, sql, params, max_rows).await;
        self.disarm(&mut conn, outcome, deadline)
            .await
            .map_err(|error| {
                // `explain` words a refusal for handler code; say what holds here.
                if error.starts_with("not allowed here") {
                    format!(
                        "not allowed: a dashboard query may only SELECT from this site's own tables; \
                         it cannot change anything, and visitor sessions and password hashes cannot be read ({error})"
                    )
                } else {
                    error
                }
            })
    }

    /// Run a statement from a handler that changes rows.
    pub async fn run(&self, sql: &str, params: &[Value]) -> Result<RunResult, String> {
        self.check_sql(sql)?;
        let mut conn = self.conn.lock().await;
        {
            let deadline = self.arm(&mut conn, Mode::Handler).await;
            let outcome = execute(&mut conn, sql, params).await;
            self.disarm(&mut conn, outcome, deadline).await
        }
    }

    /// Run several statements from a handler in one transaction: all or nothing.
    pub async fn batch(
        &self,
        statements: &[(String, Vec<Value>)],
    ) -> Result<Vec<RunResult>, String> {
        for (sql, _) in statements {
            self.check_sql(sql)?;
        }
        let mut conn = self.conn.lock().await;
        internal(&mut conn, "BEGIN IMMEDIATE").await?;
        let mut results = Vec::with_capacity(statements.len());
        for (sql, params) in statements {
            let step = {
                let deadline = self.arm(&mut conn, Mode::Handler).await;
                let outcome = execute(&mut conn, sql, params).await;
                self.disarm(&mut conn, outcome, deadline).await
            };
            match step {
                Ok(result) => results.push(result),
                Err(e) => {
                    let _ = internal(&mut conn, "ROLLBACK").await;
                    return Err(e);
                }
            }
        }
        internal(&mut conn, "COMMIT").await?;
        Ok(results)
    }

    /// Apply the migrations that have not run yet, in name order, each in its own
    /// transaction. Returns the names that were applied. A migration that was changed
    /// after it ran is an error: the fix is a new migration.
    pub async fn migrate(&self, migrations: &[Migration]) -> Result<Vec<String>, String> {
        let mut ordered: Vec<&Migration> = migrations.iter().collect();
        ordered.sort_by(|a, b| a.name.cmp(&b.name));
        let mut conn = self.conn.lock().await;

        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT name, sha256 FROM {MIGRATIONS_TABLE}"
        )))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| format!("could not read the migration history: {e}"))?;
        let applied: HashMap<String, String> = rows
            .iter()
            .filter_map(|r| Some((r.try_get(0).ok()?, r.try_get(1).ok()?)))
            .collect();
        // An applied migration whose file is missing is tolerated: after a rollback the
        // database is ahead of the code, which is safe as long as migrations only add.
        let mut done = Vec::new();
        for migration in ordered {
            let digest = hex(&Sha256::digest(migration.sql.as_bytes()));
            match applied.get(&migration.name) {
                Some(recorded) if *recorded == digest => continue,
                Some(_) => {
                    return Err(format!(
                        "migration {} was changed after it ran; restore it and put the change in a new migration",
                        migration.name
                    ));
                }
                None => {}
            }
            internal(&mut conn, "BEGIN IMMEDIATE").await?;
            let ran = {
                let deadline = self.arm(&mut conn, Mode::Migration).await;
                let outcome = sqlx::raw_sql(sqlx::AssertSqlSafe(migration.sql.clone()))
                    .execute(&mut *conn)
                    .await
                    .map(|_| ());
                self.disarm(&mut conn, outcome, deadline).await
            };
            if let Err(e) = ran {
                let _ = internal(&mut conn, "ROLLBACK").await;
                return Err(format!("migration {} failed: {e}", migration.name));
            }
            let recorded = sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {MIGRATIONS_TABLE} (name, sha256, applied_at) VALUES (?, ?, ?)"
            )))
            .bind(&migration.name)
            .bind(&digest)
            .bind(now_ms())
            .execute(&mut *conn)
            .await;
            if let Err(e) = recorded {
                let _ = internal(&mut conn, "ROLLBACK").await;
                return Err(format!(
                    "could not record migration {}: {e}",
                    migration.name
                ));
            }
            internal(&mut conn, "COMMIT").await?;
            done.push(migration.name.clone());
        }
        Ok(done)
    }

    /// Run one of Sitewright's own statements on this database, with parameters. For
    /// the visitor-account code; never given SQL that came from a site.
    pub(crate) async fn internal_query(
        &self,
        sql: &'static str,
        params: &[Value],
    ) -> Result<Vec<Value>, String> {
        let mut conn = self.conn.lock().await;
        fetch(&mut conn, sql, params, 1000)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db) if db.is_unique_violation() => "unique".to_owned(),
                _ => format!("database error: {e}"),
            })
    }

    /// The tables and views with their `CREATE` statements, for showing the agent.
    pub async fn schema(&self) -> Result<Vec<Value>, String> {
        let mut conn = self.conn.lock().await;
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT type, name, sql FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' AND name <> '{MIGRATIONS_TABLE}'
               AND name NOT LIKE '{SESSIONS_TABLE}%' AND name <> '{USERS_TABLE}'
               AND name NOT LIKE 'sqlite_autoindex_%' AND sql IS NOT NULL
             ORDER BY type DESC, name"
        )))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| format!("could not read the schema: {e}"))?;
        Ok(rows.iter().map(row_to_json).collect())
    }

    /// Write a consistent copy of the database to `dest`, which must not exist.
    pub async fn copy_to(&self, dest: &Path) -> Result<(), String> {
        let dest = dest
            .to_str()
            .ok_or("the destination path is not valid UTF-8")?;
        let mut conn = self.conn.lock().await;
        sqlx::query("VACUUM INTO ?")
            .bind(dest)
            .execute(&mut *conn)
            .await
            .map(|_| ())
            .map_err(|e| format!("could not copy the site database: {e}"))
    }

    /// Put the authorizer in `mode` and start the statement time limit.
    async fn arm(&self, conn: &mut SqliteConnection, mode: Mode) -> Instant {
        let deadline = Instant::now() + self.limits.statement_timeout;
        if let Ok(mut handle) = conn.lock_handle().await {
            handle.set_progress_handler(1000, move || Instant::now() < deadline);
        }
        self.mode.store(mode as u8, Ordering::SeqCst);
        deadline
    }

    /// Undo [`Self::arm`] and explain the statement's error, if any.
    async fn disarm<T>(
        &self,
        conn: &mut SqliteConnection,
        outcome: Result<T, sqlx::Error>,
        deadline: Instant,
    ) -> Result<T, String> {
        self.mode.store(Mode::Internal as u8, Ordering::SeqCst);
        if let Ok(mut handle) = conn.lock_handle().await {
            handle.remove_progress_handler();
        }
        outcome.map_err(|e| self.explain(&e, Instant::now() >= deadline))
    }

    /// Turn a database error into something the agent (or a person) can act on.
    fn explain(&self, error: &sqlx::Error, timed_out: bool) -> String {
        let text = match error {
            sqlx::Error::Database(e) => e.message().to_owned(),
            sqlx::Error::Protocol(message) => message.clone(),
            other => other.to_string(),
        };
        if timed_out || text.contains("interrupted") {
            return format!(
                "the statement took longer than {} seconds and was stopped",
                self.limits.statement_timeout.as_secs()
            );
        }
        if text.contains("not authorized") || text.contains("prohibited") {
            return format!(
                "not allowed here ({text}). Handlers may read and write rows; schema changes belong in \
                 backend/migrations/; ATTACH, PRAGMA and transaction statements are never available \
                 (use db.batch for a transaction)"
            );
        }
        if text.contains("database or disk is full") {
            return format!(
                "the site database reached its size limit of {} MB",
                self.limits.max_bytes / (1024 * 1024)
            );
        }
        format!("SQL error: {text}")
    }
}

/// Run one of Sitewright's own statements.
async fn internal(conn: &mut SqliteConnection, sql: &'static str) -> Result<(), String> {
    sqlx::raw_sql(sql)
        .execute(conn)
        .await
        .map(|_| ())
        .map_err(|e| format!("database error: {e}"))
}

fn bind<'q>(
    mut query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>,
    params: &[Value],
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments> {
    for param in params {
        query = match param {
            Value::Null => query.bind(None::<i64>),
            Value::Bool(b) => query.bind(i64::from(*b)),
            Value::Number(n) => match n.as_i64() {
                Some(i) => query.bind(i),
                None => query.bind(n.as_f64().unwrap_or(0.0)),
            },
            Value::String(s) => query.bind(s.clone()),
            // Arrays and objects are stored as JSON text.
            other => query.bind(other.to_string()),
        };
    }
    query
}

async fn fetch(
    conn: &mut SqliteConnection,
    sql: &str,
    params: &[Value],
    max_rows: usize,
) -> Result<Vec<Value>, sqlx::Error> {
    // The text is untrusted by design: the authorizer, not string checks, is what makes
    // running it safe.
    let query = bind(
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned())).persistent(false),
        params,
    );
    let mut rows = Vec::new();
    let mut stream = query.fetch(conn);
    while let Some(row) = stream.try_next().await? {
        if rows.len() >= max_rows {
            return Err(sqlx::Error::Protocol(format!(
                "the query returned more than {max_rows} rows; add a LIMIT"
            )));
        }
        rows.push(row_to_json(&row));
    }
    Ok(rows)
}

/// Like [`fetch`], keeping the column order and stopping quietly at `max_rows`.
async fn fetch_with_columns(
    conn: &mut SqliteConnection,
    sql: &str,
    params: &[Value],
    max_rows: usize,
) -> Result<ReadRows, sqlx::Error> {
    let query = bind(
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned())).persistent(false),
        params,
    );
    let mut result = ReadRows::default();
    let mut stream = query.fetch(conn);
    while let Some(row) = stream.try_next().await? {
        // The names are taken from the statement, not from a row object's keys: a JSON
        // object here does not keep the order its keys were added in.
        if result.columns.is_empty() {
            result.columns = row.columns().iter().map(|c| c.name().to_owned()).collect();
        }
        if result.rows.len() >= max_rows {
            result.truncated = true;
            break;
        }
        result.rows.push(row_to_json(&row));
    }
    Ok(result)
}

async fn execute(
    conn: &mut SqliteConnection,
    sql: &str,
    params: &[Value],
) -> Result<RunResult, sqlx::Error> {
    let done = bind(
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned())).persistent(false),
        params,
    )
    .execute(conn)
    .await?;
    Ok(RunResult {
        changes: done.rows_affected(),
        last_insert_rowid: done.last_insert_rowid(),
    })
}

fn row_to_json(row: &SqliteRow) -> Value {
    let mut object = serde_json::Map::new();
    for (i, column) in row.columns().iter().enumerate() {
        // A NULL in a typed column reports the column's type, so ask the value itself.
        let kind = row
            .try_get_raw(i)
            .map(|v| {
                if v.is_null() {
                    "NULL".to_owned()
                } else {
                    v.type_info().name().to_owned()
                }
            })
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// SQLite's authorizer callback. Called while a statement is compiled, once per thing
/// the statement wants to do.
unsafe extern "C" fn authorize(
    user_data: *mut c_void,
    action: c_int,
    arg1: *const c_char,
    arg2: *const c_char,
    _database: *const c_char,
    _accessor: *const c_char,
) -> c_int {
    // SAFETY: `user_data` is the `AtomicU8` registered in `SiteDb::open_with`, alive as
    // long as the connection.
    let mode = unsafe { &*(user_data as *const AtomicU8) }.load(Ordering::SeqCst);
    if mode == Mode::Internal as u8 {
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
    let bookkeeping = |table: &str| table.eq_ignore_ascii_case(MIGRATIONS_TABLE);
    let managed = |table: &str| {
        table.eq_ignore_ascii_case(USERS_TABLE) || table.eq_ignore_ascii_case(SESSIONS_TABLE)
    };
    let writes = mode == Mode::Handler as u8 || mode == Mode::Migration as u8;
    let schema = mode == Mode::Migration as u8;
    let allowed = match action {
        ffi::SQLITE_SELECT | ffi::SQLITE_RECURSIVE => true,
        // arg1 is the table.
        // arg1 is the table, arg2 the column.
        ffi::SQLITE_READ => {
            let table = text(arg1);
            !bookkeeping(table)
                && !table.eq_ignore_ascii_case(SESSIONS_TABLE)
                && !(table.eq_ignore_ascii_case(USERS_TABLE)
                    && text(arg2).eq_ignore_ascii_case("password_hash"))
        }
        ffi::SQLITE_INSERT | ffi::SQLITE_UPDATE | ffi::SQLITE_DELETE if managed(text(arg1)) => {
            false
        }
        ffi::SQLITE_INSERT | ffi::SQLITE_UPDATE | ffi::SQLITE_DELETE => {
            let table = text(arg1);
            // Writes to the schema table are how SQLite carries out DDL.
            let is_schema_table = table.starts_with("sqlite_");
            writes && !bookkeeping(table) && (schema || !is_schema_table)
        }
        // arg2 is the function name.
        ffi::SQLITE_FUNCTION => !text(arg2).eq_ignore_ascii_case("load_extension"),
        ffi::SQLITE_CREATE_TABLE
        | ffi::SQLITE_CREATE_INDEX
        | ffi::SQLITE_CREATE_VIEW
        | ffi::SQLITE_DROP_TABLE
        | ffi::SQLITE_DROP_INDEX
        | ffi::SQLITE_DROP_VIEW
        | ffi::SQLITE_DROP_TRIGGER
        | ffi::SQLITE_ALTER_TABLE
        | ffi::SQLITE_REINDEX
        | ffi::SQLITE_ANALYZE => {
            schema
                && !bookkeeping(text(arg1))
                && !bookkeeping(text(arg2))
                && !managed(text(arg1))
                && !managed(text(arg2))
        }
        // Triggers are refused even in migrations: a trigger's body runs with the
        // rights of whatever statement fires it, including Sitewright's own statements
        // on the managed tables, which would let site SQL read password hashes.
        // Also refused: ATTACH, DETACH, PRAGMA, transactions, savepoints, temp objects,
        // virtual tables.
        _ => false,
    };
    if allowed {
        ffi::SQLITE_OK
    } else {
        ffi::SQLITE_DENY
    }
}

/// The open site databases, one connection per file.
#[derive(Debug, Default)]
pub struct SiteDbs {
    limits: DbLimits,
    open: Mutex<HashMap<PathBuf, Arc<SiteDb>>>,
}

impl SiteDbs {
    pub fn new(limits: DbLimits) -> Self {
        Self {
            limits,
            open: Mutex::new(HashMap::new()),
        }
    }

    pub fn limits(&self) -> &DbLimits {
        &self.limits
    }

    /// The database at `path`, opened (and created) on first use.
    pub async fn get(&self, path: &Path) -> Result<Arc<SiteDb>, String> {
        let mut open = self.open.lock().await;
        if let Some(db) = open.get(path) {
            return Ok(db.clone());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create the site's data directory: {e}"))?;
        }
        let db = Arc::new(SiteDb::open(path, self.limits.clone()).await?);
        open.insert(path.to_owned(), db.clone());
        Ok(db)
    }

    /// Close the connection to `path`, for example before its file is replaced or moved.
    pub async fn close(&self, path: &Path) {
        self.open.lock().await.remove(path);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use serde_json::json;

    use super::*;

    fn migration(name: &str, sql: &str) -> Migration {
        Migration {
            name: name.into(),
            sql: sql.into(),
        }
    }

    async fn db_with(limits: DbLimits) -> (tempfile::TempDir, SiteDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = SiteDb::open(&dir.path().join("site.db"), limits)
            .await
            .unwrap();
        db.migrate(&[migration(
            "0001_entries.sql",
            "CREATE TABLE entries (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, meta TEXT);
             CREATE INDEX entries_name ON entries (name);",
        )])
        .await
        .unwrap();
        (dir, db)
    }

    async fn db() -> (tempfile::TempDir, SiteDb) {
        db_with(DbLimits::default()).await
    }

    #[tokio::test]
    async fn handlers_read_and_write_rows_with_parameters() {
        let (_dir, db) = db().await;
        let first = db
            .run(
                "INSERT INTO entries (name, score, meta) VALUES (?, ?, ?)",
                &[json!("Ada"), json!(9.5), json!({"tags": ["a"]})],
            )
            .await
            .unwrap();
        assert_eq!(
            first,
            RunResult {
                changes: 1,
                last_insert_rowid: 1
            }
        );
        db.run(
            "INSERT INTO entries (name, score) VALUES (?, ?)",
            &[json!("Bo'; DROP TABLE entries;--"), json!(null)],
        )
        .await
        .unwrap();
        let rows = db
            .query(
                "SELECT id, name, score, meta, score IS NULL AS missing FROM entries ORDER BY id",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            rows[0],
            json!({"id": 1, "name": "Ada", "score": 9.5, "meta": "{\"tags\":[\"a\"]}", "missing": 0})
        );
        assert_eq!(rows[1]["name"], "Bo'; DROP TABLE entries;--");
        assert_eq!(rows[1]["score"], Value::Null);
        // RETURNING works through query().
        let back = db
            .query(
                "UPDATE entries SET score = ? WHERE id = ? RETURNING score",
                &[json!(true), json!(2)],
            )
            .await
            .unwrap();
        assert_eq!(back, [json!({"score": 1.0})]);
        assert!(
            db.query("SELECT * FROM nope", &[])
                .await
                .unwrap_err()
                .starts_with("SQL error")
        );
    }

    #[tokio::test]
    async fn handlers_cannot_change_the_schema_or_leave_the_database() {
        let (dir, db) = db().await;
        let other = dir.path().join("other.db");
        for sql in [
            "CREATE TABLE x (a)".to_owned(),
            "DROP TABLE entries".to_owned(),
            "ALTER TABLE entries ADD COLUMN z".to_owned(),
            "CREATE TEMP TABLE t (a)".to_owned(),
            format!("ATTACH DATABASE '{}' AS o", other.display()),
            "PRAGMA max_page_count = 1000000".to_owned(),
            "PRAGMA writable_schema = 1".to_owned(),
            "BEGIN".to_owned(),
            "COMMIT".to_owned(),
            "SAVEPOINT s".to_owned(),
            "VACUUM".to_owned(),
            "SELECT load_extension('x')".to_owned(),
            "SELECT * FROM _sitewright_migrations".to_owned(),
            "DELETE FROM _sitewright_migrations".to_owned(),
            "UPDATE sqlite_master SET sql = ''".to_owned(),
            "SELECT 1; DROP TABLE entries".to_owned(),
        ] {
            let query = db.query(&sql, &[]).await;
            let run = db.run(&sql, &[]).await;
            assert!(query.is_err() && run.is_err(), "allowed: {sql}");
        }
        assert!(!other.exists());
        let err = db.run("CREATE TABLE x (a)", &[]).await.unwrap_err();
        assert!(
            err.contains("schema changes belong in backend/migrations/"),
            "{err}"
        );
        // The table is still there and still writable.
        db.run("INSERT INTO entries (name) VALUES ('ok')", &[])
            .await
            .unwrap();
        // Dashboards cannot write at all.
        assert!(
            db.query_read_only("SELECT count(*) AS n FROM entries", &[])
                .await
                .is_ok()
        );
        assert!(
            db.query_read_only("DELETE FROM entries", &[])
                .await
                .is_err()
        );
        assert!(
            db.query_read_only("INSERT INTO entries (name) VALUES ('x') RETURNING id", &[])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn dashboard_queries_keep_column_order_and_are_cut_short_not_refused() {
        let (_dir, db) = db().await;
        for name in ["zoe", "abe", "max"] {
            db.run("INSERT INTO entries (name) VALUES (?)", &[json!(name)])
                .await
                .unwrap();
        }
        // Columns come back as the query orders them, not sorted as a row's keys are.
        let sql = "SELECT name AS who, id AS entry, NULL AS gap, 1.5 AS average FROM entries WHERE id >= ? ORDER BY id";
        let all = db
            .query_read_only_with_columns(sql, &[json!(2)], 10)
            .await
            .unwrap();
        assert_eq!(all.columns, ["who", "entry", "gap", "average"]);
        assert_eq!(
            all.rows,
            [
                json!({"who": "abe", "entry": 2, "gap": null, "average": 1.5}),
                json!({"who": "max", "entry": 3, "gap": null, "average": 1.5}),
            ]
        );
        assert!(!all.truncated);

        // Exactly the cap is not truncation; one more row is.
        let exact = db
            .query_read_only_with_columns(sql, &[json!(2)], 2)
            .await
            .unwrap();
        assert_eq!((exact.rows.len(), exact.truncated), (2, false));
        let cut = db
            .query_read_only_with_columns(sql, &[json!(1)], 2)
            .await
            .unwrap();
        assert_eq!((cut.rows.len(), cut.truncated), (2, true));
        assert_eq!(cut.rows[0]["who"], "zoe");

        let none = db
            .query_read_only_with_columns(sql, &[json!(99)], 10)
            .await
            .unwrap();
        assert_eq!(none, ReadRows::default());

        // The same guard as every other dashboard query, with an error that fits.
        let other = _dir.path().join("other.db");
        for sql in [
            "DELETE FROM entries".to_owned(),
            "INSERT INTO entries (name) VALUES ('x') RETURNING id".to_owned(),
            "UPDATE entries SET name = 'x'".to_owned(),
            "DROP TABLE entries".to_owned(),
            "CREATE TABLE x (a)".to_owned(),
            "PRAGMA table_info(entries)".to_owned(),
            format!("ATTACH DATABASE '{}' AS o", other.display()),
            "SELECT * FROM sw_sessions".to_owned(),
            "SELECT count(*) FROM sw_sessions".to_owned(),
            "SELECT password_hash FROM sw_users".to_owned(),
            "SELECT * FROM sw_users".to_owned(),
            "SELECT * FROM _sitewright_migrations".to_owned(),
            "SELECT 1; DELETE FROM entries".to_owned(),
        ] {
            let error = db
                .query_read_only_with_columns(&sql, &[], 10)
                .await
                .unwrap_err();
            assert!(
                error.starts_with("not allowed: a dashboard"),
                "{sql}: {error}"
            );
        }
        assert!(!other.exists());
        let users = db
            .query_read_only_with_columns("SELECT id, email, created_at FROM sw_users", &[], 10)
            .await
            .unwrap();
        assert!(users.rows.is_empty());
        let count = db
            .query_read_only_with_columns("SELECT count(*) AS n FROM entries", &[], 10)
            .await
            .unwrap();
        assert_eq!(count.rows, [json!({"n": 3})]);
    }

    #[tokio::test]
    async fn batches_are_all_or_nothing() {
        let (_dir, db) = db().await;
        let insert = |name: &str| {
            (
                "INSERT INTO entries (name) VALUES (?)".to_owned(),
                vec![json!(name)],
            )
        };
        let results = db.batch(&[insert("a"), insert("b")]).await.unwrap();
        assert_eq!(results[1].last_insert_rowid, 2);
        let failed = db
            .batch(&[
                insert("c"),
                (
                    "INSERT INTO entries (name) VALUES (NULL)".to_owned(),
                    vec![],
                ),
            ])
            .await;
        assert!(failed.is_err());
        let count = db
            .query("SELECT count(*) AS n FROM entries", &[])
            .await
            .unwrap();
        assert_eq!(count, [json!({"n": 2})]);
        // The connection is usable afterwards (no transaction left open).
        db.batch(&[insert("d")]).await.unwrap();
    }

    #[tokio::test]
    async fn migrations_run_once_in_order_and_must_not_change() {
        let (_dir, db) = db().await;
        let first = migration(
            "0001_entries.sql",
            "CREATE TABLE entries (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, meta TEXT);
             CREATE INDEX entries_name ON entries (name);",
        );
        let second = migration(
            "0002_votes.sql",
            "CREATE TABLE votes (id INTEGER PRIMARY KEY); ALTER TABLE entries ADD COLUMN votes INTEGER NOT NULL DEFAULT 0;",
        );
        let third = migration(
            "0003_bad.sql",
            "CREATE TABLE half (a); CREATE TABLE entries (dup);",
        );

        // Given out of order; already applied ones are skipped.
        let applied = db.migrate(&[second.clone(), first.clone()]).await.unwrap();
        assert_eq!(applied, ["0002_votes.sql"]);
        assert!(
            db.migrate(&[first.clone(), second.clone()])
                .await
                .unwrap()
                .is_empty()
        );

        // A failing migration leaves nothing behind and is not recorded.
        let err = db
            .migrate(&[first.clone(), second.clone(), third])
            .await
            .unwrap_err();
        assert!(err.contains("0003_bad.sql failed"), "{err}");
        assert!(db.query("SELECT * FROM half", &[]).await.is_err());

        // Changed or missing history is refused.
        let edited = migration("0002_votes.sql", "CREATE TABLE votes (id);");
        let err = db.migrate(&[first.clone(), edited]).await.unwrap_err();
        assert!(err.contains("was changed after it ran"), "{err}");
        // Older code (after a rollback) may lack newer migration files: that is fine.
        assert!(
            db.migrate(std::slice::from_ref(&first))
                .await
                .unwrap()
                .is_empty()
        );

        // Migrations cannot escape either.
        for sql in [
            "PRAGMA foreign_keys = OFF",
            "ATTACH DATABASE '/tmp/x.db' AS x",
            "COMMIT; CREATE TABLE y (a)",
            "DROP TABLE _sitewright_migrations",
        ] {
            let bad = migration("0003_escape.sql", sql);
            assert!(
                db.migrate(&[first.clone(), second.clone(), bad])
                    .await
                    .is_err(),
                "allowed: {sql}"
            );
        }

        let schema = db.schema().await.unwrap();
        let names: Vec<&str> = schema.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["entries", "votes", "entries_name"]);
        assert!(schema[0]["sql"].as_str().unwrap().contains("votes INTEGER"));
    }

    #[tokio::test]
    async fn time_row_and_size_limits_hold() {
        let (_dir, db) = db_with(DbLimits {
            max_bytes: 256 * 1024,
            statement_timeout: Duration::from_millis(200),
            max_rows: 10,
            max_sql_bytes: 200,
        })
        .await;
        let started = Instant::now();
        let err = db
            .query("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c", &[])
            .await
            .unwrap_err();
        assert!(err.contains("took longer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(3));

        let err = db
            .query("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 50) SELECT x FROM c", &[])
            .await
            .unwrap_err();
        assert!(err.contains("more than 10 rows"), "{err}");
        assert!(
            db.query(&format!("SELECT '{}'", "x".repeat(300)), &[])
                .await
                .unwrap_err()
                .contains("longer than")
        );

        let big = "x".repeat(60_000);
        let mut full = None;
        for _ in 0..20 {
            if let Err(e) = db
                .run("INSERT INTO entries (name) VALUES (?)", &[json!(big)])
                .await
            {
                full = Some(e);
                break;
            }
        }
        assert!(full.unwrap().contains("size limit"));
        // Still readable, and deleting frees room again.
        db.run("DELETE FROM entries", &[]).await.unwrap();
        db.run("INSERT INTO entries (name) VALUES ('small')", &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn copies_are_complete_and_registry_reuses_connections() {
        let dir = tempfile::tempdir().unwrap();
        let dbs = SiteDbs::new(DbLimits::default());
        let path = dir.path().join("data").join("site.db");
        let db = dbs.get(&path).await.unwrap();
        assert!(Arc::ptr_eq(&db, &dbs.get(&path).await.unwrap()));
        db.migrate(&[migration("0001.sql", "CREATE TABLE t (a)")])
            .await
            .unwrap();
        db.run("INSERT INTO t VALUES (1)", &[]).await.unwrap();

        let copy = dir.path().join("copy.db");
        db.copy_to(&copy).await.unwrap();
        let copied = SiteDb::open(&copy, DbLimits::default()).await.unwrap();
        assert_eq!(
            copied.query("SELECT a FROM t", &[]).await.unwrap(),
            [json!({"a": 1})]
        );
        // The copy knows its history: the same migration is not applied twice.
        assert!(
            copied
                .migrate(&[migration("0001.sql", "CREATE TABLE t (a)")])
                .await
                .unwrap()
                .is_empty()
        );

        let read_only = SiteDb::open_read_only(&copy, DbLimits::default())
            .await
            .unwrap();
        assert!(
            read_only
                .query_read_only("SELECT a FROM t", &[])
                .await
                .is_ok()
        );
        dbs.close(&path).await;
        assert!(!Arc::ptr_eq(&db, &dbs.get(&path).await.unwrap()));
    }
}
