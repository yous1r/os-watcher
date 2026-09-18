//! Admin authentication for management endpoints.
//!
//! A single password from the config guards the management API; sessions live in
//! memory only, so a restart (including a self-upgrade) requires a fresh login.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use axum::http::{header, HeaderMap};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tokio::sync::RwLock;
use tracing::warn;
use uuid::Uuid;

use crate::config::AuthConfig;

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "osw_session";

/// Consecutive failed logins before the endpoint locks out.
const MAX_FAILURES: u32 = 5;
/// How long a lockout lasts.
const LOCKOUT_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    /// `[auth] enabled = false`: management endpoints are open, nobody logs in.
    Disabled,
    /// Authentication is enabled but no password was configured.
    NotConfigured,
    /// Wrong password.
    BadPassword,
    /// Too many consecutive failures.
    RateLimited,
}

#[derive(Clone)]
pub struct AuthManager {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    enabled: bool,
    password: String,
    ttl: ChronoDuration,
    /// Session token -> expiry.
    sessions: RwLock<HashMap<String, DateTime<Utc>>>,
    failures: Mutex<Failures>,
}

#[derive(Default)]
struct Failures {
    count: u32,
    locked_until: Option<Instant>,
}

impl AuthManager {
    pub fn new(config: &AuthConfig) -> Self {
        if config.enabled && config.password.is_empty() {
            warn!("[auth] 已启用但未设置 password，管理接口将拒绝所有请求");
        }

        Self {
            inner: Arc::new(AuthInner {
                enabled: config.enabled,
                password: config.password.clone(),
                ttl: ChronoDuration::hours(config.session_ttl_hours.max(1) as i64),
                sessions: RwLock::new(HashMap::new()),
                failures: Mutex::new(Failures::default()),
            }),
        }
    }

    /// Whether management endpoints require a session at all.
    pub fn enabled(&self) -> bool {
        self.inner.enabled
    }

    /// Whether a password is configured. When authentication is enabled but this
    /// is false, management endpoints fail with a configuration hint instead of
    /// silently allowing every caller through.
    pub fn password_configured(&self) -> bool {
        !self.inner.password.is_empty()
    }

    /// Session lifetime in seconds, for the cookie's `Max-Age`.
    pub fn session_ttl_secs(&self) -> u64 {
        self.inner.ttl.num_seconds().max(1) as u64
    }

    pub async fn login(&self, password: &str) -> Result<String, LoginError> {
        if !self.inner.enabled {
            return Err(LoginError::Disabled);
        }
        if self.inner.password.is_empty() {
            return Err(LoginError::NotConfigured);
        }
        if self.is_locked() {
            return Err(LoginError::RateLimited);
        }

        if !constant_time_eq(password, &self.inner.password) {
            self.record_failure();
            return Err(LoginError::BadPassword);
        }

        self.clear_failures();

        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let now = Utc::now();
        let mut sessions = self.inner.sessions.write().await;
        sessions.retain(|_, expires_at| *expires_at > now);
        sessions.insert(token.clone(), now + self.inner.ttl);

        Ok(token)
    }

    pub async fn logout(&self, token: &str) {
        self.inner.sessions.write().await.remove(token);
    }

    /// Whether a session token is still valid. Always true when auth is disabled.
    pub async fn validate(&self, token: &str) -> bool {
        if !self.inner.enabled {
            return true;
        }

        let now = Utc::now();
        let valid = {
            let sessions = self.inner.sessions.read().await;
            sessions
                .get(token)
                .is_some_and(|expires_at| *expires_at > now)
        };

        if !valid {
            let mut sessions = self.inner.sessions.write().await;
            if sessions
                .get(token)
                .is_some_and(|expires_at| *expires_at <= now)
            {
                sessions.remove(token);
            }
        }

        valid
    }

