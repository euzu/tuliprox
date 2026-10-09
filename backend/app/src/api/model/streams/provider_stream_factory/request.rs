use super::{
    error::{
        classify_provider_io_error, classify_provider_status_error, failed_stream_account,
        should_reject_success_response_content_type, ProviderStreamRequestFailure,
    },
    options::ProviderRequestCredentialState,
    response::prepare_provider_stream_response_for_request,
    ProviderOpenGuard, ProviderStreamFactoryFlags, ProviderStreamFactoryOptions, ERR_MAX_RETRY_COUNT, RETRY_SECONDS,
};
use crate::{
    api::model::{ProviderContentRepresentationMode, ProviderStreamFactoryResponse},
    iptv::stalker::client::validate_public_playable_url,
    model::AppConfig,
    utils::{
        content_coding::{apply_outbound_content_coding_policy, OutboundContentCodingPolicy},
        debug_if_enabled,
        request::{
            is_safe_cross_origin_redirect_header, preview_request_diagnostics_for_logging,
            send_with_retry_and_provider_policy_with_options, RequestFetchOptions,
        },
    },
};
use log::{debug, log_enabled, warn};
use reqwest::{
    header::{HeaderMap, HeaderValue, RANGE},
    StatusCode,
};
use shared::{
    defaults::DEFAULT_USER_AGENT,
    utils::{filter_request_header, sanitize_sensitive_info},
};
use std::{
    collections::HashMap,
    io,
    sync::Arc,
    time::{Duration, Instant},
};
use tuliprox_session::stream_ctx::ProviderStreamCtx;
use url::Url;

pub(super) fn merge_provider_request_headers(
    input_headers: Option<&HashMap<String, String>>,
    session_headers: Option<&HashMap<String, String>>,
) -> Option<HashMap<String, String>> {
    match (input_headers, session_headers) {
        (None, None) => None,
        (Some(headers), None) | (None, Some(headers)) => Some(headers.clone()),
        (Some(input), Some(session)) => {
            let mut merged = input.clone();
            for (key, value) in session {
                merged.insert(key.clone(), value.clone());
            }
            Some(merged)
        }
    }
}

pub(super) fn prepare_client(
    request_client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
    url_override: Option<&Url>,
    credential_state: ProviderRequestCredentialState,
) -> (reqwest::RequestBuilder, bool) {
    let original_url = stream_options.get_url();
    let url = url_override.unwrap_or(original_url);
    let requested_range = stream_options.get_requested_range();
    let original_headers = stream_options.get_headers();

    if log_enabled!(log::Level::Debug) {
        let message = format!("original headers {original_headers:?}");
        debug!("{}", sanitize_sensitive_info(&message));
    }

    let mut headers = HeaderMap::default();

    for (key, value) in original_headers {
        if filter_request_header(key.as_str()) {
            headers.insert(key.clone(), value.clone());
        }
    }

    if matches!(credential_state, ProviderRequestCredentialState::Scrubbed) {
        remove_sensitive_headers(&mut headers);
    }
    prepare_default_headers(&mut headers, stream_options);
    let partial = prepare_partial_request_headers(&mut headers, requested_range);
    let content_coding_policy = match stream_options.content_representation() {
        ProviderContentRepresentationMode::PreserveOrigin => OutboundContentCodingPolicy::Inherit,
        ProviderContentRepresentationMode::Identity => OutboundContentCodingPolicy::Identity,
    };
    apply_outbound_content_coding_policy(&mut headers, content_coding_policy);

    if log_enabled!(log::Level::Debug) {
        let message = format!(
            "Stream requested with headers: {:?}",
            headers.iter().map(|header| (header.0, String::from_utf8_lossy(header.1.as_ref()))).collect::<Vec<_>>()
        );
        debug!("{}", sanitize_sensitive_info(&message));
    }

    let request_builder = request_client.get(url.clone()).headers(headers);

    (request_builder, partial)
}

fn remove_sensitive_headers(headers: &mut axum::http::HeaderMap) {
    let names_to_remove =
        headers.keys().filter(|name| !is_safe_cross_origin_redirect_header(name.as_str())).cloned().collect::<Vec<_>>();
    for name in names_to_remove {
        headers.remove(name);
    }
}

pub(super) fn provider_headers_require_manual_redirects(headers: &HeaderMap) -> bool {
    headers.keys().any(|name| !is_safe_cross_origin_redirect_header(name.as_str()))
}

