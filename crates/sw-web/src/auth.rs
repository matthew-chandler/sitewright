//! Authentication for the admin UI: passkeys, sessions, CSRF tokens, enrollment invites
//! and API tokens.
//!
//! There is one admin user. Shell access to the server is the root of trust:
//! `sitewright admin invite` prints a one-time enrollment link, and running it again is
//! the recovery path. There are no passwords anywhere.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use sw_core::time::{MS_PER_DAY, now_ms};
use uuid::Uuid;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Url,
};
use webauthn_rs::{Webauthn, WebauthnBuilder};

/// Name of the session cookie. The `__Host-` prefix makes browsers refuse it unless it is
/// Secure, has no Domain and Path=/, so no site subdomain can ever read or set it.
pub const SESSION_COOKIE: &str = "__Host-sw_session";
/// How long a session lasts.
pub const SESSION_DAYS: i64 = 30;
/// How long an enrollment invite stays valid.
pub const INVITE_MINUTES: i64 = 15;
/// How long a passkey ceremony may take between its start and finish calls.
const CEREMONY_LIFETIME: Duration = Duration::from_secs(300);
/// Prefix of API tokens, so they are recognisable in configs and secret scanners.
pub const API_TOKEN_PREFIX: &str = "swt_";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The caller is not allowed to do this, or the proof it offered is not valid.
    #[error("{0}")]
    Denied(String),
    #[error("{0}")]
    Invalid(String),
    #[error("too many attempts; wait a few minutes and try again")]
    RateLimited,
    #[error("passkey error: {0}")]
    Webauthn(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// A signed-in browser session.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    /// Must accompany every state-changing request, in the `X-CSRF-Token` header.
    pub csrf_token: String,
}

