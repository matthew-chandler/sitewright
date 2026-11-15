//! Daily rollups and retention: raw rows are summarised per site and day, then pruned.

use sqlx::SqlitePool;
use sw_core::time::{MS_PER_DAY, now_ms};

/// The SQL condition for "a page view": a successful GET by a browser for a page rather
/// than an asset. Shared by rollups and the metric views.
const PAGE_VIEW: &str = "method = 'GET' AND status < 400 AND ua_class <> 'bot' \
    AND (path LIKE '%/' OR path LIKE '%.html' OR replace(path, rtrim(path, replace(path, '/', '')), '') NOT LIKE '%.%')";

/// Recompute the rollup rows for the UTC day containing `day_start_ms` (which must be
/// midnight). Idempotent: running it again replaces the day's rows. Returns the number
/// of sites summarised.
pub async fn rollup_day(db: &SqlitePool, day_start_ms: i64) -> Result<u64, sqlx::Error> {
    let day_end = day_start_ms + MS_PER_DAY;
    let sites: Vec<i64> = sqlx::query_scalar(
        "SELECT site_id FROM requests WHERE ts >= ?1 AND ts < ?2 UNION SELECT site_id FROM checks WHERE ts >= ?1 AND ts < ?2",
    )
    .bind(day_start_ms)
    .bind(day_end)
    .fetch_all(db)
    .await?;

    let in_day = "site_id = ?1 AND ts >= ?2 AND ts < ?3";
    let counts_sql = format!(
        "SELECT coalesce(sum({PAGE_VIEW}), 0),
                count(DISTINCT CASE WHEN {PAGE_VIEW} THEN visitor_hash END),
                coalesce(sum(status >= 400 AND ua_class <> 'bot'), 0),
                count(*)
         FROM requests WHERE {in_day}"
    );
    for site_id in &sites {
        let (views, visitors, errors, total): (i64, i64, i64, i64) =
            sqlx::query_as(sqlx::AssertSqlSafe(counts_sql.clone()))
                .bind(site_id)
                .bind(day_start_ms)
                .bind(day_end)
                .fetch_one(db)
                .await?;
        // The 95th percentile: the value 95% of the way through the sorted durations.
        let p95: i64 = if total == 0 {
            0
        } else {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT duration_ms FROM requests WHERE {in_day} ORDER BY duration_ms LIMIT 1 OFFSET ?4"
            )))
            .bind(site_id)
            .bind(day_start_ms)
            .bind(day_end)
            .bind((total * 95 - 1) / 100)
            .fetch_one(db)
            .await?
        };
        let uptime: Option<f64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT round(100.0 * avg(ok), 2) FROM checks WHERE {in_day}"
        )))
        .bind(site_id)
        .bind(day_start_ms)
        .bind(day_end)
        .fetch_one(db)
        .await?;
        sqlx::query!(
            "INSERT INTO daily_rollups (site_id, day, views, visitors, errors, p95_ms, uptime_pct)
             VALUES (?1, date(?2 / 1000, 'unixepoch'), ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (site_id, day) DO UPDATE SET
                views = excluded.views, visitors = excluded.visitors, errors = excluded.errors,
                p95_ms = excluded.p95_ms, uptime_pct = excluded.uptime_pct",
            site_id,
            day_start_ms,
            views,
            visitors,
            errors,
            p95,
            uptime
        )
        .execute(db)
        .await?;
    }
    Ok(sites.len() as u64)
}

/// Refresh today's and yesterday's rollups.
pub async fn rollup_recent(db: &SqlitePool) -> Result<(), sqlx::Error> {
    let today = now_ms().div_euclid(MS_PER_DAY) * MS_PER_DAY;
    rollup_day(db, today - MS_PER_DAY).await?;
    rollup_day(db, today).await?;
    Ok(())
}

/// Delete raw requests, events and checks older than `retention_days`, after making sure
/// every day about to be lost has its rollup. Returns the number of rows deleted.
pub async fn prune(db: &SqlitePool, retention_days: u32) -> Result<u64, sqlx::Error> {
    let today = now_ms().div_euclid(MS_PER_DAY) * MS_PER_DAY;
    let cutoff = today - i64::from(retention_days) * MS_PER_DAY;
    // Roll up any old day that still has raw rows (for example after downtime).
    let oldest: Option<i64> = sqlx::query_scalar("SELECT min(ts) FROM (SELECT min(ts) AS ts FROM requests UNION ALL SELECT min(ts) FROM checks)")
        .fetch_one(db)
        .await?;
    if let Some(oldest) = oldest {
        let mut day = oldest.div_euclid(MS_PER_DAY) * MS_PER_DAY;
        while day < cutoff {
            rollup_day(db, day).await?;
            day += MS_PER_DAY;
        }
    }
    let mut deleted = 0;
    for table in ["requests", "events", "checks"] {
        deleted += sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE ts < ?"
        )))
        .bind(cutoff)
        .execute(db)
        .await?
        .rows_affected();
    }
    Ok(deleted)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
