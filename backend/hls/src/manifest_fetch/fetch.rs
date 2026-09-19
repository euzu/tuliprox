use super::{
    error::{is_hls_retryable_initial_manifest_fetch_error, HlsManifestRejectLogReason, OriginManifestFetchError},
    http::{
        fetch_hls_origin_manifest_recovery_direct_target, origin_manifest_fetch_error_from_io_error,
        resolved_hls_manifest_request_url_from_input, response_to_fetched_manifest, ManifestRecoveryAttemptLogContext,
    },
    model::{FetchedOriginManifest, HlsOriginManifestFetchContext, RetryPolicy},
    selection_log::{log_manifest_initial_attempt, log_manifest_retry_scheduled, ManifestRetryLogKind},
};
#[cfg(any(test, feature = "test-support"))]
use super::{
    http::fetch_origin_manifest_once, model::LiveHlsOriginEntry, selection_log::log_origin_refresh_retry_scheduled,
};
use crate::{
    manifest_origin_binding::HlsManifestOriginBinding, observability::hls_origin_log_value, MAX_MANUAL_REDIRECTS,
};
#[cfg(any(test, feature = "test-support"))]
use axum::http::HeaderMap;
use log::debug;
#[cfg(any(test, feature = "test-support"))]
use reqwest::Client;
use std::time::Duration;
#[cfg(any(test, feature = "test-support"))]
use tokio::time::timeout;
#[cfg(any(test, feature = "test-support"))]
use tuliprox_core::utils::content_coding::ContentCodingError;
use tuliprox_core::utils::{
    content_coding::OutboundContentCodingPolicy,
    request::{
        send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result,
        send_input_with_retry_and_provider_policy_with_options_result, RequestFetchOptions,
    },
};

enum HlsOriginManifestFetchMode<'a> {
    InitialGlobalPolicy,
    RecoveryDirectTarget {
        binding: &'a HlsManifestOriginBinding,
        reason: Option<&'a HlsManifestRejectLogReason>,
        log_context: ManifestRecoveryAttemptLogContext,
    },
}

pub struct HlsOriginManifestFetchRequest<'a> {
    context: &'a HlsOriginManifestFetchContext,
    mode: HlsOriginManifestFetchMode<'a>,
}

impl<'a> HlsOriginManifestFetchRequest<'a> {
    pub const fn initial_global_policy(context: &'a HlsOriginManifestFetchContext) -> Self {
        Self { context, mode: HlsOriginManifestFetchMode::InitialGlobalPolicy }
    }

    pub(super) const fn recovery_direct_target(
        context: &'a HlsOriginManifestFetchContext,
        binding: &'a HlsManifestOriginBinding,
        reason: Option<&'a HlsManifestRejectLogReason>,
        log_context: ManifestRecoveryAttemptLogContext,
    ) -> Self {
        Self { context, mode: HlsOriginManifestFetchMode::RecoveryDirectTarget { binding, reason, log_context } }
    }
}

pub async fn fetch_hls_origin_manifest_request(
    request: HlsOriginManifestFetchRequest<'_>,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    match request.mode {
        HlsOriginManifestFetchMode::InitialGlobalPolicy => {
            fetch_hls_origin_manifest_initial_global_policy(request.context).await
        }
        HlsOriginManifestFetchMode::RecoveryDirectTarget { binding, reason, log_context } => {
            fetch_hls_origin_manifest_recovery_direct_target(request.context, binding, reason, log_context).await
        }
    }
}

