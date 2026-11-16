//! Visitor accounts for a site: sign-up, sign-in and sessions.
//!
//! Password handling is easy to get wrong, so backends do not write it. They call
//! `auth.signUp`, `auth.signIn` and `auth.user`; the daemon does the work here, on two
//! tables it owns inside the site's database (`sw_users`, `sw_sessions`):
//!
//! - Passwords are hashed with Argon2id. Site code can never read a hash.
//! - A session is a random 256-bit token. The database stores only its SHA-256, so a
//!   copy of the database does not contain usable sessions.
//! - Failed sign-ins are limited per account, and a sign-in for an unknown email takes
//!   as long as one for a known email.
//!
//! Errors meant for the visitor are returned as `{ "error": { status, message } }`, not
//! as failures, so that the JavaScript side can turn them into the right HTTP status.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::sitedb::SiteDb;

/// How long a session lasts.
pub const SESSION_DAYS: i64 = 30;
const MIN_PASSWORD_CHARS: usize = 8;
const MAX_PASSWORD_BYTES: usize = 200;
/// Failed sign-ins allowed per account in [`FAILURE_WINDOW`].
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Most accounts tracked for failed sign-ins; beyond it the oldest half is dropped.
const MAX_TRACKED: usize = 10_000;

/// Sign-in attempt tracking and the cookie policy. One per daemon.
#[derive(Debug)]
pub struct Auth {
    /// Whether sites are served over HTTPS (then cookies are `Secure` and `__Host-`).
    secure: bool,
    failures: Mutex<HashMap<(i64, String), (u32, Instant)>>,
}

fn visitor_error(status: u16, message: &str) -> Value {
    json!({ "error": { "status": status, "message": message } })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn normalize_email(email: &str) -> Option<String> {
    let email = email.trim().to_ascii_lowercase();
    let (local, domain) = email.split_once('@')?;
    let ok = email.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.matches('@').count() == 1;
    ok.then_some(email)
}

fn hash_password(password: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|_| "no randomness available".to_owned())?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| e.to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Hashing is deliberately slow: keep it off the async executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("background task failed: {e}"))
}

fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("auth: {key} must be a string"))
}

