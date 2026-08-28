//! Deciding whether outbound requests must follow redirects manually.
//!
//! A proxy that speaks HTTP(S) makes automatic redirect handling unreliable, so
//! the affected call sites drive redirects themselves. The predicate reads the
//! configured proxy plus the standard proxy environment variables and nothing
//! else, which is why it lives here rather than on the server's root state.

use crate::model::AppConfig;
use std::ffi::OsStr;
use url::Url;

/// `true` when requests through this configuration must follow redirects
/// manually.
pub fn should_use_manual_redirects(app_config: &AppConfig) -> bool {
    let config = app_config.config.load();
    config.proxy.as_ref().is_some_and(|proxy| should_use_manual_redirect_for_proxy(proxy.url.as_str()))
        || proxy_env_present()
}

pub fn proxy_env_present() -> bool { should_use_manual_redirects_for_env_vars(std::env::vars_os()) }

pub fn parse_proxy_url_with_http_fallback(proxy_url: &str) -> Option<Url> {
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(url) = Url::parse(trimmed) {
        if matches!(url.scheme().to_ascii_lowercase().as_str(), "http" | "https") {
            return Some(url);
        }
        if trimmed.contains("://") {
            return None;
        }
    }

    if trimmed.contains("://") {
        return None;
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return None;
    }

    Url::parse(format!("http://{trimmed}").as_str()).ok()
}

pub fn should_use_manual_redirect_for_proxy(proxy_url: &str) -> bool {
    parse_proxy_url_with_http_fallback(proxy_url).is_some_and(|url| {
        matches!(url.scheme().to_ascii_lowercase().as_str(), "http" | "https") && url.host_str().is_some()
    })
}

pub fn should_use_manual_redirects_for_env_vars<I, K, V>(vars: I) -> bool
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    const ENV_KEYS: [&str; 3] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];

    vars.into_iter().any(|(key, value)| {
        let Some(key) = key.as_ref().to_str() else {
            return false;
        };
        let Some(value) = value.as_ref().to_str() else {
            return false;
        };
        let value = value.trim();
        ENV_KEYS.iter().any(|candidate| candidate.eq_ignore_ascii_case(key))
            && !value.is_empty()
            && should_use_manual_redirect_for_proxy(value)
    })
}
