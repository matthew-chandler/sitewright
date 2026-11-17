//! Background work for site backends: jobs queued with `jobs.enqueue` and tasks declared
//! with `jobs.schedule`.
//!
//! Both run in the site's background worker and make the same host calls as a request
//! handler, so they have exactly the access the site's backend has: its database, its
//! secrets, its approved hosts and, when the owner allowed it, AI models under the same
//! per-site limits. See ADR-0011.
//!
//! This module holds the parts that do not need a worker: cron expressions, the checks
//! on what a backend declares, and the queue in Sitewright's database.

use serde::Serialize;
use serde_json::Value;

use crate::protocol::Declared;

/// Most scheduled tasks one backend may declare.
pub const MAX_SCHEDULES: usize = 20;
/// Most job handlers one backend may declare.
pub const MAX_JOB_KINDS: usize = 50;
/// Largest job payload, as JSON.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// Most jobs waiting per site.
pub const MAX_QUEUED: i64 = 1000;
/// Longest a job may be delayed.
pub const MAX_DELAY_SECS: i64 = 30 * 24 * 3600;
/// Most attempts a job may ask for.
pub const MAX_ATTEMPTS: i64 = 10;
/// Finished jobs and runs are kept this long.
pub const KEEP_MS: i64 = 7 * sw_core::time::MS_PER_DAY;
/// Most jobs one site runs per scheduler pass, so that one busy site cannot hold the
/// rest back for long.
pub const BATCH: i64 = 20;

/// How long a failed job waits before its next attempt: 30 s, 2 min, 8 min, ...
pub fn retry_delay_ms(attempts: i64) -> i64 {
    let step = attempts.clamp(1, 6) - 1;
    30_000 * 4i64.pow(u32::try_from(step).unwrap_or(0))
}

// ---------------------------------------------------------------- cron

/// A five-field cron expression (minute, hour, day of month, month, day of week),
/// evaluated in UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// Whether day of month and day of week were each given (not `*`). When both are,
    /// a day matches if either does, as in classic cron.
    days_given: bool,
    weekdays_given: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn field(text: &str, min: u32, max: u32, names: &[&str]) -> Result<(u64, bool), String> {
    let value = |part: &str| -> Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(part)) {
            return Ok(min + u32::try_from(i).unwrap_or(0));
        }
        part.parse::<u32>()
            .ok()
            .filter(|v| (min..=max).contains(v))
            .ok_or_else(|| format!("{part:?} is not between {min} and {max}"))
    };
    let mut bits = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step = step
                    .parse::<u32>()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| format!("{step:?} is not a step"))?;
                (range, step)
            }
            None => (item, 1),
        };
        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let start = value(range)?;
            // `5/15` means from 5 to the end, every 15.
            (start, if item.contains('/') { max } else { start })
        };
        if from > to {
            return Err(format!("{range:?} runs backwards"));
        }
        let mut v = from;
        while v <= to {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, text != "*"))
}

