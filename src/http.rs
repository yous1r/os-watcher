//! Shared outbound HTTP client construction.
//!
//! Both the upgrade checker (long downloads) and the push notifier (short API
//! calls) need the same proxy handling but very different timeouts.

use anyhow::{Context, Result};
use std::time::Duration;

/// Build a client with an explicit timeout, user agent and optional proxy.
///
/// The proxy comes from the explicit value when set, otherwise from the usual
/// `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` environment variables.
pub(crate) fn build_client(
    proxy: Option<&str>,
    timeout: Duration,
    user_agent: &str,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent.to_string())
        .timeout(timeout);

    if let Some(proxy) = resolve_proxy(proxy) {
        builder = builder.proxy(reqwest::Proxy::all(&proxy).context("configure HTTP proxy")?);
    }

    builder.build().context("build HTTP client")
}

/// User agent used for every outbound request.
pub(crate) fn user_agent() -> String {
    format!("os-watcher/{}", env!("CARGO_PKG_VERSION"))
}

pub(crate) fn resolve_proxy(explicit: Option<&str>) -> Option<String> {
    if let Some(proxy) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(proxy.to_string());
    }

    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .filter_map(|key| std::env::var(key).ok())
    .map(|value| value.trim().to_string())
    .find(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static PROXY_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_isolated_proxy_env(test: impl FnOnce()) {
        let _guard = PROXY_ENV_LOCK.lock().expect("proxy env lock poisoned");
        const KEYS: [&str; 6] = [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        let previous = KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect::<Vec<_>>();

        for key in KEYS {
            std::env::remove_var(key);
        }
        test();

        for (key, value) in previous {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }

    #[test]
    fn environment_proxy_is_used_when_no_explicit_proxy_is_configured() {
        with_isolated_proxy_env(|| {
            std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:7890");

            assert_eq!(
                resolve_proxy(None).as_deref(),
                Some("http://127.0.0.1:7890")
            );
        });
    }

    #[test]
    fn explicit_proxy_takes_precedence_over_environment_proxy() {
        with_isolated_proxy_env(|| {
            std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:7890");

            assert_eq!(
                resolve_proxy(Some("http://10.0.0.1:8080")).as_deref(),
                Some("http://10.0.0.1:8080")
            );
        });
    }

    #[test]
    fn blank_explicit_proxy_falls_back_to_the_environment() {
        with_isolated_proxy_env(|| {
            std::env::set_var("HTTP_PROXY", "http://127.0.0.1:8888");

            assert_eq!(
                resolve_proxy(Some("   ")).as_deref(),
                Some("http://127.0.0.1:8888")
            );
        });
    }
}
