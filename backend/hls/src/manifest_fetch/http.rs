//! The HTTP half: issuing the manifest request and reading the body.
//!
//! Redirects are followed manually so cross-origin hops can be stripped of
//! credentials, and every transport-level failure is mapped onto
//! `OriginManifestFetchError` here rather than at the call sites.

use super::{
    error::{
        classify_origin_manifest_status, HlsManifestRejectLogReason, OriginManifestFetchError,
        OriginManifestStatusClass,
    },
    FetchedOriginManifest, HlsManifestFetchSelection, HlsOriginManifestFetchContext, MAX_HLS_MANIFEST_BYTES,
};
use crate::{
    extract_hls_provider_session_header_map, hls_origin_log_value, log_hls_origin_content_coding,
    manifest_origin_binding::HlsManifestOriginBinding, safe_session_key, HlsOriginContentCodingObjectKind,
    HlsOriginContentCodingSource, MAX_MANUAL_REDIRECTS,
};
use axum::http::{header, HeaderMap, StatusCode};
use log::debug;
use reqwest::Client;
use shared::utils::sanitize_sensitive_info;
use std::{io, sync::Arc, time::Duration};
use tokio::time::timeout;
use tuliprox_core::{
    model::{resolve_provider_scheme_url_with_provider_index, InputSource},
    utils::content_coding::{
        apply_outbound_content_coding_policy, content_decoding_error_from_io, decode_response_to_identity,
        is_http_body_transport_error, read_utf8_limited, ContentBodyReadError, ContentCodingDetection,
        ContentCodingError, OutboundContentCodingPolicy,
    },
};
use url::Url;

pub(super) async fn fetch_origin_manifest_once(
    entry_url: &Url,
    headers: &HeaderMap,
    client: &Client,
    no_redirect_client: &Client,
    use_manual_redirects: bool,
    provider_url_index: Option<usize>,
    origin_manifest_timeout_ms: u64,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    if use_manual_redirects {
        fetch_origin_manifest_with_manual_redirects(
            entry_url,
            headers,
            no_redirect_client,
            provider_url_index,
            origin_manifest_timeout_ms,
        )
        .await
    } else {
        let mut request_headers = headers.clone();
        apply_outbound_content_coding_policy(&mut request_headers, OutboundContentCodingPolicy::Identity);
        let response = client
            .get(entry_url.clone())
            .headers(request_headers)
            .send()
            .await
            .map_err(|err| origin_manifest_fetch_error_from_reqwest_error(&err))?;
        response_to_fetched_manifest(response, provider_url_index, entry_url.clone(), origin_manifest_timeout_ms).await
    }
}

async fn fetch_origin_manifest_with_manual_redirects(
    entry_url: &Url,
    headers: &HeaderMap,
    client: &Client,
    provider_url_index: Option<usize>,
    origin_manifest_timeout_ms: u64,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let mut current_url = entry_url.clone();
    let mut current_headers = headers.clone();
    let mut remaining_redirects = MAX_MANUAL_REDIRECTS;

    loop {
        let mut request_headers = current_headers.clone();
        apply_outbound_content_coding_policy(&mut request_headers, OutboundContentCodingPolicy::Identity);
        let response = client
            .get(current_url.clone())
            .headers(request_headers)
            .send()
            .await
            .map_err(|err| origin_manifest_fetch_error_from_reqwest_error(&err))?;
        if !response.status().is_redirection() {
            return response_to_fetched_manifest(
                response,
                provider_url_index,
                entry_url.clone(),
                origin_manifest_timeout_ms,
            )
            .await;
        }
        if remaining_redirects == 0 {
            return Err(OriginManifestFetchError::Redirect("too many redirects".to_string()));
        }
        let response_url = response.url().clone();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| OriginManifestFetchError::Redirect("redirect missing location".to_string()))?;
        let next_url = response_url
            .join(location)
            .or_else(|_| Url::parse(location))
            .map_err(|_| OriginManifestFetchError::Redirect("redirect location invalid".to_string()))?;

        if !same_origin(&response_url, &next_url) {
            strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
        }
        current_url = next_url;
        remaining_redirects = remaining_redirects.saturating_sub(1);
    }
}

