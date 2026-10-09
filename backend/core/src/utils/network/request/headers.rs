use super::{preview_attempt_target, AttemptTarget, PROXY_DIAGNOSTICS_ONCE};
use crate::model::{AppConfig, Config, ConfigProvider, InputSource, ReverseProxyDisabledHeaderConfig};
use log::{debug, log_enabled, trace, Level};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use shared::{
    defaults::DEFAULT_USER_AGENT,
    utils::{filter_request_header, sanitize_sensitive_info},
};
use std::{collections::HashMap, sync::Arc};
use url::Url;

pub(super) fn log_proxy_diagnostics(config: &Config) {
    PROXY_DIAGNOSTICS_ONCE.call_once(|| {
        if let Some(proxy_cfg) = config.proxy.as_ref() {
            let sanitized_url = sanitize_sensitive_info(proxy_cfg.url.as_str());
            let has_inline_credentials = proxy_cfg
                .url
                .contains('@')
                || proxy_cfg.url.contains("://")
                && proxy_cfg
                .url
                .split("://")
                .nth(1)
                .is_some_and(|part| part.contains('@'));
            let has_explicit_credentials =
                proxy_cfg.username.as_ref().is_some() || proxy_cfg.password.as_ref().is_some();
            debug!(
                "Proxy config enabled: url={sanitized_url}, credentials_inline={has_inline_credentials}, credentials_fields={has_explicit_credentials}"
            );
        } else {
            debug!("Proxy config disabled (config.yml)");
        }

        let env_keys = [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ];
        let mut env_values = Vec::new();
        for key in env_keys {
            if let Ok(value) = std::env::var(key) {
                if !value.trim().is_empty() {
                    env_values.push((key, sanitize_sensitive_info(value.as_str()).to_string()));
                }
            }
        }
        if env_values.is_empty() {
            debug!("Proxy env vars not set");
        } else {
            debug!("Proxy env vars present: {env_values:?}");
        }
    });
}

pub(super) fn format_request_target_for_logging(target: &AttemptTarget) -> String {
    if target.effective_url.scheme().eq_ignore_ascii_case("https") {
        if let Some(connect_ip) = target.connect_ip {
            format!("{} (connect_ip={connect_ip})", target.request_url)
        } else {
            target.request_url.to_string()
        }
    } else {
        target.effective_url.to_string()
    }
}

pub fn preview_request_target_for_logging(url: &Url, provider: Option<&Arc<ConfigProvider>>) -> String {
    let target = preview_attempt_target(url, provider);
    format_request_target_for_logging(&target)
}

pub fn preview_request_diagnostics_for_logging(url: &Url, provider: Option<&Arc<ConfigProvider>>) -> String {
    let target = preview_attempt_target(url, provider);
    let mut parts = vec![
        format!("request_url={}", sanitize_sensitive_info(target.request_url.as_str())),
        format!("effective_url={}", sanitize_sensitive_info(target.effective_url.as_str())),
    ];

    if let Some(host_header) = target.host_header.as_ref() {
        parts.push(format!("host_header={}", sanitize_sensitive_info(host_header)));
    }
    if let Some(connect_ip) = target.connect_ip {
        parts.push(format!("connect_ip={}", sanitize_sensitive_info(&connect_ip.to_string())));
    }
    if let Some(sni_host) = target.sni_host.as_ref() {
        parts.push(format!("sni_host={}", sanitize_sensitive_info(sni_host)));
    }

    parts.join(", ")
}

pub(super) fn prepare_input_request_headers(
    app_config: &Arc<AppConfig>,
    input: &InputSource,
    headers: Option<&HeaderMap>,
) -> (HashMap<String, String>, Option<String>) {
    let custom_headers = headers
        .map(|h| h.iter().map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec())).collect::<HashMap<_, _>>());

    let config = app_config.config.load();
    let default_user_agent = config.default_user_agent.clone();
    let disabled_headers = config.get_disabled_headers();
    drop(config);

    let merged = get_request_headers(
        Some(&input.headers),
        custom_headers.as_ref(),
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
    );

    let request_headers: HashMap<String, String> = merged
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();

    (request_headers, default_user_agent)
}