impl Cron {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let expanded = match text.to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *".to_owned(),
            "@daily" | "@midnight" => "0 0 * * *".to_owned(),
            "@weekly" => "0 0 * * 0".to_owned(),
            "@monthly" => "0 0 1 * *".to_owned(),
            "@yearly" | "@annually" => "0 0 1 1 *".to_owned(),
            _ => text.to_owned(),
        };
        let parts: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = parts.as_slice() else {
            return Err(format!(
                "{text:?} is not a cron expression: it needs five fields (minute hour day month weekday), such as \"0 6 * * *\""
            ));
        };
        let named = |what: &str, r: Result<(u64, bool), String>| {
            r.map_err(|e| format!("{text:?}: the {what} field: {e}"))
        };
        let (minutes, _) = named("minute", field(minute, 0, 59, &[]))?;
        let (hours, _) = named("hour", field(hour, 0, 23, &[]))?;
        let (days, days_given) = named("day", field(day, 1, 31, &[]))?;
        let (months, _) = named("month", field(month, 1, 12, &MONTHS))?;
        let (mut weekdays, weekdays_given) = named("weekday", field(weekday, 0, 7, &WEEKDAYS))?;
        // 7 is Sunday too.
        if weekdays & (1 << 7) != 0 {
            weekdays |= 1;
        }
        let cron = Self {
            minutes,
            hours: u32::try_from(hours).unwrap_or(0),
            days: u32::try_from(days).unwrap_or(0),
            months: u16::try_from(months).unwrap_or(0),
            weekdays: u8::try_from(weekdays & 0x7f).unwrap_or(0),
            days_given,
            weekdays_given,
        };
        if cron.next_after(0).is_none() {
            return Err(format!("{text:?} never runs"));
        }
        Ok(cron)
    }

    fn day_matches(&self, day: u32, weekday: u32) -> bool {
        let by_day = self.days & (1 << day) != 0;
        let by_weekday = self.weekdays & (1 << weekday) != 0;
        if self.days_given && self.weekdays_given {
            by_day || by_weekday
        } else {
            by_day && by_weekday
        }
    }

    /// The first minute strictly after `after_ms` (Unix milliseconds) at which this
    /// runs, or `None` if it does not within eight years.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        const MINUTE: i64 = 60_000;
        const HOUR: i64 = 60 * MINUTE;
        let limit = after_ms + 8 * 366 * sw_core::time::MS_PER_DAY;
        let mut t = after_ms.div_euclid(MINUTE) * MINUTE + MINUTE;
        while t <= limit {
            let days = t.div_euclid(sw_core::time::MS_PER_DAY);
            let (year, month, day) = civil_from_days(days);
            if self.months & (1 << month) == 0 {
                let (y, m) = if month == 12 {
                    (year + 1, 1)
                } else {
                    (year, month + 1)
                };
                t = days_from_civil(y, m, 1) * sw_core::time::MS_PER_DAY;
                continue;
            }
            let weekday = u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0);
            if !self.day_matches(day, weekday) {
                t = (days + 1) * sw_core::time::MS_PER_DAY;
                continue;
            }
            let in_day = t.rem_euclid(sw_core::time::MS_PER_DAY);
            let hour = u32::try_from(in_day / HOUR).unwrap_or(0);
            if self.hours & (1 << hour) == 0 {
                t = (t.div_euclid(HOUR) + 1) * HOUR;
                continue;
            }
            let minute = (in_day % HOUR) / MINUTE;
            if self.minutes & (1 << minute) == 0 {
                t += MINUTE;
                continue;
            }
            return Some(t);
        }
        None
    }
}

/// Year, month (1-12), day (1-31) of a day counted from 1970-01-01 (H. Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let (m, d) = (i64::from(m), i64::from(d));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Check what a backend declared: limits, and that every cron expression is valid.
pub fn validate(declared: &Declared) -> Result<(), String> {
    if declared.schedules.len() > MAX_SCHEDULES {
        return Err(format!(
            "the backend declares {} schedules; at most {MAX_SCHEDULES} are allowed",
            declared.schedules.len()
        ));
    }
    if declared.jobs.len() > MAX_JOB_KINDS {
        return Err(format!(
            "the backend declares {} jobs; at most {MAX_JOB_KINDS} are allowed",
            declared.jobs.len()
        ));
    }
    for schedule in &declared.schedules {
        Cron::parse(&schedule.cron).map_err(|e| format!("schedule {}: {e}", schedule.name))?;
    }
    Ok(())
}

// ---------------------------------------------------------------- the queue

/// A job or a run of a scheduled task, as shown to the owner and the agent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobRecord {
    pub id: i64,
    /// `job` or `schedule`.
    pub kind: String,
    pub name: String,
    /// `queued`, `running`, `done` or `failed`.
    pub status: String,
    pub attempts: i64,
    pub max_attempts: i64,
    pub run_at: i64,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

/// A scheduled task of the live backend, as shown to the owner and the agent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScheduleInfo {
    pub name: String,
    pub cron: String,
    /// Unix milliseconds of the next run.
    pub next_at: Option<i64>,
}

/// A due job, claimed for running.
#[derive(Debug, Clone)]
pub(crate) struct DueJob {
    pub id: i64,
    pub name: String,
    pub payload: Value,
    pub attempts: i64,
    pub max_attempts: i64,
}

/// What `jobs.enqueue` asked for, checked.
#[derive(Debug, Clone)]
pub(crate) struct NewJob {
    pub name: String,
    pub payload: String,
    pub delay_ms: i64,
    pub attempts: i64,
}