pub(super) fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn prepare_default_headers(headers: &mut axum::http::HeaderMap, stream_options: &ProviderStreamFactoryOptions) {
    // Force Connection: close so the provider releases its slot immediately when the stream ends.
    // This prevents 509 errors from providers counting idle pooled connections against limits.
    headers.insert(axum::http::header::CONNECTION, axum::http::header::HeaderValue::from_static("close"));

    if !headers.contains_key(axum::http::header::USER_AGENT) {
        headers.insert(
            axum::http::header::USER_AGENT,
            stream_options
                .default_user_agent
                .clone()
                .unwrap_or_else(|| axum::http::header::HeaderValue::from_static(DEFAULT_USER_AGENT)),
        );
    }
}

fn prepare_partial_request_headers(headers: &mut HeaderMap, requested_range: Option<&HeaderValue>) -> bool {
    if let Some(range) = requested_range {
        headers.insert(RANGE, range.clone());
        true
    } else {
        false
    }
}

fn collect_debug_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    const HEADER_NAMES: [&str; 8] =
        ["proxy-authenticate", "via", "server", "location", "x-cache", "x-cache-status", "x-served-by", "x-proxy-id"];

    HEADER_NAMES
        .iter()
        .filter_map(|name| {
            headers.get_all(*name).iter().next().map(|value| {
                let value = value.to_str().unwrap_or("<binary>").to_string();
                ((*name).to_string(), value)
            })
        })
        .collect()
}

pub(super) async fn send_with_manual_redirects(
    request_client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
    app_config: &Arc<AppConfig>,
) -> Result<reqwest::Response, std::io::Error> {
    let mut current_url = stream_options.get_url().clone();
    let mut remaining_redirects = 10u8;
    let provider = stream_options.get_provider().cloned();
    let mut credential_state = ProviderRequestCredentialState::OriginalOrigin;

    loop {
        if stream_options.requires_public_destination() {
            validate_public_playable_url(&current_url)
                .await
                .map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err))?;
        }
        let result = send_with_retry_and_provider_policy_with_options(
            app_config,
            &current_url,
            provider.as_ref(),
            true,
            stream_options.should_retry_provider_request(),
            RequestFetchOptions::default()
                .with_http_error_responses(stream_options.flags.contains(ProviderStreamFactoryFlags::HlsResource)),
            |resolved_url| prepare_client(request_client, stream_options, Some(resolved_url), credential_state).0,
        )
        .await;

        let response = match result {
            Ok(resp) => resp,
            Err(e) => {
                // send_with_retry_and_provider already applies provider failover policy.
                // Do not rotate again here, otherwise non-failover errors (e.g. auth) may
                // incorrectly switch provider URLs.
                debug!("Manual redirect failed: {}", sanitize_sensitive_info(&e.to_string()));
                return Err(e);
            }
        };

        let status = response.status();

        if status.is_redirection() {
            if remaining_redirects == 0 {
                return Ok(response);
            }
            let location = response.headers().get(reqwest::header::LOCATION);
            let Some(location) = location else {
                return Ok(response);
            };
            let Ok(location_str) = location.to_str() else {
                return Ok(response);
            };
            let response_url = response.url().clone();
            let next_url = response_url.join(location_str).or_else(|_| Url::parse(location_str));
            let Ok(next_url) = next_url else {
                return Ok(response);
            };
            credential_state.observe_target(&response_url, &next_url);
            current_url = next_url;
            remaining_redirects = remaining_redirects.saturating_sub(1);
            continue;
        }
        return Ok(response);
    }
}