    fn failures(&self) -> MutexGuard<'_, Failures> {
        self.inner
            .failures
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn is_locked(&self) -> bool {
        let mut failures = self.failures();
        match failures.locked_until {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                failures.locked_until = None;
                failures.count = 0;
                false
            }
            None => false,
        }
    }

    fn record_failure(&self) {
        let mut failures = self.failures();
        failures.count += 1;
        if failures.count >= MAX_FAILURES {
            failures.locked_until = Some(Instant::now() + std::time::Duration::from_secs(LOCKOUT_SECS));
            failures.count = 0;
        }
    }

    fn clear_failures(&self) {
        let mut failures = self.failures();
        failures.count = 0;
        failures.locked_until = None;
    }
}

/// Compare two secrets without leaking their length or first difference through
/// timing. Both sides are ASCII passwords, so a byte-wise loop is enough.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Extract the session token from a `Cookie` header.
pub fn session_token_from(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name.trim() == SESSION_COOKIE).then(|| value.trim().to_string())
    })
}

/// `Set-Cookie` value for a fresh session. No `Secure` flag: the panel usually
/// runs on plain HTTP inside a trusted LAN.
pub fn session_cookie(token: &str, ttl_secs: u64) -> String {
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={ttl_secs}")
}

pub fn cleared_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(password: &str) -> AuthManager {
        AuthManager::new(&AuthConfig {
            enabled: true,
            password: password.to_string(),
            session_ttl_hours: 12,
        })
    }

    #[tokio::test]
    async fn five_failed_attempts_lock_out_even_the_right_password() {
        let auth = manager("secret");

        for _ in 0..MAX_FAILURES {
            assert_eq!(
                auth.login("nope").await.unwrap_err(),
                LoginError::BadPassword
            );
        }

        assert_eq!(
            auth.login("nope").await.unwrap_err(),
            LoginError::RateLimited
        );
        assert_eq!(
            auth.login("secret").await.unwrap_err(),
            LoginError::RateLimited,
            "lockout must not be bypassed by the correct password"
        );
    }

    #[tokio::test]
    async fn successful_attempt_resets_the_failure_counter() {
        let auth = manager("secret");

        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(
                auth.login("nope").await.unwrap_err(),
                LoginError::BadPassword
            );
        }

        assert!(auth.login("secret").await.is_ok());
        assert_eq!(
            auth.login("nope").await.unwrap_err(),
            LoginError::BadPassword,
            "counter restarted, so the next failure is not a lockout"
        );
    }

    #[tokio::test]
    async fn session_tokens_validate_until_logged_out() {
        let auth = manager("secret");

        let token = auth.login("secret").await.expect("login should succeed");
        assert!(auth.validate(&token).await);
        assert!(!auth.validate("bogus").await);

        auth.logout(&token).await;
        assert!(!auth.validate(&token).await);
    }

    #[tokio::test]
    async fn disabled_auth_accepts_any_token_and_refuses_logins() {
        let auth = AuthManager::new(&AuthConfig {
            enabled: false,
            ..AuthConfig::default()
        });

        assert!(auth.validate("anything").await);
        assert_eq!(
            auth.login("anything").await.unwrap_err(),
            LoginError::Disabled
        );
    }

    #[tokio::test]
    async fn missing_password_reports_configuration_error() {
        let auth = AuthManager::new(&AuthConfig::default());
        assert_eq!(
            auth.login("").await.unwrap_err(),
            LoginError::NotConfigured
        );
    }

    #[test]
    fn session_token_is_read_from_the_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "other=1; osw_session=abc123; trailing=2"
                .parse()
                .expect("valid header"),
        );
        assert_eq!(session_token_from(&headers).as_deref(), Some("abc123"));

        headers.insert(header::COOKIE, "osw_session_extra=zzz".parse().unwrap());
        assert_eq!(session_token_from(&headers), None);
    }

    #[test]
    fn session_cookie_is_http_only_same_site_and_not_secure() {
        let cookie = session_cookie("abc123", 3600);
        assert!(cookie.contains("osw_session=abc123"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(!cookie.contains("Secure"), "LAN deployments use plain HTTP");
        assert!(cleared_session_cookie().contains("Max-Age=0"));
    }
}
