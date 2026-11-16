//! SQLite access: one pool in WAL mode, with embedded migrations.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

pub use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::error::Result;
use crate::time::now_ms;

/// Migrations embedded at build time from the repository's `migrations/` directory.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Open (creating if needed) the database at `path`.
pub async fn open(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    Ok(pool)
}

/// Start a transaction that will write.
///
/// SQLite's default transaction takes the write lock only at its first write. If the
/// transaction has read first and another connection commits in between, that upgrade
/// fails at once with "database is locked", and the busy timeout does not apply. `BEGIN
/// IMMEDIATE` takes the write lock up front, where the busy timeout does make it wait.
pub async fn begin_write(
    pool: &SqlitePool,
) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, sqlx::Error> {
    pool.begin_with("BEGIN IMMEDIATE").await
}

/// Open an in-memory database with all migrations applied. For tests.
pub async fn open_in_memory() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    // A single connection, never recycled: each in-memory connection is its own database.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

/// Apply pending migrations. If the database already holds data and there is something
/// to apply, a consistent copy is written to `backups_dir` first, so an update can be
/// rolled back. Returns the backup path if one was made.
pub async fn migrate(pool: &SqlitePool, backups_dir: &Path) -> Result<Option<std::path::PathBuf>> {
    let applied = applied_versions(pool).await?;
    let pending = MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .any(|m| !applied.contains(&m.version));

    let mut backup = None;
    if pending && !applied.is_empty() {
        tokio::fs::create_dir_all(backups_dir).await?;
        let path = backups_dir.join(format!("sitewright-{}.db", now_ms()));
        sqlx::query("VACUUM INTO ?")
            .bind(path.to_string_lossy().into_owned())
            .execute(pool)
            .await?;
        tracing::info!(backup = %path.display(), "database backed up before migration");
        backup = Some(path);
    }
    MIGRATOR.run(pool).await?;
    Ok(backup)
}

/// Append a row to the audit log. Failures are logged, never returned: an audit problem
/// must not undo the action it describes.
pub async fn audit(
    pool: &SqlitePool,
    actor: &str,
    action: &str,
    target: Option<&str>,
    detail: Option<&str>,
) {
    let result = sqlx::query(
        "INSERT INTO audit_log (ts, actor, action, target, detail) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(now_ms())
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .execute(pool)
    .await;
    if let Err(error) = result {
        tracing::error!(%error, actor, action, "could not write audit log entry");
    }
}

async fn applied_versions(pool: &SqlitePool) -> Result<Vec<i64>> {
    let exists: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?;
    if exists.is_none() {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT version FROM _sqlx_migrations WHERE success = 1")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_read_then_write_transactions_wait_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open(&dir.path().join("t.db")).await.unwrap();
        migrate(&pool, &dir.path().join("backups")).await.unwrap();
        // Many tasks each read, then write, in one transaction. With deferred
        // transactions some of these fail with "database is locked".
        let tasks: Vec<_> = (0..40)
            .map(|i| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    let mut tx = begin_write(&pool).await?;
                    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM settings")
                        .fetch_one(&mut *tx)
                        .await?;
                    sqlx::query("INSERT INTO settings (key, value, updated_at) VALUES (?, ?, 0)")
                        .bind(format!("k{i}"))
                        .bind(n.to_string())
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM settings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 40);
    }

    #[tokio::test]
    async fn migrates_fresh_database_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open(&dir.path().join("t.db")).await.unwrap();
        let backup = migrate(&pool, &dir.path().join("backups")).await.unwrap();
        assert!(backup.is_none());
        // Second run is a no-op.
        assert!(
            migrate(&pool, &dir.path().join("backups"))
                .await
                .unwrap()
                .is_none()
        );
        let (mode,): (String,) = sqlx::query_as("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM sites")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
}
