use super::{
    execute_attempt_request, format_http_status, format_request_target_for_logging, get_client_request,
    is_failover_redirect, prepare_input_request_headers, prepare_physical_request_attempt,
    resolve_attempt_target_at_provider_index, same_origin, strip_sensitive_headers_for_cross_origin_redirect,
    text_response_error_log_label, AttemptTarget, ProviderFailoverResponse, RequestFetchOptions,
    TextContentBodyOptions, TextContentRetryOwner,
};
use crate::{
    model::{AppConfig, ConfigProvider, InputSource, ResourceRetryConfig},
    utils::content_coding::{content_decoding_error_from_io, is_http_body_transport_error, ContentCodingError},
};
use log::{debug, error, log_enabled, warn, Level};
use reqwest::{header::HeaderMap, StatusCode};
use shared::{
    error::string_to_io_error,
    model::{OnConnectErrorPolicy, ProviderUrlSelectionPolicy},
    utils::sanitize_sensitive_info,
};
use std::{
    collections::HashSet,
    io::{Error, ErrorKind},
    net::IpAddr,
    sync::Arc,
    time::Duration,
};
use tokio::time::sleep;
use url::Url;

pub(super) fn provider_start_index(provider: Option<&Arc<ConfigProvider>>) -> usize {
    provider.map_or(0, |provider| match provider.provider_url_selection_policy() {
        ProviderUrlSelectionPolicy::ResumeLastWorking => provider.get_current_index(),
        ProviderUrlSelectionPolicy::RestartFromFirst => 0,
    })
}

pub(super) fn next_provider_url_index(
    current_index: usize,
    provider_url_count: usize,
    start_index: usize,
) -> Option<usize> {
    if provider_url_count <= 1 {
        return None;
    }

    let next_index = (current_index + 1) % provider_url_count;
    (next_index != start_index).then_some(next_index)
}

fn provider_cycle_exhausted(provider: &ConfigProvider, current_index: usize, start_index: usize) -> bool {
    next_provider_url_index(current_index, provider.urls.len(), start_index).is_none()
}

fn log_provider_cycle_exhausted(
    provider: &ConfigProvider,
    start_index: usize,
    current_index: usize,
    last_failure: &str,
) {
    error!(
        "Provider '{}' exhausted all {} URL(s) after one full cycle starting at preferred index {} and ending at index {}: {}",
        provider.name,
        provider.urls.len(),
        start_index,
        current_index,
        sanitize_sensitive_info(last_failure)
    );
}

fn rotate_to_next_provider_url(
    provider: &ConfigProvider,
    provider_url_index: &mut usize,
    start_provider_index: usize,
    reason: &str,
) -> bool {
    let Some(next_index) = next_provider_url_index(*provider_url_index, provider.urls.len(), start_provider_index)
    else {
        return false;
    };

    warn!(
        "Provider '{}' failover: {} -> switching from URL index {} to {}",
        provider.name,
        sanitize_sensitive_info(reason),
        *provider_url_index,
        next_index
    );
    *provider_url_index = next_index;
    true
}

pub(super) fn should_try_next_ip_on_connect_error(
    provider: Option<&Arc<ConfigProvider>>,
    target: &AttemptTarget,
    attempted_ips: &mut HashSet<IpAddr>,
) -> bool {
    let Some(provider) = provider else {
        return false;
    };
    let Some(connect_ip) = target.connect_ip else {
        return false;
    };
    let Some(dns_host) = target.dns_host.as_ref() else {
        return false;
    };
    let Some(dns_cfg) = provider.get_dns_config() else {
        return false;
    };
    if dns_cfg.on_connect_error != OnConnectErrorPolicy::TryNextIp {
        return false;
    }

    let inserted = attempted_ips.insert(connect_ip);
    if !inserted {
        return false;
    }

    let total_ips = provider.ip_count_for_host(dns_host);
    total_ips > attempted_ips.len()
}

