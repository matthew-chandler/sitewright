//! Uptime checks: fetch each site's public URL on a timer, the way a visitor would, and
//! derive a status from the most recent results.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use sqlx::SqlitePool;
use sw_core::time::{MS_PER_DAY, now_ms};
use tokio_util::sync::CancellationToken;

/// A certificate with fewer days left than this marks the site as degraded.
pub const CERT_WARN_DAYS: i64 = 14;

/// One site to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub site_id: i64,
    pub hostname: String,
    /// The public URL of the site's front page.
    pub url: String,
}

/// Where the list of sites to check comes from (the routing table, in the daemon).
pub type Targets = Arc<dyn Fn() -> Vec<Target> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct UptimeOptions {
    pub interval: Duration,
    pub slow_ms: u64,
    /// When set, connect to this address instead of resolving the hostname (local dev).
    pub resolve_to: Option<SocketAddr>,
}

/// The outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub ok: bool,
    pub latency_ms: u64,
    pub status: Option<u16>,
    pub cert_days: Option<i64>,
    pub detail: String,
}

/// Fetch a site's front page once.
pub async fn check(target: &Target, resolve_to: Option<SocketAddr>) -> CheckResult {
    sw_core::crypto::install_default_provider();
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(3))
        .tls_info(true)
        .no_proxy()
        .user_agent(concat!("sitewright-uptime/", env!("CARGO_PKG_VERSION")));
    if let Some(address) = resolve_to {
        builder = builder.resolve(&target.hostname, address);
    }
    let started = Instant::now();
    let outcome = match builder.build() {
        Ok(client) => client.get(&target.url).send().await,
        Err(e) => Err(e),
    };
    let latency_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(response) => {
            let status = response.status();
            let cert_days = response
                .extensions()
                .get::<reqwest::tls::TlsInfo>()
                .and_then(|info| info.peer_certificate())
                .and_then(certificate_days_left);
            CheckResult {
                ok: status.is_success(),
                latency_ms,
                status: Some(status.as_u16()),
                cert_days,
                detail: if status.is_success() {
                    String::new()
                } else {
                    format!("HTTP {status}")
                },
            }
        }
        Err(e) => {
            let detail = if e.is_timeout() {
                "timed out".to_owned()
            } else if e.is_connect() {
                "could not connect".to_owned()
            } else {
                "request failed".to_owned()
            };
            CheckResult {
                ok: false,
                latency_ms,
                status: None,
                cert_days: None,
                detail,
            }
        }
    }
}

/// Whole days until a DER-encoded certificate expires (negative once expired).
fn certificate_days_left(der: &[u8]) -> Option<i64> {
    let (_, certificate) = x509_parser::parse_x509_certificate(der).ok()?;
    let not_after = certificate.validity().not_after.timestamp();
    Some((not_after - now_ms() / 1000).div_euclid(86_400))
}

/// Store a check result.
pub async fn record(
    db: &SqlitePool,
    site_id: i64,
    result: &CheckResult,
) -> Result<(), sqlx::Error> {
    let (now, ok, latency, status) = (
        now_ms(),
        i64::from(result.ok),
        result.latency_ms as i64,
        result.status.map(i64::from),
    );
    sqlx::query!(
        "INSERT INTO checks (ts, site_id, kind, ok, latency_ms, status, cert_days, detail) VALUES (?1, ?2, 'uptime', ?3, ?4, ?5, ?6, ?7)",
        now,
        site_id,
        ok,
        latency,
        status,
        result.cert_days,
        result.detail
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Check every target once, concurrently, and store the results.
pub async fn check_all(db: &SqlitePool, targets: &[Target], resolve_to: Option<SocketAddr>) {
    let checks = targets
        .iter()
        .map(|target| async move { (target.site_id, check(target, resolve_to).await) });
    for (site_id, result) in futures_util::future::join_all(checks).await {
        if let Err(error) = record(db, site_id, &result).await {
            tracing::error!(%error, site_id, "could not store an uptime check");
        }
    }
}

/// Run the checker until `cancel` fires.
pub fn spawn(
    db: SqlitePool,
    targets: Targets,
    options: UptimeOptions,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(options.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => check_all(&db, &targets(), options.resolve_to).await,
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Up,
    /// Reachable but slow, the certificate is close to expiry, or the latest check failed.
    Degraded,
    /// Two failed checks in a row.
    Down,
    /// Not checked yet.
    Unknown,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Up => "up",
            Status::Degraded => "degraded",
            Status::Down => "down",
            Status::Unknown => "unknown",
        }
    }
}

/// A site's current health, for the status grid.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    pub status: Status,
    /// Percentage of successful checks over the last 30 days, if any ran.
    pub uptime_30d: Option<f64>,
    /// Page views for each of the last 7 days, oldest first.
    pub views_7d: Vec<i64>,
}

/// Decide a status from the two most recent checks, newest first.
pub fn status_from(recent: &[(bool, u64, Option<i64>)], slow_ms: u64) -> Status {
    match recent {
        [] => Status::Unknown,
        [(false, ..), (false, ..), ..] => Status::Down,
        [(false, ..), ..] => Status::Degraded,
        [(true, latency, cert_days), ..] => {
            if *latency > slow_ms || cert_days.is_some_and(|days| days < CERT_WARN_DAYS) {
                Status::Degraded
            } else {
                Status::Up
            }
        }
    }
}