async fn fetch_hls_origin_manifest_initial_global_policy(
    context: &HlsOriginManifestFetchContext,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    log_manifest_initial_attempt(context).await;
    let input_source = context.origin_entry.to_input_source();
    let account_binding = {
        let session = context.session.read().await;
        if session.origin_account_binding.is_some() {
            "present"
        } else {
            "absent"
        }
    };
    debug!(
        "HLS origin manifest request started: account_binding={account_binding} request_url={}",
        hls_origin_log_value(input_source.url.as_str())
    );
    let fetch_options = RequestFetchOptions::with_attempt_idle_timeout(Duration::from_millis(
        context.origin_manifest_timeout_ms.max(1),
    ))
    .with_content_coding(OutboundContentCodingPolicy::Identity)
    .without_resource_retries();
    let attempts = context.retry_policy.attempt_count();

    // This HLS loop owns the logical manifest-attempt budget. Each iteration may traverse one bounded provider URL
    // failover cycle and redirect chain, but the generic resource-retry counter is not nested beneath it.
    for attempt_index in 0..attempts {
        let response_result = if context.use_manual_redirects {
            send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result(
                &context.app_config,
                &context.no_redirect_client,
                &input_source,
                Some(&context.headers),
                context.origin_entry.url(),
                MAX_MANUAL_REDIRECTS,
                fetch_options,
            )
            .await
        } else {
            send_input_with_retry_and_provider_policy_with_options_result(
                &context.app_config,
                &context.client,
                &input_source,
                Some(&context.headers),
                context.origin_entry.url(),
                fetch_options,
            )
            .await
        };
        let fetch_result = match response_result {
            Ok(response_result) => {
                let provider_url_index = response_result.provider_url_index;
                let resolved_request_url = resolved_hls_manifest_request_url_from_input(
                    &input_source,
                    provider_url_index,
                    context.origin_entry.url(),
                );
                response_to_fetched_manifest(
                    response_result.response,
                    provider_url_index,
                    resolved_request_url,
                    context.origin_manifest_timeout_ms,
                )
                .await
            }
            Err(err) => Err(origin_manifest_fetch_error_from_io_error(&err)),
        };
        match fetch_result {
            Ok(fetched) => return Ok(fetched.with_attempts(attempt_index + 1)),
            Err(err) if is_hls_retryable_initial_manifest_fetch_error(&err) && attempt_index + 1 < attempts => {
                let retry_after_ms = match &err {
                    OriginManifestFetchError::RetryableStatus(_, retry_after_ms) => *retry_after_ms,
                    _ => None,
                };
                let jitter_ms = if retry_after_ms.is_some() { 0 } else { context.retry_policy.sample_jitter_ms() };
                let delay_ms = next_retry_delay_ms(&context.retry_policy, attempt_index, retry_after_ms, jitter_ms);
                log_manifest_retry_scheduled(
                    context,
                    ManifestRetryLogKind::InitialFetch,
                    attempt_index,
                    attempts,
                    delay_ms,
                    None,
                    Some(&err),
                )
                .await;
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
            Err(err) => return Err(err),
        }
    }
    Err(OriginManifestFetchError::RetryExhausted)
}

pub(super) fn next_retry_delay_ms(
    retry_policy: &RetryPolicy,
    attempt_index: usize,
    retry_after_ms: Option<u64>,
    jitter_ms: u64,
) -> u64 {
    retry_after_ms
        .unwrap_or_else(|| retry_policy.delay_for_attempt_ms(attempt_index + 1, jitter_ms).unwrap_or_default())
}

pub(super) use tuliprox_core::utils::current_time_millis;

#[cfg(any(test, feature = "test-support"))]
pub(super) async fn refresh_from_live_hls_entrypoint_with_retries(
    origin_entry: &LiveHlsOriginEntry,
    headers: &HeaderMap,
    client: &Client,
    no_redirect_client: &Client,
    use_manual_redirects: bool,
    origin_manifest_timeout_ms: u64,
    retry_policy: &RetryPolicy,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let mut retry_after_delay_ms = None;
    let attempts = retry_policy.attempt_count();

    for attempt_index in 0..attempts {
        let delay_ms = retry_after_delay_ms.take().unwrap_or_else(|| {
            let jitter = retry_policy.sample_jitter_ms();
            retry_policy.delay_for_attempt_ms(attempt_index, jitter).unwrap_or_default()
        });
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let fetch_result = timeout(
            Duration::from_millis(origin_manifest_timeout_ms.max(1)),
            fetch_origin_manifest_once(
                origin_entry.url(),
                headers,
                client,
                no_redirect_client,
                use_manual_redirects,
                None,
                origin_manifest_timeout_ms,
            ),
        )
        .await
        .map_err(|_| OriginManifestFetchError::Timeout);

        match fetch_result {
            Ok(Ok(fetched)) => return Ok(fetched.with_attempts(attempt_index + 1)),
            Ok(Err(OriginManifestFetchError::PermanentStatus(status))) => {
                return Err(OriginManifestFetchError::PermanentStatus(status));
            }
            Ok(Err(OriginManifestFetchError::NonRetryableStatus(status))) => {
                return Err(OriginManifestFetchError::NonRetryableStatus(status));
            }
            Ok(Err(OriginManifestFetchError::RetryableStatus(status, retry_after_ms))) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::RetryableStatus(status, retry_after_ms));
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, retry_after_ms, 0),
                    format!("status={}", status.as_u16()),
                );
                retry_after_delay_ms = retry_after_ms;
            }
            Ok(Err(OriginManifestFetchError::Request(err))) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::Request(err));
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None, 0),
                    "error=request",
                );
            }
            Ok(Err(
                err @ (OriginManifestFetchError::ContentDecoding { .. }
                | OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(_))
                | OriginManifestFetchError::Redirect(_)
                | OriginManifestFetchError::Timeout),
            )) => {
                if attempt_index + 1 == attempts {
                    return Err(err);
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None, 0),
                    format!("error={}", err.log_label()),
                );
            }
            Err(OriginManifestFetchError::Timeout) => {
                if attempt_index + 1 == attempts {
                    return Err(OriginManifestFetchError::Timeout);
                }
                log_origin_refresh_retry_scheduled(
                    origin_entry,
                    attempt_index,
                    next_retry_delay_ms(retry_policy, attempt_index, None, 0),
                    "error=timeout",
                );
            }
            Ok(Err(
                err @ (OriginManifestFetchError::RetryExhausted
                | OriginManifestFetchError::RecoveryUnavailable { .. }
                | OriginManifestFetchError::DeterministicTimelineConflict(_)
                | OriginManifestFetchError::ProviderUnavailable(_)
                | OriginManifestFetchError::ContentCoding(_)
                | OriginManifestFetchError::DecodedBodyLimitExceeded { .. }
                | OriginManifestFetchError::InvalidUtf8 { .. }
                | OriginManifestFetchError::LocalRepresentationLimit(_)
                | OriginManifestFetchError::MalformedTransientRepresentation
                | OriginManifestFetchError::CommitGenerationExhausted),
            ))
            | Err(err) => return Err(err),
        }
    }

    Err(OriginManifestFetchError::RetryExhausted)
}