#[allow(clippy::too_many_lines)]
async fn send_with_provider_failover_only_with_options(
    app_config: &Arc<AppConfig>,
    url: &Url,
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    options: RequestFetchOptions,
    mut send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<ProviderFailoverResponse, std::io::Error> {
    let failover_patterns = app_config.config.load().reverse_proxy.as_ref().map_or_else(
        || ResourceRetryConfig::default().failover_redirect_patterns,
        |rp| rp.resource_retry.failover_redirect_patterns.clone(),
    );

    let start_provider_index = provider_start_index(provider);
    let mut provider_url_index = start_provider_index;
    let idle_timeout = options.attempt_idle_timeout_or_default();
    let idle = sleep(idle_timeout);
    tokio::pin!(idle);

    'provider_loop: loop {
        let mut attempted_dns_ips = HashSet::new();

        'ip_loop: loop {
            let attempt_target = resolve_attempt_target_at_provider_index(url, provider, provider_url_index);
            if log_enabled!(Level::Debug) {
                if let Some(current_provider) = provider {
                    let attempt_target_log = format_request_target_for_logging(&attempt_target);
                    debug!(
                        "Provider '{}' acquiring URL index {} of {}: {}",
                        current_provider.name,
                        provider_url_index,
                        current_provider.urls.len(),
                        sanitize_sensitive_info(attempt_target_log.as_str())
                    );
                }
            }

            idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
            let (base_client, request) =
                prepare_physical_request_attempt(send(&attempt_target.request_url), &attempt_target, options)?;

            tokio::select! {
                () = &mut idle => {
                    if should_try_next_ip_on_connect_error(provider, &attempt_target, &mut attempted_dns_ips) {
                        continue 'ip_loop;
                    }

                    let last_provider_failure = format!(
                        "idle timeout while trying {}",
                        sanitize_sensitive_info(attempt_target.request_url.as_str())
                    );
                    if let Some(current_provider) = provider {
                        if rotate_to_next_provider_url(
                            current_provider.as_ref(),
                            &mut provider_url_index,
                            start_provider_index,
                            "idle timeout",
                        ) {
                            continue 'provider_loop;
                        }
                        log_provider_cycle_exhausted(
                            current_provider.as_ref(),
                            start_provider_index,
                            provider_url_index,
                            &last_provider_failure,
                        );
                    }

                    return Err(Error::new(
                        ErrorKind::TimedOut,
                        format!("Request timed out: {}", sanitize_sensitive_info(url.as_str())),
                    ));
                }
                result = execute_attempt_request(app_config, base_client, request, &attempt_target) => match result {
                Ok(response) => {
                    let status = response.status();
                    if allow_redirects && status.is_redirection() {
                        if let Some(current_provider) = provider {
                            current_provider.set_current_index(provider_url_index);
                        }
                        return Ok(ProviderFailoverResponse {
                            response,
                            provider_url_index: provider.map(|_| provider_url_index),
                        });
                    }

                    let is_failover = is_failover_redirect(response.url(), &failover_patterns);
                    if !is_failover && !should_trigger_failover(status) {
                        if status.is_success() {
                            if let Some(current_provider) = provider {
                                current_provider.set_current_index(provider_url_index);
                            }
                        }
                        return Ok(ProviderFailoverResponse {
                            response,
                            provider_url_index: provider.map(|_| provider_url_index),
                        });
                    }

                    let last_provider_failure = format!(
                        "status {} while trying {}",
                        format_http_status(status),
                        sanitize_sensitive_info(attempt_target.request_url.as_str())
                    );

                    if let Some(current_provider) = provider {
                        let reason = format!("status {}", format_http_status(status));
                        if rotate_to_next_provider_url(
                            current_provider.as_ref(),
                            &mut provider_url_index,
                            start_provider_index,
                            reason.as_str(),
                        ) {
                            continue 'provider_loop;
                        }
                        log_provider_cycle_exhausted(
                            current_provider.as_ref(),
                            start_provider_index,
                            provider_url_index,
                            &last_provider_failure,
                        );
                    }

                    return Ok(ProviderFailoverResponse {
                        response,
                        provider_url_index: provider.map(|_| provider_url_index),
                    });
                },
                Err(err) => {
                    if (err.is_timeout() || err.is_connect())
                        && should_try_next_ip_on_connect_error(provider, &attempt_target, &mut attempted_dns_ips)
                    {
                        continue 'ip_loop;
                    }

                    let last_provider_failure = format!(
                        "connection error while trying {}: {}",
                        sanitize_sensitive_info(attempt_target.request_url.as_str()),
                        sanitize_sensitive_info(err.to_string().as_str())
                    );

                    if err.is_timeout() || err.is_connect() {
                        if let Some(current_provider) = provider {
                            if rotate_to_next_provider_url(
                                current_provider.as_ref(),
                                &mut provider_url_index,
                                start_provider_index,
                                "connection error",
                            ) {
                                continue 'provider_loop;
                            }
                            log_provider_cycle_exhausted(
                                current_provider.as_ref(),
                                start_provider_index,
                                provider_url_index,
                                &last_provider_failure,
                            );
                        }
                    }

                    let message = format!("Request error: {}", sanitize_sensitive_info(err.to_string().as_str()));
                    return Err(if err.is_timeout() {
                        Error::new(ErrorKind::TimedOut, message)
                    } else if err.is_connect() {
                        Error::new(ErrorKind::ConnectionRefused, message)
                    } else {
                        string_to_io_error(message)
                    });
                },
                }
            }
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
pub fn calculate_retry_backoff(base_delay_ms: u64, multiplier: f64, attempt: u32) -> u64 {
    let base = base_delay_ms.max(1);
    if multiplier <= 1.0 {
        return base;
    }
    let delay = (base as f64) * multiplier.powi(i32::try_from(attempt).unwrap_or(i32::MAX));
    if !delay.is_finite() || delay < 1.0 {
        base
    } else if delay >= u64::MAX as f64 {
        u64::MAX
    } else {
        delay as u64
    }
}

/// Sends a request with retry logic and optional provider failover support.
pub async fn send_with_retry_and_provider(
    app_config: &Arc<AppConfig>,
    url: &Url, // Used primarily for logging/context
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, std::io::Error> {
    send_with_retry_and_provider_policy_with_options(
        app_config,
        url,
        provider,
        allow_redirects,
        true,
        RequestFetchOptions::default(),
        send,
    )
    .await
}

/// Canonical retry and provider-failover entry point for outbound resource requests.
///
/// `send_with_retry_and_provider` is a thin wrapper that enables the standard retry policy. Retry attempt counts,
/// backoff values, and failover redirect patterns are sourced from `AppConfig` (`reverse_proxy.resource_retry`). The
/// `url` argument is used as the stable logging/context URL; callers should pass the original request target rather
/// than an already-rotated provider URL.
///
/// When `retry_enabled` is `false`, this function forces `max_attempts` to 1, disables provider URL rotation for idle
/// timeouts, retryable HTTP statuses, and connection/timeout errors, and skips the final fallback provider rotation
/// after attempts are exhausted.
#[allow(clippy::too_many_lines)]
pub async fn send_with_retry_and_provider_policy(
    app_config: &Arc<AppConfig>,
    url: &Url, // Used primarily for logging/context
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    retry_enabled: bool,
    send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, std::io::Error> {
    send_with_retry_and_provider_policy_with_options(
        app_config,
        url,
        provider,
        allow_redirects,
        retry_enabled,
        RequestFetchOptions::default(),
        send,
    )
    .await
}

#[allow(clippy::too_many_lines)]
pub async fn send_with_retry_and_provider_policy_with_options(
    app_config: &Arc<AppConfig>,
    url: &Url, // Used primarily for logging/context
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    retry_enabled: bool,
    options: RequestFetchOptions,
    send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, std::io::Error> {
    send_with_retry_and_provider_policy_with_options_result(
        app_config,
        url,
        provider,
        allow_redirects,
        retry_enabled,
        options,
        send,
    )
    .await
    .map(|result| result.response)
}

#[allow(clippy::too_many_lines)]
async fn send_with_retry_and_provider_policy_with_options_result(
    app_config: &Arc<AppConfig>,
    url: &Url, // Used primarily for logging/context
    provider: Option<&Arc<ConfigProvider>>,
    allow_redirects: bool,
    retry_enabled: bool,
    options: RequestFetchOptions,
    mut send: impl FnMut(&Url) -> reqwest::RequestBuilder,
) -> Result<ProviderFailoverResponse, std::io::Error> {
    if options.uses_provider_failover_only() {
        return send_with_provider_failover_only_with_options(
            app_config,
            url,
            provider,
            allow_redirects,
            options,
            send,
        )
        .await;
    }

    let config = app_config.config.load();
    let (max_attempts, backoff_ms, backoff_multiplier, failover_patterns) = config.reverse_proxy.as_ref().map_or_else(
        || {
            let (a, b, c) = ResourceRetryConfig::get_default_retry_values();
            (a, b, c, ResourceRetryConfig::default().failover_redirect_patterns)
        },
        |rp| {
            let (a, b, c) = rp.resource_retry.get_retry_values();
            (a, b, c, rp.resource_retry.failover_redirect_patterns.clone())
        },
    );
    let max_attempts = if retry_enabled { max_attempts } else { 1 };
    drop(config);

    let idle_timeout = options.attempt_idle_timeout_or_default();
    let idle = sleep(idle_timeout);
    tokio::pin!(idle);

    let max_provider_attempts = provider.as_ref().map_or(0, |p| p.urls.len());
    let start_provider_index = provider_start_index(provider);
    let mut provider_url_index = start_provider_index;
    let mut last_provider_failure: Option<String> = None;

    'provider_loop: loop {
        // 2. Retry loop for the current URL
        'attempt_loop: for attempt in 0..max_attempts {
            let mut attempted_dns_ips = HashSet::new();

            'ip_loop: loop {
                let attempt_target = resolve_attempt_target_at_provider_index(url, provider, provider_url_index);
                if log_enabled!(Level::Debug) {
                    if let Some(current_provider) = provider {
                        let attempt_target_log = format_request_target_for_logging(&attempt_target);
                        debug!(
                            "Provider '{}' attempting URL index {} of {}: {}",
                            current_provider.name,
                            provider_url_index,
                            max_provider_attempts,
                            sanitize_sensitive_info(attempt_target_log.as_str())
                        );
                    }
                }
                // Reset the idle timer for a new attempt
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);

                let (base_client, request) =
                    prepare_physical_request_attempt(send(&attempt_target.request_url), &attempt_target, options)?;

                tokio::select! {
                    () = &mut idle => {
                        warn!("Request idle for too long: {}", sanitize_sensitive_info(url.as_str()));
                        last_provider_failure = Some(format!(
                            "idle timeout while trying {}",
                            sanitize_sensitive_info(attempt_target.request_url.as_str())
                        ));
                        // 1. Try Provider Failover first
                        let mut provider_failover_exhausted = false;
                        if retry_enabled {
                            if let Some(current_provider) = provider {
                                if rotate_to_next_provider_url(
                                    current_provider.as_ref(),
                                    &mut provider_url_index,
                                    start_provider_index,
                                    "idle timeout",
                                ) {
                                    continue 'provider_loop;
                                }
                            provider_failover_exhausted =
                                max_provider_attempts > 0 && provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index);
                            }
                        }

                        // 2. If no provider or rotation failed, check if we can retry the same URL
                        if attempt < max_attempts - 1 {
                            let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
                            warn!("Idle timeout, retrying same URL in {}ms (attempt {})", delay, attempt + 1);
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                            continue 'attempt_loop;
                        }

                        if provider_failover_exhausted {
                            if let Some(current_provider) = provider {
                                log_provider_cycle_exhausted(
                                    current_provider.as_ref(),
                                    start_provider_index,
                                    provider_url_index,
                                    last_provider_failure.as_deref().unwrap_or("idle timeout"),
                                );
                            }
                        }

                        return Err(Error::new(
                            ErrorKind::TimedOut,
                            format!(
                                "Request timed out and no retries left: {}",
                                sanitize_sensitive_info(url.as_str())
                            ),
                        ));
                    }

                    result = execute_attempt_request(app_config, base_client, request, &attempt_target) => {
                        match result {
                            Ok(response) => {
                                let status = response.status();
                                if allow_redirects && status.is_redirection() {
                                    if let Some(current_provider) = provider {
                                        current_provider.set_current_index(provider_url_index);
                                    }
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }
                                let is_failover = is_failover_redirect(response.url(), &failover_patterns);
                                if !is_failover && status.is_success() {
                                    if let Some(current_provider) = provider {
                                        current_provider.set_current_index(provider_url_index);
                                    }
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }

                                last_provider_failure = Some(format!(
                                    "status {} while trying {}",
                                    format_http_status(status),
                                    sanitize_sensitive_info(attempt_target.request_url.as_str())
                                ));

                                // Failover check: Should we switch to the next provider URL?
                                let provider_failover_exhausted = retry_enabled
                                    && (is_failover || should_trigger_failover(status))
                                    && provider.is_some_and(|current_provider| {
                                        provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index)
                                    });
                                if retry_enabled && (is_failover || should_trigger_failover(status)) {
                                    if let Some(current_provider) = provider {
                                        let reason = format!("status {}", format_http_status(status));
                                        if rotate_to_next_provider_url(
                                            current_provider.as_ref(),
                                            &mut provider_url_index,
                                            start_provider_index,
                                            reason.as_str(),
                                        ) {
                                            continue 'provider_loop;
                                        }
                                    }
                                }

                                // Standard retry check for the same URL
                                let is_retryable = status.is_server_error()
                                    || matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT);

                                if attempt < max_attempts - 1 && is_retryable {
                                    perform_backoff(attempt, backoff_ms, backoff_multiplier, &response).await;
                                    continue 'attempt_loop;
                                }

                                if provider_failover_exhausted {
                                    if let Some(current_provider) = provider {
                                        log_provider_cycle_exhausted(
                                            current_provider.as_ref(),
                                            start_provider_index,
                                            provider_url_index,
                                            last_provider_failure.as_deref().unwrap_or("request failed"),
                                        );
                                    }
                                }

                                if options.return_http_errors && (status.is_client_error() || status.is_server_error()) {
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }

                                return Err(string_to_io_error(format!("Request failed ({}): {}",
                                    format_http_status(status), sanitize_sensitive_info(url.as_str()))));
                            }

                            Err(err) => {
                                // For DNS IP-connect policy, attempt next IP before provider URL rotation.
                                if retry_enabled
                                    && (err.is_timeout() || err.is_connect())
                                    && should_try_next_ip_on_connect_error(provider, &attempt_target, &mut attempted_dns_ips)
                                {
                                    continue 'ip_loop;
                                }

                                last_provider_failure = Some(format!(
                                    "connection error while trying {}: {}",
                                    sanitize_sensitive_info(attempt_target.request_url.as_str()),
                                    sanitize_sensitive_info(&err.to_string())
                                ));

                                // Connection errors (Timeout/Connect) trigger failover if provider exists
                                let provider_failover_exhausted = retry_enabled
                                    && (err.is_timeout() || err.is_connect())
                                    && provider.is_some_and(|current_provider| {
                                        provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index)
                                    });
                                if retry_enabled && (err.is_timeout() || err.is_connect()) {
                                    if let Some(current_provider) = provider {
                                        if rotate_to_next_provider_url(
                                            current_provider.as_ref(),
                                            &mut provider_url_index,
                                            start_provider_index,
                                            "connection error",
                                        ) {
                                            continue 'provider_loop;
                                        }
                                    }
                                }

                                // If not a provider or rotation failed, try standard retry
                                if (err.is_timeout() || err.is_connect()) && attempt < max_attempts - 1 {
                                    let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
                                    tokio::time::sleep(Duration::from_millis(delay)).await;
                                    continue 'attempt_loop;
                                }

                                if provider_failover_exhausted {
                                    if let Some(current_provider) = provider {
                                        log_provider_cycle_exhausted(
                                            current_provider.as_ref(),
                                            start_provider_index,
                                            provider_url_index,
                                            last_provider_failure.as_deref().unwrap_or("request error"),
                                        );
                                    }
                                }

                                let error_message = format!(
                                    "Request error: {}",
                                    sanitize_sensitive_info(&err.to_string())
                                );
                                return Err(if err.is_timeout() {
                                    Error::new(ErrorKind::TimedOut, error_message)
                                } else {
                                    string_to_io_error(error_message)
                                });
                            }
                        }
                    }
                }
            }
        }

        // 2. If per-URL retries are exhausted, try next provider URL as a last resort
        if retry_enabled {
            if let Some(current_provider) = provider {
                if rotate_to_next_provider_url(
                    current_provider.as_ref(),
                    &mut provider_url_index,
                    start_provider_index,
                    "retries exhausted for current URL",
                ) {
                    continue 'provider_loop;
                }

                if max_provider_attempts > 0 {
                    let last_failure =
                        last_provider_failure.as_deref().unwrap_or("all attempts and providers exhausted");
                    log_provider_cycle_exhausted(
                        current_provider.as_ref(),
                        start_provider_index,
                        provider_url_index,
                        last_failure,
                    );
                }
            }
        }

        break;
    }

    Err(string_to_io_error("All attempts and providers exhausted"))
}