#[allow(clippy::too_many_lines)]
async fn provider_stream_request(
    ctx: &ProviderStreamCtx,
    request_client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
) -> Result<Option<ProviderStreamFactoryResponse>, ProviderStreamRequestFailure> {
    let use_manual_redirects = stream_options.requires_public_destination()
        || tuliprox_core::model::should_use_manual_redirects(&ctx.app_config)
        || provider_headers_require_manual_redirects(stream_options.get_headers());
    if log_enabled!(log::Level::Debug) {
        let diagnostics =
            preview_request_diagnostics_for_logging(stream_options.get_url(), stream_options.get_provider());
        debug!(
            "Provider request diagnostics: manual_redirects={}, {}",
            use_manual_redirects,
            sanitize_sensitive_info(&diagnostics)
        );
    }
    let response_result = if use_manual_redirects {
        let client_no_redirect = if stream_options.requires_public_destination() {
            ctx.http_clients.public_no_redirect.load()
        } else {
            ctx.http_clients.no_redirect.load()
        };
        send_with_manual_redirects(&client_no_redirect, stream_options, &ctx.app_config).await
    } else {
        // Use send_with_retry_and_provider for automatic failover support
        let url = stream_options.get_url();
        let provider = stream_options.get_provider().cloned();

        send_with_retry_and_provider_policy_with_options(
            &ctx.app_config,
            url,
            provider.as_ref(),
            false,
            stream_options.should_retry_provider_request(),
            RequestFetchOptions::default()
                .with_http_error_responses(stream_options.flags.contains(ProviderStreamFactoryFlags::HlsResource)),
            |resolved_url| {
                let (client, _partial_content) = prepare_client(
                    request_client,
                    stream_options,
                    Some(resolved_url),
                    ProviderRequestCredentialState::OriginalOrigin,
                );
                client
            },
        )
        .await
    };
    match response_result {
        Ok(response) => {
            let status = response.status();
            let response_url = response.url().clone();
            if log_enabled!(log::Level::Debug) && !status.is_success() {
                let debug_headers = collect_debug_headers(response.headers());
                let diagnostics =
                    preview_request_diagnostics_for_logging(stream_options.get_url(), stream_options.get_provider());
                let message =
                    format!(
                        "Provider response error: status={status}, url={response_url}, headers={debug_headers:?}, {diagnostics}"
                    );
                debug!("{}", sanitize_sensitive_info(&message));
            }
            if status.is_success() {
                if should_reject_success_response_content_type(stream_options.get_item_type(), response.headers()) {
                    debug!(
                        "Provider returned HTML content for non-adaptive stream {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    return Err(ProviderStreamRequestFailure::Status {
                        status: StatusCode::BAD_GATEWAY,
                        provider_error_class: "unexpected_content_type",
                        serve_channel_unavailable: true,
                    });
                }
                let response = prepare_provider_stream_response_for_request(response, stream_options).await;
                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        let provider_error_class = error.provider_error_class();
                        return Err(ProviderStreamRequestFailure::Status {
                            status: StatusCode::BAD_GATEWAY,
                            provider_error_class,
                            serve_channel_unavailable: true,
                        });
                    }
                };
                if log_enabled!(log::Level::Debug) {
                    // Unfortunately, the HEAD request does not work, so we need this workaround.
                    // We need some header information from the provider, we extract the necessary headers and forward them to the client
                    let message = format!("Provider response info: {:?}", response.info);
                    debug!("{}", sanitize_sensitive_info(&message));
                }
                return Ok(Some(response));
            }

            if stream_options.flags.contains(ProviderStreamFactoryFlags::HlsResource) {
                // Keep range and retry metadata while discarding the provider's error body.
                let headers = response
                    .headers()
                    .iter()
                    .filter(|(name, _)| {
                        matches!(
                            **name,
                            reqwest::header::CONTENT_RANGE
                                | reqwest::header::ACCEPT_RANGES
                                | reqwest::header::RETRY_AFTER
                        )
                    })
                    .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.to_string(), value.to_string())))
                    .collect();
                // Only upstream errors pass through; an unfollowed redirect or other non-success
                // status would reach the client without its Location or body.
                let status =
                    if status.is_client_error() || status.is_server_error() { status } else { StatusCode::BAD_GATEWAY };
                return Err(ProviderStreamRequestFailure::HlsStatus { status, headers });
            }

            if status.is_client_error() {
                debug!("Client error status response : {status}");
                return match status {
                    StatusCode::NOT_FOUND
                    | StatusCode::FORBIDDEN
                    | StatusCode::UNAUTHORIZED
                    | StatusCode::PROXY_AUTHENTICATION_REQUIRED
                    | StatusCode::METHOD_NOT_ALLOWED
                    | StatusCode::BAD_REQUEST => Err(ProviderStreamRequestFailure::Status {
                        status,
                        provider_error_class: classify_provider_status_error(status),
                        serve_channel_unavailable: true,
                    }),
                    _ => Err(ProviderStreamRequestFailure::Status {
                        status,
                        provider_error_class: classify_provider_status_error(status),
                        serve_channel_unavailable: false,
                    }),
                };
            }
            if status.is_server_error() {
                debug!("Server error status response : {status}");
                return match status {
                    StatusCode::INTERNAL_SERVER_ERROR
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT => Err(ProviderStreamRequestFailure::Status {
                        status,
                        provider_error_class: classify_provider_status_error(status),
                        serve_channel_unavailable: true,
                    }),
                    _ => Err(ProviderStreamRequestFailure::Status {
                        status,
                        provider_error_class: classify_provider_status_error(status),
                        serve_channel_unavailable: false,
                    }),
                };
            }
            Err(ProviderStreamRequestFailure::Status {
                status,
                provider_error_class: classify_provider_status_error(status),
                serve_channel_unavailable: false,
            })
        }
        Err(err) => {
            let diagnostics =
                preview_request_diagnostics_for_logging(stream_options.get_url(), stream_options.get_provider());
            debug!(
                "Provider request failed: {}, {}",
                sanitize_sensitive_info(&err.to_string()),
                sanitize_sensitive_info(&diagnostics)
            );
            Err(ProviderStreamRequestFailure::Status {
                status: StatusCode::SERVICE_UNAVAILABLE,
                provider_error_class: classify_provider_io_error(&err),
                serve_channel_unavailable: true,
            })
        }
    }
}