pub(super) async fn fetch_hls_origin_manifest_recovery_direct_target(
    context: &HlsOriginManifestFetchContext,
    binding: &HlsManifestOriginBinding,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    log_context: ManifestRecoveryAttemptLogContext,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let target_url = binding.request_url();
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let reason =
        reject_reason.map_or_else(|| "pinned-host-recovery".to_string(), HlsManifestRejectLogReason::status_label);
    if log_context.candidates > 1 {
        debug!(
            "Manifest '{}' attempting URL attempt {} of {} candidate {} of {}: request_url={} reason={}",
            session_label,
            log_context.attempt_index + 1,
            log_context.attempts,
            log_context.candidate_index + 1,
            log_context.candidates,
            hls_origin_log_value(target_url.as_str()),
            reason
        );
    } else {
        debug!(
            "Manifest '{}' attempting URL attempt {} of {}: request_url={} reason={}",
            session_label,
            log_context.attempt_index + 1,
            log_context.attempts,
            hls_origin_log_value(target_url.as_str()),
            reason
        );
    }
    match target_url.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(OriginManifestFetchError::Request("invalid non-HTTP manifest recovery binding".to_string()));
        }
    }
    timeout(
        Duration::from_millis(context.origin_manifest_timeout_ms.max(1)),
        fetch_origin_manifest_once(
            target_url,
            &context.headers,
            &context.client,
            &context.no_redirect_client,
            context.use_manual_redirects,
            binding.provider_url_index(),
            context.origin_manifest_timeout_ms,
        ),
    )
    .await
    .map_err(|_| OriginManifestFetchError::Timeout)?
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ManifestRecoveryAttemptLogContext {
    pub(super) attempt_index: usize,
    pub(super) attempts: usize,
    pub(super) candidate_index: usize,
    pub(super) candidates: usize,
}

pub(super) async fn response_to_fetched_manifest(
    response: reqwest::Response,
    provider_url_index: Option<usize>,
    resolved_request_url: Url,
    origin_manifest_timeout_ms: u64,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    let status = response.status();
    debug!(
        "HLS origin manifest response received: request_url={} final_url={} status={}",
        hls_origin_log_value(resolved_request_url.as_str()),
        hls_origin_log_value(response.url().as_str()),
        status.as_u16()
    );
    match classify_origin_manifest_status(status) {
        OriginManifestStatusClass::Success => {
            let (decoded, body) = read_origin_manifest_body(response, origin_manifest_timeout_ms).await?;
            let redirect_host = hls_manifest_redirect_host(&resolved_request_url, &decoded.final_url);
            let provider_session_headers = extract_hls_provider_session_header_map(&decoded.headers);
            Ok(FetchedOriginManifest {
                body,
                final_manifest_url: decoded.final_url.to_string(),
                resolved_request_url: resolved_request_url.to_string(),
                redirect_host,
                provider_url_index,
                provider_session_headers,
                status: decoded.status,
                attempts: 1,
                candidate_requests: 1,
                selection: HlsManifestFetchSelection::Initial,
            })
        }
        OriginManifestStatusClass::Retryable => {
            Err(OriginManifestFetchError::RetryableStatus(status, retry_after_delay_ms(response.headers())))
        }
        OriginManifestStatusClass::PermanentFailure => Err(OriginManifestFetchError::PermanentStatus(status)),
        OriginManifestStatusClass::NonRetryableFailure => Err(OriginManifestFetchError::NonRetryableStatus(status)),
    }
}

async fn read_origin_manifest_body(
    response: reqwest::Response,
    origin_manifest_timeout_ms: u64,
) -> Result<(tuliprox_core::utils::content_coding::DecodedHttpResponse, String), OriginManifestFetchError> {
    let read = async move {
        let mut decoded =
            decode_response_to_identity(response, ContentCodingDetection::DeclaredOrKnownHlsManifestMagic)
                .await
                .map_err(origin_manifest_content_coding_error)?;
        if let Some(observation) = decoded.content_coding_observation() {
            log_hls_origin_content_coding(
                observation,
                HlsOriginContentCodingObjectKind::Manifest,
                false,
                HlsOriginContentCodingSource::Shared,
            );
        }
        let body = read_utf8_limited(&mut decoded.body, MAX_HLS_MANIFEST_BYTES)
            .await
            .map_err(origin_manifest_body_read_error)?;
        Ok((decoded, body))
    };
    timeout(Duration::from_millis(origin_manifest_timeout_ms.max(1)), read)
        .await
        .map_err(|_| OriginManifestFetchError::Timeout)?
}