#[allow(clippy::implicit_hasher)]
pub async fn send_input_with_retry_and_provider_policy_with_options_result(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    options: RequestFetchOptions,
) -> Result<ProviderFailoverResponse, Error> {
    let (request_headers, default_user_agent) = prepare_input_request_headers(app_config, input, headers);
    send_with_retry_and_provider_policy_with_options_result(
        app_config,
        url,
        input.get_provider(),
        false,
        true,
        options,
        |resolved_url| {
            get_client_request(
                client,
                input.method,
                Some(&request_headers),
                resolved_url,
                None,
                None,
                default_user_agent.as_deref(),
            )
        },
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::implicit_hasher)]
pub async fn send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    headers: Option<&HeaderMap>,
    url: &Url,
    max_redirects: usize,
    options: RequestFetchOptions,
) -> Result<ProviderFailoverResponse, Error> {
    let config = app_config.config.load();
    let (configured_max_attempts, backoff_ms, backoff_multiplier, failover_patterns) =
        config.reverse_proxy.as_ref().map_or_else(
            || {
                let (a, b, c) = ResourceRetryConfig::get_default_retry_values();
                (a, b, c, ResourceRetryConfig::default().failover_redirect_patterns)
            },
            |rp| {
                let (a, b, c) = rp.resource_retry.get_retry_values();
                (a, b, c, rp.resource_retry.failover_redirect_patterns.clone())
            },
        );
    drop(config);
    let provider_failover_only = options.uses_provider_failover_only();
    let max_attempts = if provider_failover_only { 1 } else { configured_max_attempts };

    let (base_headers, default_user_agent) = prepare_input_request_headers(app_config, input, headers);
    let provider = input.get_provider();
    let max_provider_attempts = provider.as_ref().map_or(0, |p| p.urls.len());
    let start_provider_index = provider_start_index(provider);
    let mut provider_url_index = start_provider_index;
    let mut last_provider_failure: Option<String> = None;
    let idle_timeout = options.attempt_idle_timeout_or_default();
    let idle = sleep(idle_timeout);
    tokio::pin!(idle);

    'provider_loop: loop {
        'attempt_loop: for attempt in 0..max_attempts {
            let mut current_url = url.clone();
            let mut current_headers = base_headers.clone();
            let mut remaining_redirects = max_redirects;
            let mut attempted_dns_ips = HashSet::new();

            'redirect_loop: loop {
                let attempt_target =
                    resolve_attempt_target_at_provider_index(&current_url, provider, provider_url_index);
                if log_enabled!(Level::Debug) {
                    if let Some(current_provider) = provider {
                        let attempt_target_log = format_request_target_for_logging(&attempt_target);
                        debug!(
                            "Provider '{}' attempting URL index {} of {}: {}",
                            current_provider.name,
                            provider_url_index,
                            max_provider_attempts,
                            sanitize_sensitive_info(attempt_target_log.as_str())
                        );
                    }
                }
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);

                let request_builder = get_client_request(
                    client,
                    input.method,
                    Some(&current_headers),
                    &attempt_target.request_url,
                    None,
                    None,
                    default_user_agent.as_deref(),
                );
                let (base_client, request) =
                    prepare_physical_request_attempt(request_builder, &attempt_target, options)?;

                tokio::select! {
                    () = &mut idle => {
                        warn!("Request idle for too long: {}", sanitize_sensitive_info(url.as_str()));
                        last_provider_failure = Some(format!(
                            "idle timeout while trying {}",
                            sanitize_sensitive_info(attempt_target.request_url.as_str())
                        ));
                        if let Some(current_provider) = provider {
                            if rotate_to_next_provider_url(
                                current_provider.as_ref(),
                                &mut provider_url_index,
                                start_provider_index,
                                "idle timeout",
                            ) {
                                continue 'provider_loop;
                            }
                            if max_provider_attempts > 0 {
                                log_provider_cycle_exhausted(
                                    current_provider.as_ref(),
                                    start_provider_index,
                                    provider_url_index,
                                    last_provider_failure.as_deref().unwrap_or("idle timeout"),
                                );
                            }
                        }

                        if attempt < max_attempts - 1 {
                            let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
                            warn!("Idle timeout, retrying same URL in {}ms (attempt {})", delay, attempt + 1);
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                            continue 'attempt_loop;
                        }

                        return Err(Error::new(
                            ErrorKind::TimedOut,
                            format!(
                                "Request timed out and no retries left: {}",
                                sanitize_sensitive_info(url.as_str())
                            ),
                        ));
                    }

                    result = execute_attempt_request(app_config, base_client, request, &attempt_target) => {
                        match result {
                            Ok(response) => {
                                if response.status().is_redirection() {
                                    if remaining_redirects == 0 {
                                        return Err(string_to_io_error(format!(
                                            "Too many redirects while requesting {}",
                                            sanitize_sensitive_info(url.as_str())
                                        )));
                                    }

                                    let response_base_url = response.url().clone();
                                    let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                                        return Err(string_to_io_error(format!(
                                            "Redirect response missing location header for {}",
                                            sanitize_sensitive_info(current_url.as_str())
                                        )));
                                    };
                                    let Ok(location_str) = location.to_str() else {
                                        return Err(string_to_io_error(format!(
                                            "Redirect response contains invalid location header for {}",
                                            sanitize_sensitive_info(current_url.as_str())
                                        )));
                                    };
                                    let next_url = response_base_url
                                        .join(location_str)
                                        .or_else(|_| Url::parse(location_str))
                                        .map_err(|_| {
                                            string_to_io_error(format!(
                                                "Redirect response contains invalid location URL for {}",
                                                sanitize_sensitive_info(current_url.as_str())
                                            ))
                                        })?;

                                    if !same_origin(&response_base_url, &next_url) {
                                        strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
                                    }
                                    current_url = next_url;
                                    remaining_redirects = remaining_redirects.saturating_sub(1);
                                    continue 'redirect_loop;
                                }

                                let status = response.status();
                                let is_failover = is_failover_redirect(response.url(), &failover_patterns);
                                if !is_failover && status.is_success() {
                                    if let Some(current_provider) = provider {
                                        current_provider.set_current_index(provider_url_index);
                                    }
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }

                                last_provider_failure = Some(format!(
                                    "status {} while trying {}",
                                    format_http_status(status),
                                    sanitize_sensitive_info(attempt_target.request_url.as_str())
                                ));

                                let provider_failover_exhausted = (is_failover || should_trigger_failover(status))
                                    && provider.is_some_and(|current_provider| {
                                        provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index)
                                    });
                                if is_failover || should_trigger_failover(status) {
                                    if let Some(current_provider) = provider {
                                        let reason = format!("status {}", format_http_status(status));
                                        if rotate_to_next_provider_url(
                                            current_provider.as_ref(),
                                            &mut provider_url_index,
                                            start_provider_index,
                                            reason.as_str(),
                                        ) {
                                            continue 'provider_loop;
                                        }
                                    }
                                }

                                let is_retryable = status.is_server_error()
                                    || matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT);
                                if attempt < max_attempts - 1 && is_retryable {
                                    perform_backoff(attempt, backoff_ms, backoff_multiplier, &response).await;
                                    continue 'attempt_loop;
                                }

                                if provider_failover_exhausted {
                                    if let Some(current_provider) = provider {
                                        log_provider_cycle_exhausted(
                                            current_provider.as_ref(),
                                            start_provider_index,
                                            provider_url_index,
                                            last_provider_failure.as_deref().unwrap_or("request failed"),
                                        );
                                    }
                                }

                                if provider_failover_only
                                    || (options.return_http_errors && (status.is_client_error() || status.is_server_error()))
                                {
                                    return Ok(ProviderFailoverResponse {
                                        response,
                                        provider_url_index: provider.map(|_| provider_url_index),
                                    });
                                }

                                return Err(string_to_io_error(format!(
                                    "Request failed ({}): {}",
                                    format_http_status(status),
                                    sanitize_sensitive_info(url.as_str())
                                )));
                            }
                            Err(err) => {
                                if (err.is_timeout() || err.is_connect())
                                    && should_try_next_ip_on_connect_error(provider, &attempt_target, &mut attempted_dns_ips)
                                {
                                    continue 'redirect_loop;
                                }

                                last_provider_failure = Some(format!(
                                    "connection error while trying {}: {}",
                                    sanitize_sensitive_info(attempt_target.request_url.as_str()),
                                    sanitize_sensitive_info(err.to_string().as_str())
                                ));

                                let provider_failover_exhausted = (err.is_timeout() || err.is_connect())
                                    && provider.is_some_and(|current_provider| {
                                        provider_cycle_exhausted(current_provider.as_ref(), provider_url_index, start_provider_index)
                                    });
                                if err.is_timeout() || err.is_connect() {
                                    if let Some(current_provider) = provider {
                                        if rotate_to_next_provider_url(
                                            current_provider.as_ref(),
                                            &mut provider_url_index,
                                            start_provider_index,
                                            "connection error",
                                        ) {
                                            continue 'provider_loop;
                                        }
                                    }
                                }

                                if (err.is_timeout() || err.is_connect()) && attempt < max_attempts - 1 {
                                    let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
                                    tokio::time::sleep(Duration::from_millis(delay)).await;
                                    continue 'attempt_loop;
                                }

                                if provider_failover_exhausted {
                                    if let Some(current_provider) = provider {
                                        log_provider_cycle_exhausted(
                                            current_provider.as_ref(),
                                            start_provider_index,
                                            provider_url_index,
                                            last_provider_failure.as_deref().unwrap_or("request error"),
                                        );
                                    }
                                }

                                let error_message = format!(
                                    "Request error: {}",
                                    sanitize_sensitive_info(err.to_string().as_str())
                                );
                                return Err(if err.is_timeout() {
                                    Error::new(ErrorKind::TimedOut, error_message)
                                } else if err.is_connect() {
                                    Error::new(ErrorKind::ConnectionRefused, error_message)
                                } else {
                                    string_to_io_error(error_message)
                                });
                            }
                        }
                    }
                }
            }
        }

        if let Some(current_provider) = provider {
            if rotate_to_next_provider_url(
                current_provider.as_ref(),
                &mut provider_url_index,
                start_provider_index,
                "retries exhausted for current URL",
            ) {
                continue 'provider_loop;
            }

            if max_provider_attempts > 0 {
                let last_failure = last_provider_failure.as_deref().unwrap_or("all attempts and providers exhausted");
                log_provider_cycle_exhausted(
                    current_provider.as_ref(),
                    start_provider_index,
                    provider_url_index,
                    last_failure,
                );
            }
        }

        break;
    }

    Err(string_to_io_error("All attempts and providers exhausted"))
}