impl Auth {
    pub fn new(secure: bool) -> Self {
        Self {
            secure,
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn cookie_name(&self) -> &'static str {
        if self.secure {
            "__Host-sw_session"
        } else {
            "sw_session"
        }
    }

    fn cookie(&self, token: &str, max_age_secs: i64) -> String {
        format!(
            "{}={token}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{}",
            self.cookie_name(),
            if self.secure { "; Secure" } else { "" }
        )
    }

    fn blocked(&self, site_id: i64, email: &str) -> bool {
        let failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        failures
            .get(&(site_id, email.to_owned()))
            .is_some_and(|(count, since)| {
                *count >= MAX_FAILURES && since.elapsed() < FAILURE_WINDOW
            })
    }

    fn record_failure(&self, site_id: i64, email: &str) {
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        if failures.len() >= MAX_TRACKED {
            failures.retain(|_, (_, since)| since.elapsed() < FAILURE_WINDOW);
            if failures.len() >= MAX_TRACKED {
                failures.clear();
            }
        }
        let entry = failures
            .entry((site_id, email.to_owned()))
            .or_insert((0, Instant::now()));
        if entry.1.elapsed() >= FAILURE_WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    fn clear_failures(&self, site_id: i64, email: &str) {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(site_id, email.to_owned()));
    }

    async fn start_session(&self, db: &SiteDb, user: Value) -> Result<Value, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| "no randomness available".to_owned())?;
        let token = hex(&bytes);
        let now = sw_core::time::now_ms();
        let expires = now + SESSION_DAYS * 24 * 60 * 60 * 1000;
        db.internal_query(
            "DELETE FROM sw_sessions WHERE expires_at <= ?",
            &[json!(now)],
        )
        .await?;
        db.internal_query(
            "INSERT INTO sw_sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
            &[json!(token_hash(&token)), user["id"].clone(), json!(now), json!(expires)],
        )
        .await?;
        Ok(json!({
            "user": user,
            "cookie": self.cookie(&token, SESSION_DAYS * 24 * 60 * 60),
        }))
    }

    /// Serve one `auth.*` host call for a site.
    pub async fn call(
        &self,
        site_id: i64,
        db: &SiteDb,
        op: &str,
        args: &Value,
    ) -> Result<Value, String> {
        match op {
            "auth.signup" => {
                let Some(email) = normalize_email(text(args, "email")?) else {
                    return Ok(visitor_error(400, "Please enter a valid email address."));
                };
                let password = text(args, "password")?.to_owned();
                if password.chars().count() < MIN_PASSWORD_CHARS
                    || password.len() > MAX_PASSWORD_BYTES
                {
                    return Ok(visitor_error(
                        400,
                        "Please choose a password of at least 8 characters.",
                    ));
                }
                let hash = blocking(move || hash_password(&password)).await??;
                let created = db
                    .internal_query(
                        "INSERT INTO sw_users (email, password_hash, created_at) VALUES (?, ?, ?) RETURNING id, email, created_at",
                        &[json!(email), json!(hash), json!(sw_core::time::now_ms())],
                    )
                    .await;
                match created {
                    Ok(mut rows) if !rows.is_empty() => {
                        self.start_session(db, rows.remove(0)).await
                    }
                    Ok(_) => Err("the account could not be created".into()),
                    Err(e) if e == "unique" => Ok(visitor_error(
                        409,
                        "There is already an account with this email address.",
                    )),
                    Err(e) => Err(e),
                }
            }
            "auth.login" => {
                let wrong = || visitor_error(401, "Wrong email or password.");
                let Some(email) = normalize_email(text(args, "email")?) else {
                    return Ok(wrong());
                };
                let password = text(args, "password")?.to_owned();
                if self.blocked(site_id, &email) {
                    return Ok(visitor_error(
                        429,
                        "Too many attempts. Please wait a few minutes and try again.",
                    ));
                }
                let mut rows = db
                    .internal_query(
                        "SELECT id, email, created_at, password_hash FROM sw_users WHERE email = ?",
                        &[json!(email)],
                    )
                    .await?;
                let user = rows.pop();
                // Verify against something even when there is no such account, so the
                // answer takes the same time either way.
                let stored = user
                    .as_ref()
                    .and_then(|u| u["password_hash"].as_str())
                    .map(str::to_owned);
                let matches = blocking(move || match stored {
                    Some(hash) => verify_password(&password, &hash),
                    None => {
                        let _ = hash_password(&password);
                        false
                    }
                })
                .await?;
                let Some(mut user) = user.filter(|_| matches) else {
                    self.record_failure(site_id, &email);
                    return Ok(wrong());
                };
                self.clear_failures(site_id, &email);
                if let Some(fields) = user.as_object_mut() {
                    fields.remove("password_hash");
                }
                self.start_session(db, user).await
            }
            "auth.session" => {
                let Some(token) = args
                    .get("token")
                    .and_then(Value::as_str)
                    .filter(|t| t.len() == 64)
                else {
                    return Ok(Value::Null);
                };
                let mut rows = db
                    .internal_query(
                        "SELECT u.id, u.email, u.created_at FROM sw_sessions s JOIN sw_users u ON u.id = s.user_id
                         WHERE s.token_hash = ? AND s.expires_at > ?",
                        &[json!(token_hash(token)), json!(sw_core::time::now_ms())],
                    )
                    .await?;
                Ok(rows.pop().unwrap_or(Value::Null))
            }
            "auth.logout" => {
                if let Some(token) = args.get("token").and_then(Value::as_str) {
                    db.internal_query(
                        "DELETE FROM sw_sessions WHERE token_hash = ?",
                        &[json!(token_hash(token))],
                    )
                    .await?;
                }
                Ok(json!({ "cookie": self.cookie("", 0) }))
            }
            "auth.cookie_name" => Ok(json!(self.cookie_name())),
            "auth.delete_user" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or("auth: id must be a number")?;
                db.internal_query("DELETE FROM sw_sessions WHERE user_id = ?", &[json!(id)])
                    .await?;
                db.internal_query("DELETE FROM sw_users WHERE id = ?", &[json!(id)])
                    .await?;
                Ok(Value::Null)
            }
            "auth.set_password" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or("auth: id must be a number")?;
                let password = text(args, "password")?.to_owned();
                if password.chars().count() < MIN_PASSWORD_CHARS
                    || password.len() > MAX_PASSWORD_BYTES
                {
                    return Ok(visitor_error(
                        400,
                        "Please choose a password of at least 8 characters.",
                    ));
                }
                let hash = blocking(move || hash_password(&password)).await??;
                db.internal_query(
                    "UPDATE sw_users SET password_hash = ? WHERE id = ?",
                    &[json!(hash), json!(id)],
                )
                .await?;
                // Changing the password signs the account out everywhere.
                db.internal_query("DELETE FROM sw_sessions WHERE user_id = ?", &[json!(id)])
                    .await?;
                Ok(Value::Null)
            }
            other => Err(format!("unknown operation {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::sitedb::DbLimits;

    async fn db() -> (tempfile::TempDir, SiteDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = SiteDb::open(&dir.path().join("site.db"), DbLimits::default())
            .await
            .unwrap();
        (dir, db)
    }

    fn token(result: &Value) -> String {
        let cookie = result["cookie"].as_str().unwrap();
        cookie.split(['=', ';']).nth(1).unwrap().to_owned()
    }

    #[tokio::test]
    async fn sign_up_sign_in_sessions_and_sign_out() {
        let (_dir, db) = db().await;
        let auth = Auth::new(true);
        let creds = json!({"email": " Ada@Example.com ", "password": "correct horse"});

        let made = auth.call(1, &db, "auth.signup", &creds).await.unwrap();
        assert_eq!(made["user"]["email"], "ada@example.com");
        assert_eq!(made["user"]["id"], 1);
        assert!(made["user"].get("password_hash").is_none());
        let cookie = made["cookie"].as_str().unwrap();
        assert!(cookie.starts_with("__Host-sw_session="));
        assert!(cookie.ends_with("; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax; Secure"));

        let who = auth
            .call(1, &db, "auth.session", &json!({"token": token(&made)}))
            .await
            .unwrap();
        assert_eq!(who["email"], "ada@example.com");
        for bad in [
            json!({"token": "0".repeat(64)}),
            json!({"token": "short"}),
            json!({}),
        ] {
            assert_eq!(
                auth.call(1, &db, "auth.session", &bad).await.unwrap(),
                Value::Null
            );
        }

        // Duplicate, invalid and weak sign-ups are visitor errors, not failures.
        let dup = auth
            .call(
                1,
                &db,
                "auth.signup",
                &json!({"email": "ADA@example.com", "password": "another one"}),
            )
            .await
            .unwrap();
        assert_eq!(dup["error"]["status"], 409);
        let weak = auth
            .call(
                1,
                &db,
                "auth.signup",
                &json!({"email": "b@example.com", "password": "short"}),
            )
            .await
            .unwrap();
        assert_eq!(weak["error"]["status"], 400);
        let bad = auth
            .call(
                1,
                &db,
                "auth.signup",
                &json!({"email": "not-an-email", "password": "long enough"}),
            )
            .await
            .unwrap();
        assert_eq!(bad["error"]["status"], 400);

        let again = auth.call(1, &db, "auth.login", &creds).await.unwrap();
        assert_ne!(
            token(&again),
            token(&made),
            "every sign-in gets its own session"
        );
        assert!(again["user"].get("password_hash").is_none());

        let out = auth
            .call(1, &db, "auth.logout", &json!({"token": token(&again)}))
            .await
            .unwrap();
        assert!(out["cookie"].as_str().unwrap().contains("Max-Age=0"));
        assert_eq!(
            auth.call(1, &db, "auth.session", &json!({"token": token(&again)}))
                .await
                .unwrap(),
            Value::Null
        );
        // The first session is still valid until the password changes.
        auth.call(
            1,
            &db,
            "auth.set_password",
            &json!({"id": 1, "password": "a new password"}),
        )
        .await
        .unwrap();
        assert_eq!(
            auth.call(1, &db, "auth.session", &json!({"token": token(&made)}))
                .await
                .unwrap(),
            Value::Null
        );
        let old = auth.call(1, &db, "auth.login", &creds).await.unwrap();
        assert_eq!(old["error"]["status"], 401);
        let new = auth
            .call(
                1,
                &db,
                "auth.login",
                &json!({"email": "ada@example.com", "password": "a new password"}),
            )
            .await
            .unwrap();
        assert_eq!(new["user"]["id"], 1);

        auth.call(1, &db, "auth.delete_user", &json!({"id": 1}))
            .await
            .unwrap();
        assert_eq!(
            auth.call(1, &db, "auth.session", &json!({"token": token(&new)}))
                .await
                .unwrap(),
            Value::Null
        );
    }

    #[tokio::test]
    async fn failed_sign_ins_are_limited_per_account_and_site() {
        let (_dir, db) = db().await;
        let auth = Auth::new(false);
        auth.call(
            1,
            &db,
            "auth.signup",
            &json!({"email": "a@example.com", "password": "the right one"}),
        )
        .await
        .unwrap();
        let wrong = json!({"email": "a@example.com", "password": "the wrong one"});
        for _ in 0..MAX_FAILURES {
            assert_eq!(
                auth.call(1, &db, "auth.login", &wrong).await.unwrap()["error"]["status"],
                401
            );
        }
        // Now even the right password has to wait.
        let right = json!({"email": "a@example.com", "password": "the right one"});
        assert_eq!(
            auth.call(1, &db, "auth.login", &right).await.unwrap()["error"]["status"],
            429
        );
        // Unknown accounts answer the same way as wrong passwords, and other sites and
        // accounts are not affected.
        let unknown = json!({"email": "nobody@example.com", "password": "whatever it is"});
        assert_eq!(
            auth.call(1, &db, "auth.login", &unknown).await.unwrap()["error"]["message"],
            "Wrong email or password."
        );
        assert!(auth.call(2, &db, "auth.login", &right).await.unwrap()["cookie"].is_string());
        // Without HTTPS the cookie cannot be Secure.
        let cookie = auth.call(2, &db, "auth.login", &right).await.unwrap()["cookie"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(cookie.starts_with("sw_session=") && !cookie.contains("Secure"));
    }

    #[tokio::test]
    async fn site_sql_can_join_users_but_never_see_hashes_or_sessions() {
        let (_dir, db) = db().await;
        let auth = Auth::new(true);
        auth.call(
            1,
            &db,
            "auth.signup",
            &json!({"email": "a@example.com", "password": "long enough"}),
        )
        .await
        .unwrap();
        db.migrate(&[crate::sitedb::Migration {
            name: "0001_notes.sql".into(),
            sql: "CREATE TABLE notes (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES sw_users (id), body TEXT NOT NULL);".into(),
        }])
        .await
        .unwrap();
        db.run("INSERT INTO notes (user_id, body) VALUES (1, 'hi')", &[])
            .await
            .unwrap();
        let rows = db
            .query(
                "SELECT n.body, u.email FROM notes n JOIN sw_users u ON u.id = n.user_id",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows, [json!({"body": "hi", "email": "a@example.com"})]);
        // A note for a user that does not exist is refused by the foreign key.
        assert!(
            db.run("INSERT INTO notes (user_id, body) VALUES (99, 'x')", &[])
                .await
                .is_err()
        );

        for sql in [
            "SELECT * FROM sw_users",
            "SELECT password_hash FROM sw_users",
            "SELECT email FROM sw_users WHERE password_hash LIKE '$argon2%'",
            "SELECT * FROM sw_sessions",
            "SELECT count(*) FROM sw_sessions",
            "UPDATE sw_users SET email = 'x@example.com'",
            "UPDATE sw_users SET password_hash = 'x'",
            "INSERT INTO sw_users (email, password_hash, created_at) VALUES ('e@example.com', 'x', 0)",
            "DELETE FROM sw_users",
            "DELETE FROM sw_sessions",
            "INSERT INTO sw_sessions (token_hash, user_id, created_at, expires_at) VALUES ('x', 1, 0, 9999999999999)",
        ] {
            assert!(db.query(sql, &[]).await.is_err(), "allowed: {sql}");
        }
        for sql in [
            "DROP TABLE sw_users",
            "ALTER TABLE sw_users ADD COLUMN admin INTEGER",
            "DROP TABLE sw_sessions",
            "CREATE TRIGGER t AFTER INSERT ON notes BEGIN DELETE FROM sw_sessions; END",
            "CREATE TRIGGER leak AFTER DELETE ON notes BEGIN INSERT INTO notes (user_id, body) SELECT id, password_hash FROM sw_users; END",
            "CREATE TRIGGER u AFTER INSERT ON sw_users BEGIN SELECT 1; END",
        ] {
            let bad = crate::sitedb::Migration {
                name: "0002_bad.sql".into(),
                sql: sql.into(),
            };
            assert!(
                db.migrate(&[bad]).await.is_err(),
                "allowed in a migration: {sql}"
            );
        }
        // The managed tables do not show up as the site's own schema.
        let names: Vec<String> = db
            .schema()
            .await
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["notes"]);
    }
}