/// Current health of every site that has data.
pub async fn health(db: &SqlitePool, slow_ms: u64) -> Result<HashMap<i64, Health>, sqlx::Error> {
    let now = now_ms();
    let mut out: HashMap<i64, Health> = HashMap::new();
    let blank = || Health {
        status: Status::Unknown,
        uptime_30d: None,
        views_7d: vec![0; 7],
    };

    // The two newest checks per site.
    let recent = sqlx::query!(
        r#"SELECT site_id, ok, latency_ms, cert_days FROM (
               SELECT site_id, ok, latency_ms, cert_days, row_number() OVER (PARTITION BY site_id ORDER BY ts DESC, id DESC) AS n
               FROM checks WHERE ts >= ?1)
           WHERE n <= 2 ORDER BY site_id, n"#,
        // Old checks say nothing about now.
        now - MS_PER_DAY
    )
    .fetch_all(db)
    .await?;
    let mut by_site: HashMap<i64, Vec<(bool, u64, Option<i64>)>> = HashMap::new();
    for row in recent {
        by_site.entry(row.site_id).or_default().push((
            row.ok != 0,
            row.latency_ms as u64,
            row.cert_days,
        ));
    }
    for (site_id, checks) in by_site {
        out.entry(site_id).or_insert_with(blank).status = status_from(&checks, slow_ms);
    }

    let since = now - 30 * MS_PER_DAY;
    let uptime = sqlx::query!(r#"SELECT site_id, 100.0 * avg(ok) AS "pct?: f64" FROM checks WHERE ts >= ?1 GROUP BY site_id"#, since)
        .fetch_all(db)
        .await?;
    for row in uptime {
        out.entry(row.site_id).or_insert_with(blank).uptime_30d = row.pct;
    }

    let today = now.div_euclid(MS_PER_DAY);
    let first_day = (today - 6) * MS_PER_DAY;
    let views = sqlx::query!(
        r#"SELECT site_id, day, views FROM daily_rollups WHERE day >= date(?1 / 1000, 'unixepoch')"#,
        first_day
    )
    .fetch_all(db)
    .await?;
    let day_names: Vec<String> = sqlx::query_scalar(
        "WITH RECURSIVE d(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM d WHERE i < 6) SELECT date(? / 1000 + i * 86400, 'unixepoch') FROM d ORDER BY i",
    )
    .bind(first_day)
    .fetch_all(db)
    .await?;
    for row in views {
        if let Some(index) = day_names.iter().position(|d| *d == row.day) {
            out.entry(row.site_id).or_insert_with(blank).views_7d[index] = row.views;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_the_two_latest_checks() {
        let ok = (true, 100, None);
        let failed = (false, 100, None);
        assert_eq!(status_from(&[], 2000), Status::Unknown);
        assert_eq!(status_from(&[ok], 2000), Status::Up);
        assert_eq!(status_from(&[ok, failed], 2000), Status::Up, "recovered");
        assert_eq!(
            status_from(&[failed, ok], 2000),
            Status::Degraded,
            "one failure is not down yet"
        );
        assert_eq!(status_from(&[failed], 2000), Status::Degraded);
        assert_eq!(status_from(&[failed, failed], 2000), Status::Down);
        assert_eq!(
            status_from(&[(true, 2500, None), ok], 2000),
            Status::Degraded,
            "slow"
        );
        assert_eq!(
            status_from(&[(true, 100, Some(10)), ok], 2000),
            Status::Degraded,
            "certificate expiring"
        );
        assert_eq!(status_from(&[(true, 100, Some(60)), ok], 2000), Status::Up);
    }

    #[tokio::test]
    async fn unreachable_sites_fail_quickly_with_a_reason() {
        let target = Target {
            site_id: 1,
            hostname: "nothing.invalid".into(),
            url: "http://nothing.invalid/".into(),
        };
        // Port 9 on localhost: nothing listens there.
        let result = check(&target, Some("127.0.0.1:9".parse().unwrap())).await;
        assert!(!result.ok);
        assert_eq!(result.status, None);
        assert!(!result.detail.is_empty());
    }

    #[tokio::test]
    async fn health_combines_checks_and_rollups() {
        let db = sw_core::db::open_in_memory().await.unwrap();
        let now = now_ms();
        for (age, site, ok) in [(3, 1, 1), (2, 1, 0), (1, 1, 0), (2, 2, 0), (1, 2, 1)] {
            sqlx::query("INSERT INTO checks (ts, site_id, ok, latency_ms) VALUES (?, ?, ?, 50)")
                .bind(now - age * 1000)
                .bind(site)
                .bind(ok)
                .execute(&db)
                .await
                .unwrap();
        }
        let today: String = sqlx::query_scalar("SELECT date(? / 1000, 'unixepoch')")
            .bind(now)
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO daily_rollups (site_id, day, views, visitors, errors, p95_ms) VALUES (1, ?, 12, 3, 0, 5)").bind(&today).execute(&db).await.unwrap();

        let health = health(&db, 2000).await.unwrap();
        assert_eq!(health[&1].status, Status::Down);
        assert!((health[&1].uptime_30d.unwrap() - 33.33).abs() < 0.1);
        assert_eq!(health[&1].views_7d, [0, 0, 0, 0, 0, 0, 12]);
        assert_eq!(health[&2].status, Status::Up);
        assert_eq!(health[&2].uptime_30d, Some(50.0));
    }
}