/// Helper to handle sleep duration for retries, respecting Retry-After headers
pub(super) async fn perform_backoff(attempt: u32, ms: u64, mult: f64, response: &reqwest::Response) {
    let wait_dur = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map_or_else(|| Duration::from_millis(calculate_retry_backoff(ms, mult, attempt)), Duration::from_secs);

    tokio::time::sleep(wait_dur).await;
}

pub(super) fn text_body_retry_values(
    app_config: &Arc<AppConfig>,
    body_options: TextContentBodyOptions,
) -> (u32, u64, f64) {
    if body_options.retry_owner != TextContentRetryOwner::DecodedBodyConsumer {
        return (1, 0, 1.0);
    }
    let config = app_config.config.load();
    let values = config
        .reverse_proxy
        .as_ref()
        .map_or_else(ResourceRetryConfig::get_default_retry_values, |rp| rp.resource_retry.get_retry_values());
    drop(config);
    values
}

pub(super) fn should_retry_text_body_error(error: &Error) -> bool {
    matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::ConnectionRefused)
        || content_decoding_error_from_io(error).is_some()
        || is_http_body_transport_error(error)
        || error
            .get_ref()
            .and_then(|source| source.downcast_ref::<ContentCodingError>())
            .is_some_and(|error| matches!(error, ContentCodingError::PrefixRead(_)))
}