pub fn get_request_headers<S: ::std::hash::BuildHasher + Default>(
    request_headers: Option<&HashMap<String, String, S>>,
    custom_headers: Option<&HashMap<String, Vec<u8>, S>>,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::default();
    let mut has_user_agent = false;

    // 1. First, we process the configured request headers (from input config).
    // These should have the highest priority.
    if let Some(req_headers) = request_headers {
        for (key, value) in req_headers {
            if let (Ok(key), Ok(value)) =
                (HeaderName::from_bytes(key.as_bytes()), HeaderValue::from_bytes(value.as_bytes()))
            {
                if filter_request_header(key.as_str()) {
                    if disabled_headers.as_ref().is_some_and(|d| d.should_remove(key.as_str())) {
                        continue;
                    }
                    if key == axum::http::header::USER_AGENT {
                        has_user_agent = true;
                    }
                    headers.insert(key, value);
                }
            }
        }
    }

    // 2. Next, we process custom headers (from the client request).
    // These are only added if they don't already exist in the headers map (i.e., not overridden by config).
    if let Some(custom) = custom_headers {
        for (key, value) in custom {
            let key_lc = key.to_lowercase();
            if filter_request_header(key_lc.as_str()) {
                if disabled_headers.as_ref().is_some_and(|d| d.should_remove(key_lc.as_str())) {
                    continue;
                }
                if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(key.as_bytes()), HeaderValue::from_bytes(value)) {
                    // Only insert if not already present (config takes precedence)
                    if !headers.contains_key(&name) {
                        if name == axum::http::header::USER_AGENT {
                            has_user_agent = true;
                        }
                        headers.insert(name, val);
                    }
                }
            }
        }
    }

    if log_enabled!(Level::Trace) {
        let he: HashMap<String, String> =
            headers.iter().map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string())).collect();
        if !he.is_empty() {
            trace!("Request headers {he:?}");
        }
    }

    // 3. Finally, if no User-Agent was provided by config OR client, use the default.
    if !has_user_agent
        && !disabled_headers.is_some_and(|disabled| disabled.should_remove(axum::http::header::USER_AGENT.as_str()))
    {
        let config_ua = default_user_agent
            .and_then(|ua| {
                let trimmed = ua.trim();
                (!trimmed.is_empty()).then_some(trimmed)
            })
            .and_then(|ua| HeaderValue::from_str(ua).ok());

        headers.insert(
            axum::http::header::USER_AGENT,
            config_ua.unwrap_or_else(|| HeaderValue::from_static(DEFAULT_USER_AGENT)),
        );
    }

    headers
}

pub fn overlay_upstream_user_agent(
    headers: &mut HeaderMap,
    upstream_user_agent: Option<&str>,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
) {
    if disabled_headers.is_some_and(|disabled| disabled.should_remove(axum::http::header::USER_AGENT.as_str())) {
        return;
    }
    if let Some(value) = upstream_user_agent
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| HeaderValue::from_str(value).ok())
    {
        headers.insert(axum::http::header::USER_AGENT, value);
    }
}

/// Appends a stable playback-session index to an already resolved User-Agent.
pub fn append_user_agent_stream_index(headers: &mut HeaderMap, stream_index: u64) {
    let Some(user_agent) = headers.get(axum::http::header::USER_AGENT).map(HeaderValue::as_bytes) else {
        return;
    };
    let Some(first) = user_agent.iter().position(|byte| !byte.is_ascii_whitespace()) else {
        return;
    };
    let last = user_agent.iter().rposition(|byte| !byte.is_ascii_whitespace()).map_or(first, |index| index + 1);
    let user_agent = &user_agent[first..last];

    let mut digits = [0_u8; 20];
    let mut cursor = digits.len();
    let mut remaining = stream_index;
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    let suffix = &digits[cursor..];
    if user_agent.strip_suffix(suffix).is_some_and(|prefix| prefix.ends_with(b" ")) {
        return;
    }

    let mut indexed_user_agent = Vec::with_capacity(user_agent.len() + 1 + suffix.len());
    indexed_user_agent.extend_from_slice(user_agent);
    indexed_user_agent.push(b' ');
    indexed_user_agent.extend_from_slice(suffix);
    if let Ok(value) = HeaderValue::from_bytes(&indexed_user_agent) {
        headers.insert(axum::http::header::USER_AGENT, value);
    }
}