/// Flags the failed account for the Xtream expiry worker. No request is sent here: the worker
/// checks the account only when its per-account, per-panel and cooldown throttles allow it,
/// because frequent account queries can get the account banned.
fn request_provider_account_probe(ctx: &ProviderStreamCtx, options: &ProviderStreamFactoryOptions) {
    let Some(channel) = options.stream_channel.as_ref() else {
        return;
    };
    let sources = ctx.app_config.sources.load_full();
    let Some(input) = sources.get_input_by_name(&channel.input_name) else {
        return;
    };
    if !input.input_type.is_xtream() {
        return;
    }
    if let Some(account) = failed_stream_account(input, options) {
        ctx.active_provider.request_account_probe(&account.name);
    }
}

#[allow(clippy::too_many_lines)]
pub(super) async fn get_provider_stream(
    ctx: &ProviderStreamCtx,
    client: &reqwest::Client,
    stream_options: &ProviderStreamFactoryOptions,
) -> Result<Option<ProviderStreamFactoryResponse>, ProviderStreamRequestFailure> {
    let log_url = stream_options.get_log_url();
    debug_if_enabled!("stream provider {}", sanitize_sensitive_info(log_url.as_ref()));
    let start = Instant::now();
    let mut connect_err: u32 = 1;

    let handle_cancel = stream_options.get_cancel_token();
    while stream_options.should_continue() {
        let request_future = provider_stream_request(ctx, client, stream_options);
        let request_res = if let Some(ref cancel) = handle_cancel {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    debug!(
                        "Provider stream request cancelled during open: {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    return Err(ProviderStreamRequestFailure::Status {
                        status: StatusCode::BAD_GATEWAY,
                        provider_error_class: "cancelled",
                        serve_channel_unavailable: false,
                    });
                }
                res = request_future => res,
            }
        } else {
            request_future.await
        };
        match request_res {
            Ok(Some(stream_response)) => {
                return Ok(Some(stream_response));
            }
            Ok(None) => {
                if connect_err > ERR_MAX_RETRY_COUNT {
                    warn!(
                        "The stream could be unavailable. {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    break;
                }
            }
            Err(failure) => {
                if matches!(failure.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                    request_provider_account_probe(ctx, stream_options);
                }
                if stream_options.flags.contains(ProviderStreamFactoryFlags::HlsResource)
                    || failure.should_serve_channel_unavailable()
                {
                    return Err(failure);
                }
                let status = failure.status();
                debug!("Provider stream response error status response : {status}");
                if matches!(
                    status,
                    StatusCode::FORBIDDEN
                        | StatusCode::SERVICE_UNAVAILABLE
                        | StatusCode::UNAUTHORIZED
                        | StatusCode::PROXY_AUTHENTICATION_REQUIRED
                        | StatusCode::RANGE_NOT_SATISFIABLE
                ) {
                    warn!(
                        "The stream could be unavailable. ({status}) {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    break;
                }
                if connect_err > ERR_MAX_RETRY_COUNT {
                    warn!(
                        "The stream could be unavailable. ({status}) {}",
                        sanitize_sensitive_info(stream_options.get_log_url().as_ref())
                    );
                    break;
                }
            }
        }
        if !stream_options.should_continue() || connect_err > ERR_MAX_RETRY_COUNT {
            break;
        }
        if !stream_options.should_retry_initial_open_loop() {
            break;
        }
        if start.elapsed().as_secs() > RETRY_SECONDS {
            warn!(
                "The stream could be unavailable. Giving up after {RETRY_SECONDS} seconds. {}",
                sanitize_sensitive_info(stream_options.get_log_url().as_ref())
            );
            break;
        }
        connect_err += 1;
        if let Some(ref cancel) = handle_cancel {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        debug_if_enabled!("Reconnecting stream {}", sanitize_sensitive_info(stream_options.get_log_url().as_ref()));
    }
    debug_if_enabled!("Stopped reconnecting stream {}", sanitize_sensitive_info(stream_options.get_log_url().as_ref()));
    stream_options.cancel_reconnect();
    Err(ProviderStreamRequestFailure::Status {
        status: StatusCode::SERVICE_UNAVAILABLE,
        provider_error_class: "service_unavailable",
        serve_channel_unavailable: true,
    })
}

impl Drop for ProviderOpenGuard {
    fn drop(&mut self) {
        if !self.handed_off {
            if let Some(token) = self.token.take() {
                token.cancel();
            }
        }
    }
}