pub(super) fn origin_manifest_content_coding_error(error: ContentCodingError) -> OriginManifestFetchError {
    match error {
        ContentCodingError::PrefixRead(io_error) => {
            if let Some(decoding_error) = content_decoding_error_from_io(&io_error) {
                OriginManifestFetchError::ContentDecoding { coding: decoding_error.coding }
            } else if io_error.kind() == io::ErrorKind::TimedOut {
                OriginManifestFetchError::Timeout
            } else if is_http_body_transport_error(&io_error) {
                origin_manifest_fetch_error_from_io_error(&io_error)
            } else {
                OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(io_error))
            }
        }
        error => OriginManifestFetchError::ContentCoding(error),
    }
}

fn origin_manifest_body_read_error(error: ContentBodyReadError) -> OriginManifestFetchError {
    match error {
        ContentBodyReadError::LimitExceeded { limit } => OriginManifestFetchError::DecodedBodyLimitExceeded { limit },
        ContentBodyReadError::InvalidUtf8 { valid_up_to, error_len } => {
            OriginManifestFetchError::InvalidUtf8 { valid_up_to, error_len }
        }
        ContentBodyReadError::Io(io_error) => {
            if let Some(decoding_error) = content_decoding_error_from_io(&io_error) {
                OriginManifestFetchError::ContentDecoding { coding: decoding_error.coding }
            } else {
                origin_manifest_fetch_error_from_io_error(&io_error)
            }
        }
    }
}

pub fn hls_manifest_redirect_host(resolved_request_url: &Url, final_url: &Url) -> Option<String> {
    let final_host = final_url.host_str()?;
    (resolved_request_url.host_str() != Some(final_host)).then(|| final_host.to_string())
}

pub fn resolved_hls_manifest_request_url_from_input(
    input_source: &InputSource,
    provider_url_index: Option<usize>,
    fallback_url: &Url,
) -> Url {
    let fallback = || Url::parse(input_source.url.as_str()).unwrap_or_else(|_| fallback_url.clone());
    let (Some(provider), Some(provider_url_index)) = (input_source.get_provider(), provider_url_index) else {
        return fallback();
    };
    match resolve_provider_scheme_url_with_provider_index(
        input_source.url.as_str(),
        Some(Arc::clone(provider)),
        provider_url_index,
    ) {
        Ok((_provider, resolved_url)) => Url::parse(resolved_url.as_ref()).unwrap_or_else(|err| {
            debug!(
                "HLS provider URL resolution returned invalid URL: error={} request_url={}",
                sanitize_sensitive_info(err.to_string().as_str()),
                hls_origin_log_value(input_source.url.as_str())
            );
            fallback()
        }),
        Err(err) => {
            debug!(
                "HLS provider URL resolution failed: error={} request_url={}",
                sanitize_sensitive_info(err.to_string().as_str()),
                hls_origin_log_value(input_source.url.as_str())
            );
            fallback()
        }
    }
}

pub(super) fn origin_manifest_fetch_error_from_request_error(err: &impl ToString) -> OriginManifestFetchError {
    let message = sanitize_sensitive_info(err.to_string().as_str()).to_string();
    let Some(status) = request_failed_status_from_message(&message) else {
        return OriginManifestFetchError::Request(message);
    };
    match classify_origin_manifest_status(status) {
        OriginManifestStatusClass::Success => OriginManifestFetchError::Request(message),
        OriginManifestStatusClass::Retryable => OriginManifestFetchError::RetryableStatus(status, None),
        OriginManifestStatusClass::PermanentFailure => OriginManifestFetchError::PermanentStatus(status),
        OriginManifestStatusClass::NonRetryableFailure => OriginManifestFetchError::NonRetryableStatus(status),
    }
}

fn origin_manifest_fetch_error_from_reqwest_error(err: &reqwest::Error) -> OriginManifestFetchError {
    if err.is_timeout() {
        OriginManifestFetchError::Timeout
    } else {
        origin_manifest_fetch_error_from_request_error(err)
    }
}

pub(super) fn origin_manifest_fetch_error_from_io_error(err: &io::Error) -> OriginManifestFetchError {
    if err.kind() == io::ErrorKind::TimedOut {
        OriginManifestFetchError::Timeout
    } else {
        origin_manifest_fetch_error_from_request_error(err)
    }
}

pub(super) fn request_failed_status_from_message(message: &str) -> Option<StatusCode> {
    let marker = "Request failed (";
    let status_start = message.find(marker)?.checked_add(marker.len())?;
    let status_text = message.get(status_start..)?.split(')').next()?;
    let status_code = status_text.split_whitespace().next()?.parse::<u16>().ok()?;
    StatusCode::from_u16(status_code).ok()
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HeaderMap) {
    crate::scrub_hls_origin_headers(headers, None);
}

pub fn retry_after_delay_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1_000))
}