pub(super) async fn sleep_before_text_body_retry(attempt: u32, backoff_ms: u64, backoff_multiplier: f64, err: &Error) {
    let delay = calculate_retry_backoff(backoff_ms, backoff_multiplier, attempt);
    warn!(
        "Text response body failed retryably, retrying in {}ms (attempt {}): {}",
        delay,
        attempt + 1,
        text_response_error_log_label(err)
    );
    tokio::time::sleep(Duration::from_millis(delay)).await;
}

pub(super) fn is_retryable_text_response_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::PROXY_AUTHENTICATION_REQUIRED
                | StatusCode::REQUEST_TIMEOUT
                | StatusCode::TOO_EARLY
                | StatusCode::TOO_MANY_REQUESTS
        )
}

/// Checks if a status code or error indicates a need for failover
///
/// Returns true for server-side errors that might be resolved by trying another URL.
/// Returns false for client-side errors (401, 403, etc.) where the problem is with
/// credentials or permissions, not the server availability.
pub fn should_trigger_failover(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NOT_FOUND
            | StatusCode::GONE
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::BAD_GATEWAY
            | StatusCode::GATEWAY_TIMEOUT
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::PROXY_AUTHENTICATION_REQUIRED
    )
    // Explicitly NOT triggering failover for:
    // - 401 Unauthorized (wrong credentials)
    // - 403 Forbidden (permission issue)
    // - 402 Payment Required (subscription issue)
    // - 451 Unavailable For Legal Reasons (geo-blocking)
    //
    // Note: DO triggering failover for:
    // - 429 Too Many Requests
    // - 408 Request Timeout
    // - 407 Proxy Authentication Required
}