impl NewJob {
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or("jobs.enqueue takes a job name")?
            .to_owned();
        let payload = args
            .get("payload")
            .cloned()
            .unwrap_or(Value::Null)
            .to_string();
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(format!(
                "a job's payload may be at most {} KB of JSON; store larger data in the database and pass its id",
                MAX_PAYLOAD_BYTES / 1024
            ));
        }
        let delay = args
            .get("delaySeconds")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if !(0.0..=MAX_DELAY_SECS as f64).contains(&delay) {
            return Err(format!(
                "delaySeconds must be between 0 and {MAX_DELAY_SECS} (30 days)"
            ));
        }
        let attempts = args.get("attempts").and_then(Value::as_i64).unwrap_or(3);
        if !(1..=MAX_ATTEMPTS).contains(&attempts) {
            return Err(format!("attempts must be between 1 and {MAX_ATTEMPTS}"));
        }
        Ok(Self {
            name,
            payload,
            delay_ms: (delay * 1000.0) as i64,
            attempts,
        })
    }
}

/// The queue, in Sitewright's own database. Every function takes the site id and
/// touches only that site's rows.
#[derive(Debug, Clone)]
pub(crate) struct JobStore {
    pool: sqlx::SqlitePool,
}

fn db_error(e: sqlx::Error) -> String {
    e.to_string()
}

impl JobStore {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn enqueue(&self, site_id: i64, job: &NewJob) -> Result<i64, String> {
        let waiting = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!: i64" FROM site_jobs
               WHERE site_id = ?1 AND kind = 'job' AND status IN ('queued', 'running')"#,
            site_id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)?;
        if waiting >= MAX_QUEUED {
            return Err(format!(
                "this site already has {MAX_QUEUED} jobs waiting; try again later"
            ));
        }
        let now = sw_core::time::now_ms();
        let run_at = now + job.delay_ms;
        let id = sqlx::query_scalar!(
            r#"INSERT INTO site_jobs (site_id, kind, name, payload, status, run_at, max_attempts, created_at)
               VALUES (?1, 'job', ?2, ?3, 'queued', ?4, ?5, ?6) RETURNING id as "id!""#,
            site_id,
            job.name,
            job.payload,
            run_at,
            job.attempts,
            now
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(id)
    }

    /// Sites with jobs that are due now.
    pub async fn sites_with_due_jobs(&self, now: i64) -> Result<Vec<i64>, String> {
        sqlx::query_scalar!(
            r#"SELECT DISTINCT site_id as "site_id!" FROM site_jobs WHERE status = 'queued' AND run_at <= ?1"#,
            now
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)
    }