mod tests {
    use super::*;

    async fn request(
        db: &SqlitePool,
        ts: i64,
        site: i64,
        path: &str,
        status: i64,
        ua: &str,
        visitor: &str,
        ms: i64,
    ) {
        sqlx::query("INSERT INTO requests (ts, site_id, method, path, status, bytes, duration_ms, ua_class, visitor_hash) VALUES (?, ?, 'GET', ?, ?, 1, ?, ?, ?)")
            .bind(ts).bind(site).bind(path).bind(status).bind(ms).bind(ua).bind(visitor).execute(db).await.unwrap();
    }

    #[tokio::test]
    async fn a_day_is_summarised_per_site_and_recomputed_idempotently() {
        let db = sw_core::db::open_in_memory().await.unwrap();
        let day = 19_000 * MS_PER_DAY;
        for (i, (path, status, ua, visitor)) in [
            ("/", 200, "browser", "a"),
            ("/about", 200, "browser", "a"),
            ("/trails/", 200, "browser", "b"),
            ("/site.css", 200, "browser", "a"),
            ("/img/x.png", 200, "browser", "b"),
            ("/", 200, "bot", "c"),
            ("/missing", 404, "browser", "b"),
            ("/about.html", 304, "browser", "b"),
        ]
        .into_iter()
        .enumerate()
        {
            request(
                &db,
                day + i as i64 * 1000,
                1,
                path,
                status,
                ua,
                visitor,
                (i as i64 + 1) * 10,
            )
            .await;
        }
        request(&db, day + 5, 2, "/", 200, "browser", "z", 7).await;
        request(&db, day - 5, 1, "/", 200, "browser", "yesterday", 7).await;
        for (i, ok) in [1, 1, 1, 0].into_iter().enumerate() {
            sqlx::query("INSERT INTO checks (ts, site_id, ok, latency_ms) VALUES (?, 1, ?, 20)")
                .bind(day + i as i64)
                .bind(ok)
                .execute(&db)
                .await
                .unwrap();
        }

        rollup_day(&db, day).await.unwrap();
        rollup_day(&db, day).await.unwrap();
        let rows: Vec<(i64, String, i64, i64, i64, i64, Option<f64>)> =
            sqlx::query_as("SELECT site_id, day, views, visitors, errors, p95_ms, uptime_pct FROM daily_rollups ORDER BY site_id").fetch_all(&db).await.unwrap();
        assert_eq!(rows.len(), 2);
        // Site 1: four page views (/, /about, /trails/, /about.html) by two visitors; one 404.
        assert_eq!(rows[0], (1, "2022-01-08".into(), 4, 2, 1, 80, Some(75.0)));
        assert_eq!(rows[1], (2, "2022-01-08".into(), 1, 1, 0, 7, None));
    }

    #[tokio::test]
    async fn pruning_keeps_recent_rows_and_rolls_up_what_it_deletes() {
        let db = sw_core::db::open_in_memory().await.unwrap();
        let today = now_ms().div_euclid(MS_PER_DAY) * MS_PER_DAY;
        request(
            &db,
            today - 40 * MS_PER_DAY + 5,
            1,
            "/",
            200,
            "browser",
            "old",
            5,
        )
        .await;
        request(
            &db,
            today - 10 * MS_PER_DAY,
            1,
            "/",
            200,
            "browser",
            "recent",
            5,
        )
        .await;
        sqlx::query("INSERT INTO events (ts, site_id, name, visitor_hash) VALUES (?, 1, 'x', 'v')")
            .bind(today - 40 * MS_PER_DAY)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO checks (ts, site_id, ok, latency_ms) VALUES (?, 1, 1, 5)")
            .bind(today - 40 * MS_PER_DAY)
            .execute(&db)
            .await
            .unwrap();

        assert_eq!(prune(&db, 30).await.unwrap(), 3);
        let (requests, events, checks): (i64, i64, i64) =
            sqlx::query_as("SELECT (SELECT count(*) FROM requests), (SELECT count(*) FROM events), (SELECT count(*) FROM checks)").fetch_one(&db).await.unwrap();
        assert_eq!((requests, events, checks), (1, 0, 0));
        // The deleted day lives on as a rollup.
        let (views, uptime): (i64, Option<f64>) =
            sqlx::query_as("SELECT views, uptime_pct FROM daily_rollups")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!((views, uptime), (1, Some(100.0)));
    }
}