/// A new session together with the cookie value to hand to the browser.
#[derive(Debug)]
pub struct NewSession {
    pub session: Session,
    /// The raw session token. Only its hash is stored.
    pub token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PasskeyInfo {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiTokenInfo {
    pub id: i64,
    pub name: String,
    pub prefix: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// Who started a registration ceremony.
#[derive(Debug, Clone)]
enum RegistrationGrant {
    /// A one-time invite; holds the invite's row id.
    Invite(i64),
    /// An already signed-in user adding another passkey.
    Session,
}

struct PendingRegistration {
    started: Instant,
    state: PasskeyRegistration,
    grant: RegistrationGrant,
    user_uuid: Uuid,
}

struct PendingLogin {
    started: Instant,
    state: PasskeyAuthentication,
}

/// The authentication service.
pub struct Auth {
    db: SqlitePool,
    webauthn: Webauthn,
    registrations: Mutex<HashMap<String, PendingRegistration>>,
    logins: Mutex<HashMap<String, PendingLogin>>,
    limiter: RateLimiter,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth").finish_non_exhaustive()
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The stored form of a token: its SHA-256, in hex.
fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Auth {
    /// `rp_id` is the admin hostname; `origin` is the admin UI's full origin.
    pub fn new(db: SqlitePool, rp_id: &str, origin: &str) -> Result<Self, AuthError> {
        let url = Url::parse(origin).map_err(|e| {
            AuthError::Invalid(format!("admin origin {origin:?} is not a URL: {e}"))
        })?;
        let webauthn = WebauthnBuilder::new(rp_id, &url)
            .and_then(|b| b.rp_name("Sitewright").build())
            .map_err(|e| {
                AuthError::Webauthn(format!("cannot set up passkeys for {origin}: {e}"))
            })?;
        Ok(Self {
            db,
            webauthn,
            registrations: Mutex::new(HashMap::new()),
            logins: Mutex::new(HashMap::new()),
            limiter: RateLimiter::new(20, Duration::from_secs(300)),
        })
    }

    /// Count an authentication attempt from `ip`; refuse when there were too many.
    pub fn check_rate(&self, ip: IpAddr) -> Result<(), AuthError> {
        if self.limiter.allow(ip) {
            Ok(())
        } else {
            Err(AuthError::RateLimited)
        }
    }

    // ------------------------------------------------------------------ invites

    /// Create a one-time enrollment token, valid for [`INVITE_MINUTES`].
    pub async fn create_invite(&self) -> Result<String, AuthError> {
        let token = random_token();
        let hash = hash_token(&token);
        let now = now_ms();
        let expires = now + INVITE_MINUTES * 60_000;
        sqlx::query!(
            "INSERT INTO invites (token_hash, created_at, expires_at) VALUES (?1, ?2, ?3)",
            hash,
            now,
            expires
        )
        .execute(&self.db)
        .await?;
        sw_core::db::audit(&self.db, "cli", "auth.invite.create", None, None).await;
        Ok(token)
    }

    /// The row id of a usable invite, if `token` is one.
    async fn valid_invite(&self, token: &str) -> Result<Option<i64>, AuthError> {
        let hash = hash_token(token);
        let now = now_ms();
        Ok(sqlx::query_scalar!(
            r#"SELECT id as "id!" FROM invites WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2"#,
            hash,
            now
        )
        .fetch_optional(&self.db)
        .await?)
    }

    /// Whether `token` is an unused, unexpired invite.
    pub async fn invite_is_valid(&self, token: &str) -> Result<bool, AuthError> {
        Ok(self.valid_invite(token).await?.is_some())
    }

    // ------------------------------------------------------------------ passkeys

    /// Whether any passkey is enrolled (if not, the sign-in page explains how to start).
    pub async fn has_passkeys(&self) -> Result<bool, AuthError> {
        let n = sqlx::query_scalar!(r#"SELECT count(*) as "n!: i64" FROM passkeys"#)
            .fetch_one(&self.db)
            .await?;
        Ok(n > 0)
    }

    /// The single admin user's id and WebAuthn handle, creating the user on first use.
    async fn admin_user(&self) -> Result<(i64, Uuid), AuthError> {
        if let Some(row) =
            sqlx::query!(r#"SELECT id as "id!", uuid FROM users ORDER BY id LIMIT 1"#)
                .fetch_optional(&self.db)
                .await?
        {
            let uuid = Uuid::parse_str(&row.uuid)
                .map_err(|e| AuthError::Invalid(format!("stored user handle is invalid: {e}")))?;
            return Ok((row.id, uuid));
        }
        let uuid = Uuid::new_v4();
        let (text, now) = (uuid.to_string(), now_ms());
        let id = sqlx::query!(
            "INSERT INTO users (uuid, name, created_at) VALUES (?1, 'admin', ?2)",
            text,
            now
        )
        .execute(&self.db)
        .await?
        .last_insert_rowid();
        Ok((id, uuid))
    }

    async fn load_passkeys(&self) -> Result<Vec<(i64, Passkey)>, AuthError> {
        let rows = sqlx::query!(r#"SELECT id as "id!", data FROM passkeys ORDER BY id"#)
            .fetch_all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r.id, serde_json::from_str(&r.data).ok()?)))
            .collect())
    }

    /// Begin enrolling a passkey. Allowed with a valid invite token, or for a signed-in
    /// session. Returns a ceremony id and the options for `navigator.credentials.create`.
    pub async fn register_start(
        &self,
        invite: Option<&str>,
        session: Option<&Session>,
    ) -> Result<(String, CreationChallengeResponse), AuthError> {
        let grant = match (invite, session) {
            (_, Some(_)) => RegistrationGrant::Session,
            (Some(token), None) => match self.valid_invite(token).await? {
                Some(id) => RegistrationGrant::Invite(id),
                None => {
                    return Err(AuthError::Denied(
                        "this enrollment link has expired or was already used".into(),
                    ));
                }
            },
            (None, None) => {
                return Err(AuthError::Denied(
                    "sign in or use an enrollment link first".into(),
                ));
            }
        };
        let (_, user_uuid) = self.admin_user().await?;
        let existing: Vec<_> = self
            .load_passkeys()
            .await?
            .iter()
            .map(|(_, p)| p.cred_id().clone())
            .collect();
        let (options, state) = self
            .webauthn
            .start_passkey_registration(user_uuid, "admin", "Sitewright admin", Some(existing))
            .map_err(|e| AuthError::Webauthn(e.to_string()))?;
        let ceremony = random_token();
        let mut pending = locked(&self.registrations);
        pending.retain(|_, p| p.started.elapsed() < CEREMONY_LIFETIME);
        pending.insert(
            ceremony.clone(),
            PendingRegistration {
                started: Instant::now(),
                state,
                grant,
                user_uuid,
            },
        );
        Ok((ceremony, options))
    }

    /// Finish enrolling a passkey. An invite-based enrollment consumes the invite and
    /// signs the browser in; a session-based one returns `None`.
    pub async fn register_finish(
        &self,
        ceremony: &str,
        credential: &RegisterPublicKeyCredential,
        name: &str,
        user_agent: &str,
    ) -> Result<Option<NewSession>, AuthError> {
        let pending = locked(&self.registrations)
            .remove(ceremony)
            .filter(|p| p.started.elapsed() < CEREMONY_LIFETIME)
            .ok_or_else(|| AuthError::Denied("the enrollment timed out; start again".into()))?;
        let passkey = self
            .webauthn
            .finish_passkey_registration(credential, &pending.state)
            .map_err(|e| AuthError::Webauthn(e.to_string()))?;

        let (user_id, user_uuid) = self.admin_user().await?;
        if user_uuid != pending.user_uuid {
            return Err(AuthError::Denied(
                "the enrollment does not match the admin user".into(),
            ));
        }
        let now = now_ms();
        let mut tx = sw_core::db::begin_write(&self.db).await?;
        if let RegistrationGrant::Invite(invite_id) = pending.grant {
            // Claim the invite atomically: two enrollments cannot share one link.
            let claimed = sqlx::query!("UPDATE invites SET used_at = ?1 WHERE id = ?2 AND used_at IS NULL AND expires_at > ?1", now, invite_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if claimed != 1 {
                return Err(AuthError::Denied(
                    "this enrollment link has expired or was already used".into(),
                ));
            }
        }
        let credential_id = URL_SAFE_NO_PAD.encode(passkey.cred_id());
        let data =
            serde_json::to_string(&passkey).map_err(|e| AuthError::Webauthn(e.to_string()))?;
        let name = clean_name(name, "Passkey");
        sqlx::query!(
            "INSERT INTO passkeys (user_id, credential_id, name, data, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            user_id,
            credential_id,
            name,
            data,
            now
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        sw_core::db::audit(&self.db, "admin", "auth.passkey.enroll", Some(&name), None).await;

        match pending.grant {
            RegistrationGrant::Invite(_) => {
                Ok(Some(self.create_session(user_id, user_agent).await?))
            }
            RegistrationGrant::Session => Ok(None),
        }
    }

    /// Begin signing in. Returns a ceremony id and the options for
    /// `navigator.credentials.get`.
    pub async fn login_start(&self) -> Result<(String, RequestChallengeResponse), AuthError> {
        let passkeys: Vec<Passkey> = self
            .load_passkeys()
            .await?
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        if passkeys.is_empty() {
            return Err(AuthError::Denied(
                "no passkey is enrolled yet; run `sitewright admin invite` on the server to get an enrollment link".into(),
            ));
        }
        let (options, state) = self
            .webauthn
            .start_passkey_authentication(&passkeys)
            .map_err(|e| AuthError::Webauthn(e.to_string()))?;
        let ceremony = random_token();
        let mut pending = locked(&self.logins);
        pending.retain(|_, p| p.started.elapsed() < CEREMONY_LIFETIME);
        pending.insert(
            ceremony.clone(),
            PendingLogin {
                started: Instant::now(),
                state,
            },
        );
        Ok((ceremony, options))
    }

    /// Finish signing in and create a session.
    pub async fn login_finish(
        &self,
        ceremony: &str,
        credential: &PublicKeyCredential,
        user_agent: &str,
    ) -> Result<NewSession, AuthError> {
        let pending = locked(&self.logins)
            .remove(ceremony)
            .filter(|p| p.started.elapsed() < CEREMONY_LIFETIME)
            .ok_or_else(|| AuthError::Denied("the sign-in timed out; try again".into()))?;
        let result = self
            .webauthn
            .finish_passkey_authentication(credential, &pending.state)
            .map_err(|_| AuthError::Denied("that passkey was not accepted".into()))?;

        let now = now_ms();
        let mut user_id = None;
        for (id, mut passkey) in self.load_passkeys().await? {
            if passkey.cred_id() != result.cred_id() {
                continue;
            }
            // Keep the signature counter current, so a cloned authenticator is noticed.
            passkey.update_credential(&result);
            let data =
                serde_json::to_string(&passkey).map_err(|e| AuthError::Webauthn(e.to_string()))?;
            user_id = sqlx::query_scalar!(
                "UPDATE passkeys SET data = ?1, last_used_at = ?2 WHERE id = ?3 RETURNING user_id",
                data,
                now,
                id
            )
            .fetch_optional(&self.db)
            .await?;
        }
        let user_id = user_id
            .ok_or_else(|| AuthError::Denied("that passkey is no longer enrolled".into()))?;
        sw_core::db::audit(&self.db, "admin", "auth.login", None, None).await;
        self.create_session(user_id, user_agent).await
    }

    pub async fn list_passkeys(&self) -> Result<Vec<PasskeyInfo>, AuthError> {
        Ok(sqlx::query_as!(
            PasskeyInfo,
            r#"SELECT id as "id!", name, created_at, last_used_at FROM passkeys ORDER BY id"#
        )
        .fetch_all(&self.db)
        .await?)
    }

    /// Remove a passkey. The last one cannot be removed from the UI: that would lock the
    /// admin out until someone runs `sitewright admin invite`.
    pub async fn delete_passkey(&self, id: i64) -> Result<(), AuthError> {
        let removed = sqlx::query!(
            "DELETE FROM passkeys WHERE id = ?1 AND (SELECT count(*) FROM passkeys) > 1",
            id
        )
        .execute(&self.db)
        .await?
        .rows_affected();
        if removed == 0 {
            return Err(AuthError::Invalid(
                "the last passkey cannot be removed; add another one first".into(),
            ));
        }
        sw_core::db::audit(&self.db, "admin", "auth.passkey.delete", None, None).await;
        Ok(())
    }

    // ------------------------------------------------------------------ sessions

    async fn create_session(
        &self,
        user_id: i64,
        user_agent: &str,
    ) -> Result<NewSession, AuthError> {
        let token = random_token();
        let (hash, csrf, now) = (hash_token(&token), random_token(), now_ms());
        let expires = now + SESSION_DAYS * MS_PER_DAY;
        let user_agent: String = user_agent.chars().take(200).collect();
        let id = sqlx::query!(
            "INSERT INTO sessions (token_hash, user_id, csrf_token, user_agent, created_at, last_seen_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            hash,
            user_id,
            csrf,
            user_agent,
            now,
            expires
        )
        .execute(&self.db)
        .await?
        .last_insert_rowid();
        // Housekeeping: expired sessions and invites have no further use.
        sqlx::query!("DELETE FROM sessions WHERE expires_at <= ?1", now)
            .execute(&self.db)
            .await?;
        sqlx::query!("DELETE FROM invites WHERE expires_at <= ?1", now)
            .execute(&self.db)
            .await?;
        Ok(NewSession {
            session: Session {
                id,
                user_id,
                csrf_token: csrf,
            },
            token,
        })
    }

    /// Create a session without a passkey ceremony. For tests and nothing else.
    #[doc(hidden)]
    pub async fn session_for_tests(&self) -> Result<NewSession, AuthError> {
        let (user_id, _) = self.admin_user().await?;
        self.create_session(user_id, "test").await
    }

    /// The session a cookie value belongs to, if it is still valid.
    pub async fn session(&self, token: &str) -> Result<Option<Session>, AuthError> {
        let hash = hash_token(token);
        let now = now_ms();
        let row = sqlx::query!(
            r#"SELECT id as "id!", user_id, csrf_token, last_seen_at FROM sessions WHERE token_hash = ?1 AND expires_at > ?2"#,
            hash,
            now
        )
        .fetch_optional(&self.db)
        .await?;
        let Some(row) = row else { return Ok(None) };
        // Touch at most once a minute; this runs on every request.
        if now - row.last_seen_at > 60_000 {
            sqlx::query!(
                "UPDATE sessions SET last_seen_at = ?1 WHERE id = ?2",
                now,
                row.id
            )
            .execute(&self.db)
            .await?;
        }
        Ok(Some(Session {
            id: row.id,
            user_id: row.user_id,
            csrf_token: row.csrf_token,
        }))
    }

    /// End a session.
    pub async fn logout(&self, session: &Session) -> Result<(), AuthError> {
        sqlx::query!("DELETE FROM sessions WHERE id = ?1", session.id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// The `Set-Cookie` value that stores a session token.
    pub fn session_cookie(token: &str) -> String {
        format!(
            "{SESSION_COOKIE}={token}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Strict",
            SESSION_DAYS * 86_400
        )
    }

    /// The `Set-Cookie` value that removes the session cookie.
    pub fn clear_cookie() -> String {
        format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Strict")
    }

    /// Extract the session token from a `Cookie` header.
    pub fn token_from_cookie_header(header: &str) -> Option<&str> {
        header.split(';').find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == SESSION_COOKIE && !value.is_empty()).then_some(value)
        })
    }

    // ------------------------------------------------------------------ API tokens

    /// Create an API token and return it. It is shown once; only its hash is stored.
    pub async fn create_api_token(&self, name: &str) -> Result<String, AuthError> {
        let token = format!("{API_TOKEN_PREFIX}{}", random_token());
        let hash = hash_token(&token);
        let prefix: String = token.chars().take(API_TOKEN_PREFIX.len() + 6).collect();
        let (name, now) = (clean_name(name, "API token"), now_ms());
        sqlx::query!(
            "INSERT INTO api_tokens (name, token_hash, prefix, created_at) VALUES (?1, ?2, ?3, ?4)",
            name,
            hash,
            prefix,
            now
        )
        .execute(&self.db)
        .await?;
        sw_core::db::audit(&self.db, "admin", "auth.token.create", Some(&name), None).await;
        Ok(token)
    }

    /// Whether `token` is a valid API token. Records its use.
    pub async fn verify_api_token(&self, token: &str) -> Result<bool, AuthError> {
        if !token.starts_with(API_TOKEN_PREFIX) {
            return Ok(false);
        }
        let hash = hash_token(token);
        let now = now_ms();
        let used = sqlx::query!(
            "UPDATE api_tokens SET last_used_at = ?1 WHERE token_hash = ?2",
            now,
            hash
        )
        .execute(&self.db)
        .await?
        .rows_affected();
        Ok(used == 1)
    }

    pub async fn list_api_tokens(&self) -> Result<Vec<ApiTokenInfo>, AuthError> {
        Ok(sqlx::query_as!(
            ApiTokenInfo,
            r#"SELECT id as "id!", name, prefix, created_at, last_used_at FROM api_tokens ORDER BY id"#
        )
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn delete_api_token(&self, id: i64) -> Result<(), AuthError> {
        sqlx::query!("DELETE FROM api_tokens WHERE id = ?1", id)
            .execute(&self.db)
            .await?;
        sw_core::db::audit(&self.db, "admin", "auth.token.delete", None, None).await;
        Ok(())
    }
}

/// Trim a user-supplied label and bound its length; fall back to `default` when empty.
fn clean_name(name: &str, default: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    if cleaned.is_empty() {
        default.to_owned()
    } else {
        cleaned
    }
}

/// A sliding-window limiter on authentication attempts per client address.
#[derive(Debug)]
pub struct RateLimiter {
    max: usize,
    window: Duration,
    attempts: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// Record an attempt and say whether it is within the limit.
    pub fn allow(&self, ip: IpAddr) -> bool {
        let mut attempts = locked(&self.attempts);
        let now = Instant::now();
        // Forget addresses that have been quiet, so the map cannot grow without bound.
        attempts.retain(|_, times| {
            times
                .last()
                .is_some_and(|t| now.duration_since(*t) < self.window)
        });
        let times = attempts.entry(ip).or_default();
        times.retain(|t| now.duration_since(*t) < self.window);
        if times.len() >= self.max {
            return false;
        }
        times.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn auth() -> Auth {
        let db = sw_core::db::open_in_memory().await.unwrap();
        Auth::new(db, "admin.example.com", "https://admin.example.com").unwrap()
    }

    #[test]
    fn rate_limiter_blocks_after_the_limit_per_address() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        let a: IpAddr = "203.0.113.1".parse().unwrap();
        let b: IpAddr = "203.0.113.2".parse().unwrap();
        assert!(limiter.allow(a) && limiter.allow(a) && limiter.allow(a));
        assert!(!limiter.allow(a));
        assert!(limiter.allow(b), "other addresses are unaffected");
    }

    #[test]
    fn cookies_are_host_only_secure_and_strict() {
        let cookie = Auth::session_cookie("abc");
        assert!(cookie.starts_with("__Host-sw_session=abc; Path=/;"));
        for attribute in ["Secure", "HttpOnly", "SameSite=Strict", "Max-Age=2592000"] {
            assert!(cookie.contains(attribute), "{attribute}");
        }
        assert!(
            !cookie.contains("Domain"),
            "a Domain attribute would share the cookie with site subdomains"
        );
        assert_eq!(
            Auth::token_from_cookie_header("a=b; __Host-sw_session=tok; c=d"),
            Some("tok")
        );
        assert_eq!(Auth::token_from_cookie_header("sw_session=tok"), None);
        assert_eq!(Auth::token_from_cookie_header("__Host-sw_session="), None);
    }

    #[tokio::test]
    async fn sessions_validate_expire_and_end() {
        let auth = auth().await;
        let new = auth.session_for_tests().await.unwrap();
        let found = auth.session(&new.token).await.unwrap().unwrap();
        assert_eq!(found.csrf_token, new.session.csrf_token);
        assert!(auth.session("wrong").await.unwrap().is_none());
        // Only the hash is stored.
        let (stored,): (String,) = sqlx::query_as("SELECT token_hash FROM sessions")
            .fetch_one(&auth.db)
            .await
            .unwrap();
        assert_ne!(stored, new.token);
        assert_eq!(stored.len(), 64);

        sqlx::query("UPDATE sessions SET expires_at = 1")
            .execute(&auth.db)
            .await
            .unwrap();
        assert!(auth.session(&new.token).await.unwrap().is_none(), "expired");

        let again = auth.session_for_tests().await.unwrap();
        auth.logout(&again.session).await.unwrap();
        assert!(auth.session(&again.token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn invites_are_single_purpose_and_expire() {
        let auth = auth().await;
        let token = auth.create_invite().await.unwrap();
        assert!(auth.invite_is_valid(&token).await.unwrap());
        assert!(!auth.invite_is_valid("nope").await.unwrap());
        // A valid invite starts a registration; an invalid one does not.
        assert!(auth.register_start(Some(&token), None).await.is_ok());
        assert!(matches!(
            auth.register_start(Some("nope"), None).await,
            Err(AuthError::Denied(_))
        ));
        assert!(matches!(
            auth.register_start(None, None).await,
            Err(AuthError::Denied(_))
        ));

        sqlx::query("UPDATE invites SET expires_at = 1")
            .execute(&auth.db)
            .await
            .unwrap();
        assert!(!auth.invite_is_valid(&token).await.unwrap());
        assert!(
            matches!(auth.login_start().await, Err(AuthError::Denied(_))),
            "no passkeys yet"
        );
    }

    #[tokio::test]
    async fn api_tokens_are_hashed_and_revocable() {
        let auth = auth().await;
        let token = auth.create_api_token("  ci\n ").await.unwrap();
        assert!(token.starts_with("swt_"));
        assert!(auth.verify_api_token(&token).await.unwrap());
        assert!(!auth.verify_api_token("swt_wrong").await.unwrap());
        assert!(!auth.verify_api_token("other").await.unwrap());
        let listed = auth.list_api_tokens().await.unwrap();
        assert_eq!(listed[0].name, "ci");
        assert!(token.starts_with(&listed[0].prefix) && listed[0].prefix.len() == 10);
        assert!(listed[0].last_used_at.is_some());
        let (stored,): (String,) = sqlx::query_as("SELECT token_hash FROM api_tokens")
            .fetch_one(&auth.db)
            .await
            .unwrap();
        assert!(!stored.contains(&token[4..]));
        auth.delete_api_token(listed[0].id).await.unwrap();
        assert!(!auth.verify_api_token(&token).await.unwrap());
    }
}
