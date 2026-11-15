//! Visitor hashing and user-agent classification.

use std::net::IpAddr;
use std::sync::Mutex;

use rand::Rng;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use sw_core::time::{MS_PER_DAY, now_ms};

const SALT_KEY: &str = "visitor_salt";

/// Produces the per-visitor hash stored with requests and events.
///
/// The hash covers the client IP, user agent and site, salted with a random key that is
/// replaced every UTC day. The same visitor therefore gets the same hash within a day (so
/// unique visitors can be counted) and an unrelated one the next day, and the stored value
/// cannot be turned back into an IP address once the salt is gone.
#[derive(Debug)]
pub struct VisitorHasher {
    db: Option<SqlitePool>,
    current: Mutex<Option<Salt>>,
}

#[derive(Debug, Clone)]
struct Salt {
    day: i64,
    key: [u8; 32],
}

impl VisitorHasher {
    /// A hasher whose daily salt is persisted in the `settings` table, so a restart in
    /// the middle of a day does not split that day's visitors in two.
    pub async fn load(db: SqlitePool) -> Self {
        let stored: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
            .bind(SALT_KEY)
            .fetch_optional(&db)
            .await
            .unwrap_or(None);
        let current = stored.and_then(|(value,)| {
            let (day, key) = value.split_once(':')?;
            let key: [u8; 32] = hex::decode(key).ok()?.try_into().ok()?;
            Some(Salt {
                day: day.parse().ok()?,
                key,
            })
        });
        Self {
            db: Some(db),
            current: Mutex::new(current),
        }
    }

    /// A hasher with an in-memory salt only. For tests.
    pub fn ephemeral() -> Self {
        Self {
            db: None,
            current: Mutex::new(None),
        }
    }

    /// The hash for a visitor of `site_id` right now.
    pub fn hash(&self, ip: IpAddr, user_agent: &str, site_id: i64) -> String {
        self.hash_at(now_ms(), ip, user_agent, site_id)
    }

    /// The hash for a visitor of `site_id` at time `now` (Unix milliseconds).
    pub fn hash_at(&self, now: i64, ip: IpAddr, user_agent: &str, site_id: i64) -> String {
        let key = self.salt_for(now.div_euclid(MS_PER_DAY));
        let mut hasher = Sha256::new();
        hasher.update(key);
        hasher.update(ip.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(user_agent.as_bytes());
        hasher.update([0]);
        hasher.update(site_id.to_le_bytes());
        hex::encode(&hasher.finalize()[..12])
    }

    fn salt_for(&self, day: i64) -> [u8; 32] {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(salt) = current.as_ref()
            && salt.day == day
        {
            return salt.key;
        }
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        *current = Some(Salt { day, key });
        if let Some(db) = self.db.clone() {
            // Persisting replaces (and so destroys) the previous day's salt.
            let value = format!("{day}:{}", hex::encode(key));
            tokio::spawn(async move {
                let result = sqlx::query(
                    "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                )
                .bind(SALT_KEY)
                .bind(value)
                .bind(now_ms())
                .execute(&db)
                .await;
                if let Err(error) = result {
                    tracing::warn!(%error, "could not persist the daily visitor salt");
                }
            });
        }
        key
    }
}

/// A coarse classification of the client, from its `User-Agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UaClass {
    Browser,
    Bot,
    Other,
}

impl UaClass {
    pub fn as_str(self) -> &'static str {
        match self {
            UaClass::Browser => "browser",
            UaClass::Bot => "bot",
            UaClass::Other => "other",
        }
    }
}

/// Classify obvious bots and browsers by user agent. Anything else is `Other`.
pub fn classify_user_agent(user_agent: &str) -> UaClass {
    let ua = user_agent.to_ascii_lowercase();
    const BOT_MARKERS: &[&str] = &[
        "bot",
        "crawl",
        "spider",
        "slurp",
        "curl/",
        "wget/",
        "python-requests",
        "python-urllib",
        "go-http-client",
        "httpclient",
        "libwww",
        "scrapy",
        "headless",
        "monitor",
        "uptime",
        "pingdom",
        "facebookexternalhit",
        "preview",
        "feedfetcher",
        "okhttp",
        "java/",
        "axios",
        "node-fetch",
        "postman",
        "sitewright",
    ];
    if ua.is_empty() || BOT_MARKERS.iter().any(|m| ua.contains(m)) {
        UaClass::Bot
    } else if ua.starts_with("mozilla/") || ua.starts_with("opera/") {
        UaClass::Browser
    } else {
        UaClass::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_user_agents() {
        let chrome = "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Mobile Safari/537.36";
        assert_eq!(classify_user_agent(chrome), UaClass::Browser);
        assert_eq!(
            classify_user_agent("Mozilla/5.0 (compatible; Googlebot/2.1)"),
            UaClass::Bot
        );
        assert_eq!(classify_user_agent("curl/8.5.0"), UaClass::Bot);
        assert_eq!(classify_user_agent(""), UaClass::Bot);
        assert_eq!(classify_user_agent("SomeApp/1.0"), UaClass::Other);
    }

    #[test]
    fn hash_is_stable_within_a_day_and_changes_across_days() {
        let hasher = VisitorHasher::ephemeral();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let day0 = 10 * MS_PER_DAY + 5;
        let a = hasher.hash_at(day0, ip, "ua", 1);
        assert_eq!(a, hasher.hash_at(day0 + 1000, ip, "ua", 1));
        assert_ne!(a, hasher.hash_at(day0, ip, "ua", 2), "differs per site");
        assert_ne!(
            a,
            hasher.hash_at(day0, ip, "other", 1),
            "differs per user agent"
        );
        assert_ne!(
            a,
            hasher.hash_at(day0, "203.0.113.8".parse().unwrap(), "ua", 1)
        );
        assert_ne!(
            a,
            hasher.hash_at(day0 + MS_PER_DAY, ip, "ua", 1),
            "rotates daily"
        );
        assert!(!a.contains("203"), "no raw IP in the hash");
        assert_eq!(a.len(), 24);
    }

    #[tokio::test]
    async fn salt_survives_a_restart_within_the_day() {
        let db = sw_core::db::open_in_memory().await.unwrap();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let first = VisitorHasher::load(db.clone()).await;
        let a = first.hash(ip, "ua", 1);
        // Wait for the background persist.
        for _ in 0..50 {
            let n: (i64,) = sqlx::query_as("SELECT count(*) FROM settings")
                .fetch_one(&db)
                .await
                .unwrap();
            if n.0 == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let second = VisitorHasher::load(db).await;
        assert_eq!(a, second.hash(ip, "ua", 1));
    }
}