    /// Claim up to `limit` due jobs of a site, oldest first, and mark them running.
    pub async fn claim_due(
        &self,
        site_id: i64,
        now: i64,
        limit: i64,
    ) -> Result<Vec<DueJob>, String> {
        let rows = sqlx::query!(
            r#"UPDATE site_jobs SET status = 'running', attempts = attempts + 1
               WHERE id IN (SELECT id FROM site_jobs
                            WHERE site_id = ?1 AND status = 'queued' AND run_at <= ?2
                            ORDER BY run_at, id LIMIT ?3)
               RETURNING id as "id!", name, payload, attempts, max_attempts"#,
            site_id,
            now,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        let mut jobs: Vec<DueJob> = rows
            .into_iter()
            .map(|r| DueJob {
                id: r.id,
                name: r.name,
                payload: serde_json::from_str(&r.payload).unwrap_or(Value::Null),
                attempts: r.attempts,
                max_attempts: r.max_attempts,
            })
            .collect();
        jobs.sort_by_key(|j| j.id);
        Ok(jobs)
    }

    /// Record how a job ended. A failure is retried later while attempts remain,
    /// unless `retry` is false.
    pub async fn finish(
        &self,
        job: &DueJob,
        error: Option<&str>,
        retry: bool,
    ) -> Result<(), String> {
        let now = sw_core::time::now_ms();
        match error {
            None => {
                sqlx::query!(
                    "UPDATE site_jobs SET status = 'done', finished_at = ?2, last_error = NULL WHERE id = ?1",
                    job.id,
                    now
                )
                .execute(&self.pool)
                .await
                .map_err(db_error)?;
            }
            Some(error) => {
                let error: String = error.chars().take(2000).collect();
                if retry && job.attempts < job.max_attempts {
                    let run_at = now + retry_delay_ms(job.attempts);
                    sqlx::query!(
                        "UPDATE site_jobs SET status = 'queued', run_at = ?2, last_error = ?3 WHERE id = ?1",
                        job.id,
                        run_at,
                        error
                    )
                    .execute(&self.pool)
                    .await
                    .map_err(db_error)?;
                } else {
                    sqlx::query!(
                        "UPDATE site_jobs SET status = 'failed', finished_at = ?2, last_error = ?3 WHERE id = ?1",
                        job.id,
                        now,
                        error
                    )
                    .execute(&self.pool)
                    .await
                    .map_err(db_error)?;
                }
            }
        }
        Ok(())
    }

    /// Record the start of a scheduled run; returns its id.
    pub async fn start_run(&self, site_id: i64, name: &str, at: i64) -> Result<i64, String> {
        let now = sw_core::time::now_ms();
        sqlx::query_scalar!(
            r#"INSERT INTO site_jobs (site_id, kind, name, status, run_at, attempts, max_attempts, created_at)
               VALUES (?1, 'schedule', ?2, 'running', ?3, 1, 1, ?4) RETURNING id as "id!""#,
            site_id,
            name,
            at,
            now
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)
    }

    pub async fn finish_run(&self, id: i64, error: Option<&str>) -> Result<(), String> {
        let now = sw_core::time::now_ms();
        let status = if error.is_some() { "failed" } else { "done" };
        let error: Option<String> = error.map(|e| e.chars().take(2000).collect());
        sqlx::query!(
            "UPDATE site_jobs SET status = ?2, finished_at = ?3, last_error = ?4 WHERE id = ?1",
            id,
            status,
            now,
            error
        )
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    /// The newest `limit` jobs and runs of a site, newest first.
    pub async fn recent(&self, site_id: i64, limit: i64) -> Result<Vec<JobRecord>, String> {
        let rows = sqlx::query!(
            r#"SELECT id as "id!", kind, name, status, attempts, max_attempts, run_at, created_at,
                      finished_at, last_error
               FROM site_jobs WHERE site_id = ?1 ORDER BY id DESC LIMIT ?2"#,
            site_id,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(rows
            .into_iter()
            .map(|r| JobRecord {
                id: r.id,
                kind: r.kind,
                name: r.name,
                status: r.status,
                attempts: r.attempts,
                max_attempts: r.max_attempts,
                run_at: r.run_at,
                created_at: r.created_at,
                finished_at: r.finished_at,
                error: r.last_error,
            })
            .collect())
    }

    /// How many jobs of a site are waiting or running.
    pub async fn pending(&self, site_id: i64) -> Result<i64, String> {
        sqlx::query_scalar!(
            r#"SELECT count(*) as "n!: i64" FROM site_jobs
               WHERE site_id = ?1 AND status IN ('queued', 'running')"#,
            site_id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)
    }

    /// At start-up: whatever was running when the daemon stopped did not finish. Jobs
    /// go back to the queue (the attempt counts); scheduled runs are marked failed.
    pub async fn recover(&self) -> Result<u64, String> {
        let now = sw_core::time::now_ms();
        let jobs = sqlx::query!(
            "UPDATE site_jobs SET status = CASE WHEN attempts < max_attempts THEN 'queued' ELSE 'failed' END,
                    finished_at = CASE WHEN attempts < max_attempts THEN NULL ELSE ?1 END,
                    last_error = 'the server stopped while this was running'
             WHERE status = 'running' AND kind = 'job'",
            now
        )
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected();
        let runs = sqlx::query!(
            "UPDATE site_jobs SET status = 'failed', finished_at = ?1,
                    last_error = 'the server stopped while this was running'
             WHERE status = 'running' AND kind = 'schedule'",
            now
        )
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected();
        Ok(jobs + runs)
    }

    /// Forget finished jobs and runs older than [`KEEP_MS`].
    pub async fn prune(&self, now: i64) -> Result<u64, String> {
        let cutoff = now - KEEP_MS;
        Ok(sqlx::query!(
            "DELETE FROM site_jobs WHERE status IN ('done', 'failed') AND finished_at < ?1",
            cutoff
        )
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unix milliseconds of a UTC date and time.
    fn at(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, mo, d) * sw_core::time::MS_PER_DAY + h * 3_600_000 + mi * 60_000
    }

    #[test]
    fn civil_dates_round_trip() {
        for days in [-1000, 0, 59, 365, 10_957, 20_000, 20_774, 30_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{days}");
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(2028, 2, 29)), (2028, 2, 29));
    }

    #[test]
    fn next_runs_follow_the_expression() {
        let now = at(2026, 11, 17, 10, 7) + 12_345; // Tuesday
        let next = |expr: &str| Cron::parse(expr).unwrap().next_after(now).unwrap();
        assert_eq!(next("* * * * *"), at(2026, 11, 17, 10, 8));
        assert_eq!(next("*/15 * * * *"), at(2026, 11, 17, 10, 15));
        assert_eq!(next("5/15 * * * *"), at(2026, 11, 17, 10, 20));
        assert_eq!(next("0 6 * * *"), at(2026, 11, 18, 6, 0));
        assert_eq!(next("@hourly"), at(2026, 11, 17, 11, 0));
        assert_eq!(next("@daily"), at(2026, 11, 18, 0, 0));
        assert_eq!(next("0 9 * * mon-fri"), at(2026, 11, 18, 9, 0));
        assert_eq!(next("0 9 * * 0"), at(2026, 11, 22, 9, 0));
        assert_eq!(next("0 9 * * 7"), at(2026, 11, 22, 9, 0));
        assert_eq!(next("30 2 1 * *"), at(2026, 12, 1, 2, 30));
        assert_eq!(next("0 0 1 jan *"), at(2027, 1, 1, 0, 0));
        assert_eq!(next("0 12 29 2 *"), at(2028, 2, 29, 12, 0));
        assert_eq!(next("0,30 8-9 * * *"), at(2026, 11, 18, 8, 0));
        // Both day fields given: either may match (the 20th, or any Monday).
        assert_eq!(next("0 0 20 * mon"), at(2026, 11, 20, 0, 0));
        // Exactly on a matching minute: the next one, never the same.
        let exact = at(2026, 11, 17, 10, 15);
        assert_eq!(
            Cron::parse("*/15 * * * *").unwrap().next_after(exact),
            Some(at(2026, 11, 17, 10, 30))
        );
    }

    #[test]
    fn bad_expressions_are_explained() {
        for (expr, says) in [
            ("* * * *", "five fields"),
            ("60 * * * *", "minute"),
            ("* 24 * * *", "hour"),
            ("* * 0 * *", "day"),
            ("* * * 13 *", "month"),
            ("* * * * 8", "weekday"),
            ("*/0 * * * *", "step"),
            ("10-5 * * * *", "backwards"),
            ("0 0 30 2 *", "never runs"),
            ("every day", "five fields"),
        ] {
            let error = Cron::parse(expr).unwrap_err();
            assert!(error.contains(says), "{expr}: {error}");
        }
    }

    #[test]
    fn declarations_are_checked() {
        use crate::protocol::DeclaredSchedule;
        let mut declared = Declared {
            jobs: vec!["send".into()],
            schedules: vec![DeclaredSchedule {
                name: "nightly".into(),
                cron: "0 3 * * *".into(),
            }],
        };
        validate(&declared).unwrap();
        declared.schedules[0].cron = "0 3 * *".into();
        assert!(
            validate(&declared)
                .unwrap_err()
                .contains("schedule nightly")
        );
        declared.schedules = (0..=MAX_SCHEDULES)
            .map(|i| DeclaredSchedule {
                name: format!("s{i}"),
                cron: "@hourly".into(),
            })
            .collect();
        assert!(validate(&declared).unwrap_err().contains("at most"));
    }

    #[test]
    fn enqueue_arguments_are_checked() {
        let ok = NewJob::from_args(&serde_json::json!({"name": "a", "payload": {"x": 1}, "delaySeconds": 1.5, "attempts": 2})).unwrap();
        assert_eq!(
            (ok.delay_ms, ok.attempts, ok.payload.as_str()),
            (1500, 2, "{\"x\":1}")
        );
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"name": "a", "delaySeconds": -1}),
            serde_json::json!({"name": "a", "delaySeconds": 31 * 24 * 3600}),
            serde_json::json!({"name": "a", "attempts": 0}),
            serde_json::json!({"name": "a", "attempts": 11}),
            serde_json::json!({"name": "a", "payload": "x".repeat(MAX_PAYLOAD_BYTES)}),
        ] {
            assert!(NewJob::from_args(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn retries_back_off() {
        assert_eq!(retry_delay_ms(1), 30_000);
        assert_eq!(retry_delay_ms(2), 120_000);
        assert_eq!(retry_delay_ms(3), 480_000);
        assert_eq!(retry_delay_ms(100), retry_delay_ms(6));
    }
}
