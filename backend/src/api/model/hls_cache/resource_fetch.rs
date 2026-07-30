use super::{
    append_hls_provider_session_headers,
    cache::{hls_cache_capacity_from_io, hls_cache_object_limit_from_io},
    force_identity_without_range, hls_object_body_deadline, hls_origin_log_value, log_hls_origin_content_coding,
    scrub_hls_origin_headers, HlsBoundAccountAcquireErrorKind, HlsLogIdentity, HlsOriginContentCodingObjectKind,
    HlsOriginContentCodingSource, SegmentFetchPolicy,
};
use crate::{
    processing::parser::hls::origin_manifest::ParsedByteRange,
    utils::content_coding::{
        apply_outbound_content_coding_policy, content_decoding_error_from_io, decode_response_to_identity,
        is_http_body_transport_error, ContentCodingDetection, ContentCodingError, ContentDecodingIoError,
        DecodedHttpResponse, OutboundContentCodingPolicy,
    },
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use futures::{future::BoxFuture, FutureExt};
use log::{debug, warn};
use reqwest::Client;
use shared::utils::sanitize_sensitive_info;
use std::{
    io,
    time::{Duration, Instant},
};
use url::Url;

const MAX_MANUAL_REDIRECTS: usize = 10;
const STORAGE_FULL_RAW_OS_ERRORS: &[i32] = &[28, 112, 122];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsResourceStatusClass {
    Success,
    Retryable,
    Permanent,
    NonRetryable,
}

fn classify_hls_resource_status(status: StatusCode) -> HlsResourceStatusClass {
    if status.is_success() {
        return HlsResourceStatusClass::Success;
    }
    if status.is_server_error()
        || matches!(
            status,
            StatusCode::PROXY_AUTHENTICATION_REQUIRED
                | StatusCode::REQUEST_TIMEOUT
                | StatusCode::TOO_EARLY
                | StatusCode::TOO_MANY_REQUESTS
        )
    {
        return HlsResourceStatusClass::Retryable;
    }
    if matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
            | StatusCode::GONE
    ) {
        return HlsResourceStatusClass::Permanent;
    }
    HlsResourceStatusClass::NonRetryable
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsResourceFetchKind {
    Segment,
    Map,
    Key,
    Part,
    Other,
}

impl HlsResourceFetchKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Segment => "Segment",
            Self::Map => "Map",
            Self::Key => "Key",
            Self::Part => "Part",
            Self::Other => "Resource",
        }
    }

    pub fn operation(self) -> &'static str {
        match self {
            Self::Segment => "segment fetch",
            Self::Map => "map fetch",
            Self::Key => "key fetch",
            Self::Part => "part fetch",
            Self::Other => "resource fetch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsResourceFetchSource {
    Normal,
    Transient,
}

impl HlsResourceFetchSource {
    pub fn as_log_value(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Transient => "transient",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsOriginByteRangeExpectation {
    FullObject,
    PartialContent,
    AnySuccess,
}

#[derive(Debug, Clone)]
pub struct HlsOriginResourceFetchTarget {
    pub kind: HlsResourceFetchKind,
    pub source: HlsResourceFetchSource,
    pub object_id: String,
    pub origin_url: String,
    pub headers: HeaderMap,
    pub byte_range_expectation: HlsOriginByteRangeExpectation,
}

impl HlsOriginResourceFetchTarget {
    fn log_context(&self) -> HlsResourceFetchLogContext<'_> {
        HlsResourceFetchLogContext {
            kind: self.kind,
            source: self.source,
            object_id: &self.object_id,
            origin_url: Some(&self.origin_url),
        }
    }
}

#[derive(Clone)]
pub struct HlsOriginResourceClients {
    pub client: Client,
    pub no_redirect_client: Client,
    pub use_manual_redirects: bool,
}

#[derive(Debug)]
pub enum HlsOriginResourceFetchError {
    PermanentStatus(StatusCode),
    RetryableStatus(StatusCode),
    NonRetryableStatus(StatusCode),
    Transport(String),
    Redirect,
    Timeout,
    Superseded,
    InvalidOriginUrl,
    InvalidByteRange,
    UnexpectedByteRangeStatus,
    ContentCoding(HlsContentCodingFailure),
    ContentDecoding(HlsContentDecodingFailure),
    CacheObjectLimit { limit: u64 },
    LocalCacheCapacity {
        required_session_bytes: u64,
        required_global_bytes: u64,
        projected_write_bytes: u64,
        revision: super::HlsCacheCapacityRevision,
    },
    CacheCommit(HlsCacheCommitFailure),
    ProviderUnavailable(HlsBoundAccountAcquireErrorKind),
}

/// Non-streaming HTTP content-coding failures that permanently reject one origin object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsContentCodingFailure {
    InvalidHeader,
    Unsupported,
    EncodedPartialContent,
}

impl HlsContentCodingFailure {
    fn label(&self) -> String {
        match self {
            Self::InvalidHeader => "ContentCodingError(InvalidHeader)".to_string(),
            Self::Unsupported => "ContentCodingError(Unsupported)".to_string(),
            Self::EncodedPartialContent => "ContentCodingError(EncodedPartialContent)".to_string(),
        }
    }
}

/// Fixed coding class for a retryable failure while streaming a decoded origin body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsContentDecodingFailure {
    coding: crate::utils::content_coding::ContentCoding,
}

impl HlsContentDecodingFailure {
    fn from_io_error(error: &ContentDecodingIoError) -> Self { Self { coding: error.coding } }

    fn label(&self) -> String { format!("ContentDecodingError(coding={})", self.coding.as_http_token()) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsCacheCommitFailure {
    io_kind: String,
    raw_os_error: Option<i32>,
    storage_full: bool,
    message: String,
}

impl HlsCacheCommitFailure {
    pub fn from_io_error(err: &io::Error) -> Self {
        let raw_os_error = err.raw_os_error();
        let io_kind = format!("{:?}", err.kind());
        Self {
            io_kind,
            raw_os_error,
            storage_full: matches!(err.kind(), io::ErrorKind::StorageFull)
                || raw_os_error.is_some_and(|code| STORAGE_FULL_RAW_OS_ERRORS.contains(&code)),
            message: sanitize_sensitive_info(&err.to_string()).to_string(),
        }
    }

    fn label(&self) -> String {
        let raw_os_error = self.raw_os_error.map_or_else(|| "none".to_string(), |code| code.to_string());
        format!(
            "CacheCommitError(io_kind={} raw_os_error={} storage_full={} message=\"{}\")",
            self.io_kind, raw_os_error, self.storage_full, self.message
        )
    }

    pub fn storage_full(&self) -> bool { self.storage_full }
}

impl HlsOriginResourceFetchError {
    pub fn cache_commit(err: &io::Error) -> Self {
        if let Some(capacity) = hls_cache_capacity_from_io(err) {
            return Self::LocalCacheCapacity {
                required_session_bytes: capacity.required_session_bytes(),
                required_global_bytes: capacity.required_global_bytes(),
                projected_write_bytes: capacity.staged_bytes(),
                revision: capacity.revision().clone(),
            };
        }
        Self::CacheCommit(HlsCacheCommitFailure::from_io_error(err))
    }

    /// Classifies an error produced while consuming a decoded body into the HLS retry contract.
    pub fn cache_body(err: &io::Error) -> Self {
        if let Some(error) = content_decoding_error_from_io(err) {
            return Self::ContentDecoding(HlsContentDecodingFailure::from_io_error(error));
        }
        if err.kind() == io::ErrorKind::TimedOut {
            return Self::Timeout;
        }
        if is_http_body_transport_error(err) {
            return Self::Transport(sanitize_sensitive_info(&err.to_string()).to_string());
        }
        if let Some(error) = hls_cache_object_limit_from_io(err) {
            return Self::CacheObjectLimit { limit: error.limit() };
        }
        Self::cache_commit(err)
    }

    fn from_body_read_error(err: &io::Error) -> Self {
        if let Some(error) = content_decoding_error_from_io(err) {
            return Self::ContentDecoding(HlsContentDecodingFailure::from_io_error(error));
        }
        if err.kind() == io::ErrorKind::TimedOut {
            return Self::Timeout;
        }
        Self::Transport(sanitize_sensitive_info(&err.to_string()).to_string())
    }

    fn from_content_coding_error(err: ContentCodingError) -> Self {
        match err {
            ContentCodingError::InvalidHeader => Self::ContentCoding(HlsContentCodingFailure::InvalidHeader),
            ContentCodingError::Unsupported(_) => Self::ContentCoding(HlsContentCodingFailure::Unsupported),
            ContentCodingError::EncodedPartialContent => {
                Self::ContentCoding(HlsContentCodingFailure::EncodedPartialContent)
            }
            ContentCodingError::PrefixRead(err) => Self::from_body_read_error(&err),
        }
    }

    pub fn retryable_failure(&self) -> bool {
        matches!(
            self,
            Self::RetryableStatus(_)
                | Self::Transport(_)
                | Self::Redirect
                | Self::Timeout
                | Self::ContentDecoding(_)
                | Self::LocalCacheCapacity { .. }
        ) || matches!(self, Self::CacheCommit(failure) if !failure.storage_full())
            || matches!(self, Self::ProviderUnavailable(kind) if kind.is_retryable_resource_failure())
    }

    pub fn permanent_status(&self) -> Option<StatusCode> {
        match self {
            Self::PermanentStatus(status) | Self::NonRetryableStatus(status) => Some(*status),
            _ => None,
        }
    }

    pub fn is_local_cache_capacity(&self) -> bool { matches!(self, Self::LocalCacheCapacity { .. }) }

    pub(crate) fn capacity_revision(&self) -> Option<&super::HlsCacheCapacityRevision> {
        match self {
            Self::LocalCacheCapacity { revision, .. } => Some(revision),
            _ => None,
        }
    }

    pub(crate) fn projected_write_bytes(&self) -> Option<u64> {
        match self {
            Self::LocalCacheCapacity { projected_write_bytes, .. } => Some(*projected_write_bytes),
            _ => None,
        }
    }

    pub fn object_failure_is_permanent(&self) -> bool {
        matches!(
            self,
            Self::PermanentStatus(_)
                | Self::NonRetryableStatus(_)
                | Self::InvalidOriginUrl
                | Self::InvalidByteRange
                | Self::UnexpectedByteRangeStatus
                | Self::ContentCoding(_)
                | Self::CacheObjectLimit { .. }
        )
    }

    fn log_status(&self) -> HlsResourceFetchLogStatus {
        match self {
            Self::PermanentStatus(status) | Self::RetryableStatus(status) | Self::NonRetryableStatus(status) => {
                HlsResourceFetchLogStatus::Http(*status)
            }
            Self::Transport(_) | Self::InvalidOriginUrl | Self::InvalidByteRange | Self::UnexpectedByteRangeStatus => {
                HlsResourceFetchLogStatus::TransportError
            }
            Self::Redirect => HlsResourceFetchLogStatus::RedirectError,
            Self::Timeout => HlsResourceFetchLogStatus::Timeout,
            Self::Superseded => HlsResourceFetchLogStatus::Superseded,
            Self::ContentCoding(failure) => HlsResourceFetchLogStatus::ContentCodingError(failure.clone()),
            Self::ContentDecoding(failure) => HlsResourceFetchLogStatus::ContentDecodingError(failure.clone()),
            Self::CacheObjectLimit { limit } => HlsResourceFetchLogStatus::CacheObjectLimit { limit: *limit },
            Self::LocalCacheCapacity { required_session_bytes, required_global_bytes, .. } => {
                HlsResourceFetchLogStatus::LocalCacheCapacity {
                    required_session_bytes: *required_session_bytes,
                    required_global_bytes: *required_global_bytes,
                }
            }
            Self::CacheCommit(failure) => HlsResourceFetchLogStatus::CacheCommitError(failure.clone()),
            Self::ProviderUnavailable(kind) => HlsResourceFetchLogStatus::ProviderUnavailable(*kind),
        }
    }

    fn aborts_without_retry(&self) -> bool {
        match self {
            Self::PermanentStatus(_)
            | Self::NonRetryableStatus(_)
            | Self::InvalidOriginUrl
            | Self::InvalidByteRange
            | Self::UnexpectedByteRangeStatus
            | Self::ContentCoding(_)
            | Self::CacheObjectLimit { .. }
            | Self::LocalCacheCapacity { .. }
            | Self::Superseded => true,
            Self::ProviderUnavailable(kind) => !kind.is_retryable_resource_failure(),
            Self::CacheCommit(failure) => failure.storage_full(),
            Self::RetryableStatus(_)
            | Self::Transport(_)
            | Self::Redirect
            | Self::Timeout
            | Self::ContentDecoding(_) => false,
        }
    }
}

type HlsOriginResourceResponsePrepareFuture<R> = BoxFuture<'static, Result<R, HlsOriginResourceFetchError>>;

struct HlsOriginResourceRetryRun<'a> {
    target: HlsOriginResourceFetchTarget,
    clients: HlsOriginResourceClients,
    policy: &'a SegmentFetchPolicy,
    identity: &'a HlsLogIdentity,
}

/// Absolute per-attempt deadline shared by decoder preparation and decoded cache-body consumption.
#[derive(Debug, Clone, Copy)]
pub struct HlsOriginResourceBodyDeadline {
    deadline: tokio::time::Instant,
    timeout: Duration,
}

impl HlsOriginResourceBodyDeadline {
    fn new(timeout: Duration) -> Self { Self { deadline: tokio::time::Instant::now() + timeout, timeout } }

    /// Returns the remaining body budget, saturating at zero once the deadline is exhausted.
    pub fn remaining(self) -> Duration { self.deadline.saturating_duration_since(tokio::time::Instant::now()) }

    /// Returns the absolute deadline used by decoded cache-body consumers.
    pub fn deadline(self) -> tokio::time::Instant { self.deadline }

    /// Returns the configured per-attempt body budget for timeout diagnostics.
    pub fn timeout(self) -> Duration { self.timeout }
}

/// Runs the shared HLS origin-resource retry policy with an optional per-attempt guard.
///
/// The prepare callback runs after the attempt log and before the HTTP request. If the HTTP
/// request or decoder setup fails before the commit callback takes ownership of the guard, the
/// cleanup callback is invoked by the runner. Once the commit callback receives a guard, it owns its
/// cleanup path. Direct client streams return the decoded reader from their commit callback; body
/// errors observed after that handoff deliberately remain outside this retry loop.
pub async fn run_hls_origin_resource_retry_loop_with_attempt_prepare<T, G, P, C, F>(
    target: HlsOriginResourceFetchTarget,
    clients: HlsOriginResourceClients,
    policy: &SegmentFetchPolicy,
    identity: &HlsLogIdentity,
    prepare_attempt: P,
    cleanup_attempt: C,
    commit: F,
) -> Result<T, HlsOriginResourceFetchError>
where
    T: Send + 'static,
    G: Send + 'static,
    P: FnMut(HlsResourceFetchAttempt) -> BoxFuture<'static, Result<G, HlsOriginResourceFetchError>>,
    C: FnMut(G) -> BoxFuture<'static, ()>,
    F: FnMut(
        DecodedHttpResponse,
        HlsResourceFetchAttempt,
        HlsOriginResourceBodyDeadline,
        G,
    ) -> BoxFuture<'static, Result<T, HlsOriginResourceFetchError>>,
{
    let coding_object_kind = match target.kind {
        HlsResourceFetchKind::Segment => HlsOriginContentCodingObjectKind::Segment,
        HlsResourceFetchKind::Map => HlsOriginContentCodingObjectKind::Map,
        HlsResourceFetchKind::Key => HlsOriginContentCodingObjectKind::Key,
        HlsResourceFetchKind::Part => HlsOriginContentCodingObjectKind::Part,
        HlsResourceFetchKind::Other => HlsOriginContentCodingObjectKind::Other,
    };
    let range_requested = target.headers.contains_key(header::RANGE);
    run_hls_origin_resource_raw_retry_core(
        HlsOriginResourceRetryRun { target, clients, policy, identity },
        prepare_attempt,
        cleanup_attempt,
        |response, deadline| {
            async move {
                let remaining = deadline.remaining();
                if remaining.is_zero() {
                    return Err(HlsOriginResourceFetchError::Timeout);
                }
                match tokio::time::timeout_at(
                    deadline.deadline(),
                    decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly),
                )
                .await
                {
                    Ok(result) => {
                        let decoded = result.map_err(HlsOriginResourceFetchError::from_content_coding_error)?;
                        if let Some(observation) = decoded.content_coding_observation() {
                            log_hls_origin_content_coding(
                                observation,
                                coding_object_kind,
                                range_requested,
                                HlsOriginContentCodingSource::Shared,
                            );
                        }
                        Ok(decoded)
                    }
                    Err(_) => Err(HlsOriginResourceFetchError::Timeout),
                }
            }
            .boxed()
        },
        commit,
    )
    .await
}

/// Owns the single logical retry budget shared by decoded cache fetches and direct streams.
async fn run_hls_origin_resource_raw_retry_core<T, G, R, P, C, D, F>(
    run: HlsOriginResourceRetryRun<'_>,
    mut prepare_attempt: P,
    mut cleanup_attempt: C,
    mut prepare_response: D,
    mut commit: F,
) -> Result<T, HlsOriginResourceFetchError>
where
    T: Send + 'static,
    G: Send + 'static,
    R: Send + 'static,
    P: FnMut(HlsResourceFetchAttempt) -> BoxFuture<'static, Result<G, HlsOriginResourceFetchError>>,
    C: FnMut(G) -> BoxFuture<'static, ()>,
    D: FnMut(reqwest::Response, HlsOriginResourceBodyDeadline) -> HlsOriginResourceResponsePrepareFuture<R>,
    F: FnMut(
        R,
        HlsResourceFetchAttempt,
        HlsOriginResourceBodyDeadline,
        G,
    ) -> BoxFuture<'static, Result<T, HlsOriginResourceFetchError>>,
{
    let HlsOriginResourceRetryRun { target, clients, policy, identity } = run;
    let attempts = policy.retry_delays_ms.len();
    for attempt_index in 0..attempts {
        let attempt = HlsResourceFetchAttempt { attempt_index, attempts };
        let delay_ms = policy.retry_delay_ms(attempt_index);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        log_hls_resource_attempt_started(identity, target.log_context(), attempt);
        let attempt_started_at = Instant::now();
        let result = match prepare_attempt(attempt).await {
            Ok(guard) => match fetch_hls_origin_resource_response(&target, &clients).await {
                Ok(response) => {
                    let deadline =
                        HlsOriginResourceBodyDeadline::new(hls_object_body_deadline(policy.origin_segment_timeout_ms));
                    match prepare_response(response, deadline).await {
                        Ok(response) => commit(response, attempt, deadline, guard).await,
                        Err(err) => {
                            cleanup_attempt(guard).await;
                            Err(err)
                        }
                    }
                }
                Err(err) => {
                    cleanup_attempt(guard).await;
                    Err(err)
                }
            },
            Err(err) => Err(err),
        };

        match result {
            Ok(output) => {
                log_hls_resource_attempt_succeeded(identity, target.log_context(), attempt_started_at.elapsed());
                return Ok(output);
            }
            Err(err) if err.aborts_without_retry() || attempt_index + 1 == attempts => {
                if matches!(err, HlsOriginResourceFetchError::Timeout) {
                    log_hls_resource_timeout(
                        identity,
                        target.log_context(),
                        attempt,
                        hls_object_body_deadline(policy.origin_segment_timeout_ms).as_millis(),
                    );
                }
                log_hls_resource_fetch_failed(identity, target.log_context(), attempt, err.log_status());
                return Err(err);
            }
            Err(err) => {
                if matches!(err, HlsOriginResourceFetchError::Timeout) {
                    log_hls_resource_timeout(
                        identity,
                        target.log_context(),
                        attempt,
                        hls_object_body_deadline(policy.origin_segment_timeout_ms).as_millis(),
                    );
                }
                log_hls_resource_retry_scheduled(
                    identity,
                    target.log_context(),
                    attempt,
                    err.log_status(),
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default(),
                );
            }
        }
    }

    Err(HlsOriginResourceFetchError::Timeout)
}

async fn fetch_hls_origin_resource_response(
    target: &HlsOriginResourceFetchTarget,
    clients: &HlsOriginResourceClients,
) -> Result<reqwest::Response, HlsOriginResourceFetchError> {
    let url = Url::parse(&target.origin_url).map_err(|_| HlsOriginResourceFetchError::InvalidOriginUrl)?;
    let response = if clients.use_manual_redirects {
        fetch_hls_origin_resource_with_manual_redirects(&url, target.headers.clone(), &clients.no_redirect_client)
            .await?
    } else {
        let mut headers = target.headers.clone();
        apply_outbound_content_coding_policy(&mut headers, OutboundContentCodingPolicy::Identity);
        clients.client.get(url).headers(headers).send().await.map_err(|err| {
            HlsOriginResourceFetchError::Transport(sanitize_sensitive_info(err.to_string().as_str()).to_string())
        })?
    };
    classify_hls_origin_resource_response(response, target.byte_range_expectation)
}

async fn fetch_hls_origin_resource_with_manual_redirects(
    entry_url: &Url,
    headers: HeaderMap,
    client: &Client,
) -> Result<reqwest::Response, HlsOriginResourceFetchError> {
    let mut current_url = entry_url.clone();
    let mut current_headers = headers;
    let mut remaining_redirects = MAX_MANUAL_REDIRECTS;

    loop {
        let mut request_headers = current_headers.clone();
        apply_outbound_content_coding_policy(&mut request_headers, OutboundContentCodingPolicy::Identity);
        let response = client.get(current_url.clone()).headers(request_headers).send().await.map_err(|err| {
            HlsOriginResourceFetchError::Transport(sanitize_sensitive_info(err.to_string().as_str()).to_string())
        })?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if remaining_redirects == 0 {
            return Err(HlsOriginResourceFetchError::Redirect);
        }
        let response_url = response.url().clone();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(HlsOriginResourceFetchError::Redirect)?;
        let next_url = response_url
            .join(location)
            .or_else(|_| Url::parse(location))
            .map_err(|_| HlsOriginResourceFetchError::Redirect)?;
        if !same_origin(&response_url, &next_url) {
            strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
        }
        current_url = next_url;
        remaining_redirects = remaining_redirects.saturating_sub(1);
    }
}

fn classify_hls_origin_resource_response(
    response: reqwest::Response,
    byte_range_expectation: HlsOriginByteRangeExpectation,
) -> Result<reqwest::Response, HlsOriginResourceFetchError> {
    let status = response.status();
    match classify_hls_resource_status(status) {
        HlsResourceStatusClass::Success => {
            match byte_range_expectation {
                HlsOriginByteRangeExpectation::FullObject if status == StatusCode::PARTIAL_CONTENT => {
                    return Err(HlsOriginResourceFetchError::UnexpectedByteRangeStatus);
                }
                HlsOriginByteRangeExpectation::PartialContent if status != StatusCode::PARTIAL_CONTENT => {
                    return Err(HlsOriginResourceFetchError::UnexpectedByteRangeStatus);
                }
                HlsOriginByteRangeExpectation::FullObject
                | HlsOriginByteRangeExpectation::PartialContent
                | HlsOriginByteRangeExpectation::AnySuccess => {}
            }
            Ok(response)
        }
        HlsResourceStatusClass::Retryable => Err(HlsOriginResourceFetchError::RetryableStatus(status)),
        HlsResourceStatusClass::Permanent => Err(HlsOriginResourceFetchError::PermanentStatus(status)),
        HlsResourceStatusClass::NonRetryable => Err(HlsOriginResourceFetchError::NonRetryableStatus(status)),
    }
}

pub fn build_hls_origin_resource_headers(
    source_headers: &HeaderMap,
    provider_session_headers: &HeaderMap,
    byte_range: Option<ParsedByteRange>,
) -> Result<HeaderMap, HlsOriginResourceFetchError> {
    build_hls_origin_resource_headers_with_range(
        source_headers,
        provider_session_headers,
        byte_range.map_or(HlsOriginRangeHeader::None, HlsOriginRangeHeader::Parsed),
    )
}

pub fn build_hls_origin_resource_headers_with_client_range(
    source_headers: &HeaderMap,
    provider_session_headers: &HeaderMap,
    client_range: Option<HeaderValue>,
) -> Result<HeaderMap, HlsOriginResourceFetchError> {
    build_hls_origin_resource_headers_with_range(
        source_headers,
        provider_session_headers,
        client_range.map_or(HlsOriginRangeHeader::None, HlsOriginRangeHeader::Forwarded),
    )
}

/// The sole controlled `Range` value inserted after origin-header scrubbing.
enum HlsOriginRangeHeader {
    None,
    Parsed(ParsedByteRange),
    Forwarded(HeaderValue),
}

fn build_hls_origin_resource_headers_with_range(
    source_headers: &HeaderMap,
    provider_session_headers: &HeaderMap,
    range: HlsOriginRangeHeader,
) -> Result<HeaderMap, HlsOriginResourceFetchError> {
    let mut headers = source_headers.clone();
    scrub_hls_origin_headers(&mut headers, None);
    append_hls_provider_session_headers(&mut headers, provider_session_headers);
    force_identity_without_range(&mut headers);
    match range {
        HlsOriginRangeHeader::None => {}
        HlsOriginRangeHeader::Parsed(byte_range) => {
            let end = byte_range
                .offset
                .checked_add(byte_range.length)
                .and_then(|end_exclusive| end_exclusive.checked_sub(1))
                .ok_or(HlsOriginResourceFetchError::InvalidByteRange)?;
            let range_value = format!("bytes={}-{}", byte_range.offset, end);
            let range_value =
                HeaderValue::from_str(&range_value).map_err(|_| HlsOriginResourceFetchError::InvalidByteRange)?;
            headers.insert(header::RANGE, range_value);
        }
        HlsOriginRangeHeader::Forwarded(range) => {
            headers.insert(header::RANGE, range);
        }
    }
    Ok(headers)
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HeaderMap) {
    scrub_hls_origin_headers(headers, None);
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HlsResourceFetchLogContext<'a> {
    pub(super) kind: HlsResourceFetchKind,
    pub(super) source: HlsResourceFetchSource,
    pub(super) object_id: &'a str,
    pub(super) origin_url: Option<&'a str>,
}

#[derive(Debug, Clone)]
enum HlsResourceFetchLogStatus {
    Http(StatusCode),
    Timeout,
    Superseded,
    TransportError,
    RedirectError,
    ContentCodingError(HlsContentCodingFailure),
    ContentDecodingError(HlsContentDecodingFailure),
    CacheObjectLimit { limit: u64 },
    LocalCacheCapacity { required_session_bytes: u64, required_global_bytes: u64 },
    CacheCommitError(HlsCacheCommitFailure),
    ProviderUnavailable(HlsBoundAccountAcquireErrorKind),
}

impl HlsResourceFetchLogStatus {
    fn label(self) -> String {
        match self {
            Self::Http(status) => {
                let reason = status.canonical_reason().unwrap_or("Unknown");
                format!("{} {reason}", status.as_u16())
            }
            Self::Timeout => "Timeout".to_string(),
            Self::Superseded => "Superseded".to_string(),
            Self::TransportError => "TransportError".to_string(),
            Self::RedirectError => "RedirectError".to_string(),
            Self::ContentCodingError(failure) => failure.label(),
            Self::ContentDecodingError(failure) => failure.label(),
            Self::CacheObjectLimit { limit } => format!("CacheObjectLimit(limit={limit})"),
            Self::LocalCacheCapacity { required_session_bytes, required_global_bytes } => format!(
                "LocalCacheCapacity(required_session_bytes={required_session_bytes} required_global_bytes={required_global_bytes})"
            ),
            Self::CacheCommitError(failure) => failure.label(),
            Self::ProviderUnavailable(kind) => format!("ProviderUnavailable({})", kind.as_log_label()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HlsResourceFetchAttempt {
    pub attempt_index: usize,
    pub attempts: usize,
}

impl HlsResourceFetchAttempt {
    /// Returns the one-based attempt number used in human-readable diagnostics.
    const fn display_number(self) -> usize { self.attempt_index + 1 }
}

#[cfg(test)]
thread_local! {
    static BODY_FAILURE_LOG_ATTEMPTS: std::cell::RefCell<Vec<(usize, usize)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[cfg(test)]
pub(super) fn take_body_failure_log_attempts() -> Vec<(usize, usize)> {
    BODY_FAILURE_LOG_ATTEMPTS.with(|attempts| std::mem::take(&mut *attempts.borrow_mut()))
}

fn log_hls_resource_attempt_started(
    identity: &HlsLogIdentity,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
) {
    if let Some(origin_url) = context.origin_url {
        debug!(
            "{} '{}' attempting URL attempt {} of {}: session={} proxy_session={} source={} resource_url={}",
            context.kind.label(),
            context.object_id,
            attempt.display_number(),
            attempt.attempts,
            identity.session(),
            identity.proxy_session(),
            context.source.as_log_value(),
            hls_origin_log_value(origin_url)
        );
    } else {
        debug!(
            "{} '{}' attempting URL attempt {} of {}: session={} proxy_session={} source={}",
            context.kind.label(),
            context.object_id,
            attempt.display_number(),
            attempt.attempts,
            identity.session(),
            identity.proxy_session(),
            context.source.as_log_value()
        );
    }
}

fn log_hls_resource_attempt_succeeded(
    identity: &HlsLogIdentity,
    context: HlsResourceFetchLogContext<'_>,
    elapsed: Duration,
) {
    debug!(
        "{} '{}' success: session={} proxy_session={} source={} {} took {:.3}s",
        context.kind.label(),
        context.object_id,
        identity.session(),
        identity.proxy_session(),
        context.source.as_log_value(),
        context.kind.operation(),
        elapsed.as_secs_f64()
    );
}

fn log_hls_resource_retry_scheduled(
    identity: &HlsLogIdentity,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    status: HlsResourceFetchLogStatus,
    next_delay_ms: u64,
) {
    warn!(
        "{} '{}' retry scheduled: session={} proxy_session={} source={} status {} attempt {} of {} next_delay_ms={}",
        context.kind.label(),
        context.object_id,
        identity.session(),
        identity.proxy_session(),
        context.source.as_log_value(),
        status.label(),
        attempt.display_number(),
        attempt.attempts,
        next_delay_ms
    );
}

fn log_hls_resource_fetch_failed(
    identity: &HlsLogIdentity,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    status: HlsResourceFetchLogStatus,
) {
    warn!(
        "{} '{}' failed: session={} proxy_session={} source={} status {} attempt {} of {}",
        context.kind.label(),
        context.object_id,
        identity.session(),
        identity.proxy_session(),
        context.source.as_log_value(),
        status.label(),
        attempt.display_number(),
        attempt.attempts
    );
}

fn log_hls_resource_timeout(
    identity: &HlsLogIdentity,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    deadline_ms: u128,
) {
    warn!(
        "HLS origin object fetch timed out: session={} proxy_session={} source={} kind={} object={} attempt={} of {} deadline_ms={}",
        identity.session(),
        identity.proxy_session(),
        context.source.as_log_value(),
        context.kind.label(),
        context.object_id,
        attempt.display_number(),
        attempt.attempts,
        deadline_ms
    );
}

/// Logs one sanitized terminal diagnostic for an origin-body failure after response handoff.
pub(super) fn log_hls_resource_body_failure(
    identity: &HlsLogIdentity,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    error: &io::Error,
    deadline_ms: u128,
) {
    #[cfg(test)]
    BODY_FAILURE_LOG_ATTEMPTS.with(|attempts| attempts.borrow_mut().push((attempt.attempt_index, attempt.attempts)));

    let failure = HlsOriginResourceFetchError::from_body_read_error(error);
    if matches!(&failure, HlsOriginResourceFetchError::Timeout) {
        log_hls_resource_timeout(identity, context, attempt, deadline_ms);
    } else {
        log_hls_resource_fetch_failed(identity, context, attempt, failure.log_status());
    }
}

pub fn retry_after_secs_from_ms(retry_after_ms: u64) -> u64 {
    retry_after_ms.saturating_add(999).saturating_div(1_000).max(1)
}

#[cfg(test)]
mod tests {
    use super::{
        build_hls_origin_resource_headers, build_hls_origin_resource_headers_with_client_range,
        fetch_hls_origin_resource_response, run_hls_origin_resource_retry_loop_with_attempt_prepare,
        HlsCacheCommitFailure, HlsContentCodingFailure, HlsOriginByteRangeExpectation, HlsOriginResourceBodyDeadline,
        HlsOriginResourceClients, HlsOriginResourceFetchError, HlsOriginResourceFetchTarget, HlsResourceFetchAttempt,
        HlsResourceFetchKind, HlsResourceFetchLogStatus, HlsResourceFetchSource,
    };
    use crate::{
        api::model::{HlsBoundAccountAcquireErrorKind, HlsLogIdentity, SegmentFetchPolicy},
        processing::parser::hls::origin_manifest::ParsedByteRange,
        utils::content_coding::{ContentCoding, ContentCodingError, ContentDecodingIoError},
    };
    use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
    use futures::FutureExt;
    use reqwest::{redirect::Policy, Client};
    use std::{
        io,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(request).expect("request should be HTTP text")
    }

    fn test_log_identity() -> HlsLogIdentity { HlsLogIdentity::for_test("content-session", "proxy-session") }

    fn fetch_target(origin_url: String) -> HlsOriginResourceFetchTarget {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        HlsOriginResourceFetchTarget {
            kind: HlsResourceFetchKind::Segment,
            source: HlsResourceFetchSource::Normal,
            object_id: "segment".to_string(),
            origin_url,
            headers,
            byte_range_expectation: HlsOriginByteRangeExpectation::FullObject,
        }
    }

    fn fetch_clients(use_manual_redirects: bool) -> HlsOriginResourceClients {
        HlsOriginResourceClients {
            client: Client::builder().build().expect("client"),
            no_redirect_client: Client::builder().redirect(Policy::none()).build().expect("no redirect client"),
            use_manual_redirects,
        }
    }

    #[test]
    fn formats_http_fetch_log_status_with_reason() {
        assert_eq!(
            HlsResourceFetchLogStatus::Http(StatusCode::INTERNAL_SERVER_ERROR).label(),
            "500 Internal Server Error"
        );
    }

    #[test]
    fn formats_non_http_fetch_log_status() {
        assert_eq!(HlsResourceFetchLogStatus::Timeout.label(), "Timeout");
        assert_eq!(HlsResourceFetchLogStatus::TransportError.label(), "TransportError");
        assert_eq!(HlsResourceFetchLogStatus::RedirectError.label(), "RedirectError");
    }

    #[test]
    fn resource_fetch_attempt_display_number_is_one_based_without_changing_raw_index() {
        let first = HlsResourceFetchAttempt { attempt_index: 0, attempts: 3 };
        let last = HlsResourceFetchAttempt { attempt_index: 2, attempts: 3 };

        assert_eq!(first.display_number(), 1);
        assert_eq!(last.display_number(), 3);
        assert_eq!(first.attempt_index, 0);
        assert_eq!(last.attempt_index, 2);
        assert_eq!(first.attempts, 3);
        assert_eq!(last.attempts, 3);
    }

    #[test]
    fn exhausted_body_deadline_saturates_at_zero() {
        let deadline = HlsOriginResourceBodyDeadline::new(Duration::ZERO);

        assert_eq!(deadline.remaining(), Duration::ZERO);
    }

    #[test]
    fn unsupported_content_coding_is_a_permanent_object_failure() {
        let error = HlsOriginResourceFetchError::from_content_coding_error(ContentCodingError::Unsupported(
            "compress".to_string(),
        ));

        assert!(error.object_failure_is_permanent());
        assert!(!error.retryable_failure());
        assert!(error.aborts_without_retry());
        assert_eq!(error.log_status().label(), "ContentCodingError(Unsupported)");
    }

    #[test]
    fn streaming_decoder_failure_is_retryable_and_not_a_cache_commit_failure() {
        let error = io::Error::new(io::ErrorKind::InvalidData, ContentDecodingIoError { coding: ContentCoding::Gzip });

        let error = HlsOriginResourceFetchError::cache_body(&error);

        assert!(matches!(&error, HlsOriginResourceFetchError::ContentDecoding(_)));
        assert!(error.retryable_failure());
        assert!(!error.object_failure_is_permanent());
    }

    #[test]
    fn direct_body_failure_classification_distinguishes_decoder_transport_and_timeout() {
        let decoder_error =
            io::Error::new(io::ErrorKind::InvalidData, ContentDecodingIoError { coding: ContentCoding::Gzip });
        let decoder_failure = HlsOriginResourceFetchError::from_body_read_error(&decoder_error);
        assert!(matches!(&decoder_failure, HlsOriginResourceFetchError::ContentDecoding(_)));
        assert_eq!(decoder_failure.log_status().label(), "ContentDecodingError(coding=gzip)");

        let transport_error = io::Error::new(io::ErrorKind::ConnectionReset, "origin body reset");
        let transport_failure = HlsOriginResourceFetchError::from_body_read_error(&transport_error);
        assert!(matches!(&transport_failure, HlsOriginResourceFetchError::Transport(_)));
        assert_eq!(transport_failure.log_status().label(), "TransportError");

        let timeout_error = io::Error::new(io::ErrorKind::TimedOut, "origin body timed out");
        let timeout_failure = HlsOriginResourceFetchError::from_body_read_error(&timeout_error);
        assert!(matches!(&timeout_failure, HlsOriginResourceFetchError::Timeout));
        assert_eq!(timeout_failure.log_status().label(), "Timeout");
    }

    #[tokio::test]
    async fn final_automatic_origin_request_overrides_accept_encoding_with_identity() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind local server");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let request = read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write response");
            request
        });

        let target = fetch_target(format!("http://{address}/segment.ts"));
        let response =
            fetch_hls_origin_resource_response(&target, &fetch_clients(false)).await.expect("origin response");
        assert_eq!(response.status(), StatusCode::OK);

        let request = server.await.expect("server task").to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity\r\n"));
        assert!(!request.contains("accept-encoding: gzip\r\n"));
    }

    #[tokio::test]
    async fn every_manual_redirect_request_enforces_accept_encoding_identity() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind local server");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in [
                "HTTP/1.1 302 Found\r\nLocation: /next.ts\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                requests.push(read_request(&mut stream).await);
                stream.write_all(response.as_bytes()).await.expect("write response");
            }
            requests
        });

        let target = fetch_target(format!("http://{address}/segment.ts"));
        let response = fetch_hls_origin_resource_response(&target, &fetch_clients(true))
            .await
            .expect("redirected origin response");
        assert_eq!(response.status(), StatusCode::OK);

        let requests = server.await.expect("server task");
        assert_eq!(requests.len(), 2);
        for request in requests {
            let request = request.to_ascii_lowercase();
            assert!(request.contains("accept-encoding: identity\r\n"));
            assert!(!request.contains("accept-encoding: gzip\r\n"));
        }
    }

    #[tokio::test]
    async fn automatic_cross_origin_redirect_preserves_identity_and_range_but_strips_provider_cookie() {
        let destination_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind redirect destination");
        let destination_address = destination_listener.local_addr().expect("redirect destination address");
        let destination = tokio::spawn(async move {
            let (mut stream, _) = destination_listener.accept().await.expect("accept redirected request");
            let request = read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write redirected response");
            request
        });

        let source_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind redirect source");
        let source_address = source_listener.local_addr().expect("redirect source address");
        let source = tokio::spawn(async move {
            let (mut stream, _) = source_listener.accept().await.expect("accept initial request");
            let request = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{destination_address}/final.ts\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.expect("write redirect response");
            request
        });

        let mut provider_headers = HeaderMap::new();
        provider_headers.insert(header::COOKIE, HeaderValue::from_static("sid=provider"));
        let headers = build_hls_origin_resource_headers_with_client_range(
            &HeaderMap::new(),
            &provider_headers,
            Some(HeaderValue::from_static("bytes=2-5")),
        )
        .expect("forwarded range headers");
        let mut target = fetch_target(format!("http://{source_address}/segment.ts"));
        target.headers = headers;

        let response = fetch_hls_origin_resource_response(&target, &fetch_clients(false))
            .await
            .expect("automatic redirect response");
        assert_eq!(response.status(), StatusCode::OK);

        let initial_request = source.await.expect("initial request task").to_ascii_lowercase();
        let redirected_request = destination.await.expect("redirected request task").to_ascii_lowercase();
        for request in [&initial_request, &redirected_request] {
            assert!(request.contains("accept-encoding: identity\r\n"));
            assert!(request.contains("range: bytes=2-5\r\n"));
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("proxy-authorization:"));
        }
        assert!(initial_request.contains("cookie: sid=provider\r\n"));
        assert!(!redirected_request.contains("cookie:"));
    }

    #[tokio::test]
    async fn full_object_partial_response_is_rejected_before_decoder_and_cleans_attempt_guard() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind local server");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let request = read_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/10\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad",
                )
                .await
                .expect("write response");
            request
        });
        let target = fetch_target(format!("http://{address}/segment.ts"));
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let cleanup_count = Arc::new(AtomicUsize::new(0));
        let commit_count = Arc::new(AtomicUsize::new(0));
        let cleanup_count_for_callback = Arc::clone(&cleanup_count);
        let commit_count_for_callback = Arc::clone(&commit_count);
        let identity = test_log_identity();

        let result = run_hls_origin_resource_retry_loop_with_attempt_prepare(
            target,
            fetch_clients(false),
            &policy,
            &identity,
            |_| async { Ok(()) }.boxed(),
            move |()| {
                let cleanup_count = Arc::clone(&cleanup_count_for_callback);
                async move {
                    cleanup_count.fetch_add(1, Ordering::Relaxed);
                }
                .boxed()
            },
            move |_response, _attempt, _deadline, ()| {
                let commit_count = Arc::clone(&commit_count_for_callback);
                async move {
                    commit_count.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                .boxed()
            },
        )
        .await;

        assert!(matches!(result, Err(HlsOriginResourceFetchError::UnexpectedByteRangeStatus)));
        assert_eq!(cleanup_count.load(Ordering::Relaxed), 1);
        assert_eq!(commit_count.load(Ordering::Relaxed), 0);
        let request = server.await.expect("server task").to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity\r\n"));
    }

    #[tokio::test]
    async fn range_accepted_encoded_partial_response_is_rejected_by_content_coding_policy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind local server");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let request = read_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Encoding: gzip\r\nContent-Range: bytes 0-2/10\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad",
                )
                .await
                .expect("write response");
            request
        });
        let mut target = fetch_target(format!("http://{address}/segment.ts"));
        target.byte_range_expectation = HlsOriginByteRangeExpectation::PartialContent;
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let cleanup_count = Arc::new(AtomicUsize::new(0));
        let commit_count = Arc::new(AtomicUsize::new(0));
        let cleanup_count_for_callback = Arc::clone(&cleanup_count);
        let commit_count_for_callback = Arc::clone(&commit_count);
        let identity = test_log_identity();

        let result = run_hls_origin_resource_retry_loop_with_attempt_prepare(
            target,
            fetch_clients(false),
            &policy,
            &identity,
            |_| async { Ok(()) }.boxed(),
            move |()| {
                let cleanup_count = Arc::clone(&cleanup_count_for_callback);
                async move {
                    cleanup_count.fetch_add(1, Ordering::Relaxed);
                }
                .boxed()
            },
            move |_response, _attempt, _deadline, ()| {
                let commit_count = Arc::clone(&commit_count_for_callback);
                async move {
                    commit_count.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                .boxed()
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(HlsOriginResourceFetchError::ContentCoding(HlsContentCodingFailure::EncodedPartialContent))
        ));
        assert_eq!(cleanup_count.load(Ordering::Relaxed), 1);
        assert_eq!(commit_count.load(Ordering::Relaxed), 0);
        let request = server.await.expect("server task").to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity\r\n"));
    }

    #[test]
    fn cache_commit_failure_marks_enospc_as_storage_full() {
        let failure = HlsCacheCommitFailure::from_io_error(&io::Error::from_raw_os_error(28));

        assert!(failure.storage_full());
        assert!(HlsResourceFetchLogStatus::CacheCommitError(failure).label().contains("storage_full=true"));
    }

    #[test]
    fn storage_full_cache_commit_failure_is_not_retryable() {
        let err = HlsOriginResourceFetchError::cache_commit(&io::Error::from_raw_os_error(28));

        assert!(!err.retryable_failure());
        assert!(err.aborts_without_retry());
    }

    #[test]
    fn local_cache_capacity_aborts_origin_retries_but_remains_object_retryable() {
        let err = HlsOriginResourceFetchError::LocalCacheCapacity {
            required_session_bytes: 12,
            required_global_bytes: 0,
            projected_write_bytes: 12,
            revision: super::super::HlsCacheCapacityRevision::for_test(),
        };

        assert!(err.retryable_failure());
        assert!(err.aborts_without_retry());
        assert!(!err.object_failure_is_permanent());
        assert!(err.is_local_cache_capacity());
    }

    #[test]
    fn superseded_fetch_aborts_retry_without_marking_the_object_permanent() {
        let err = HlsOriginResourceFetchError::Superseded;

        assert!(!err.retryable_failure());
        assert!(err.aborts_without_retry());
        assert!(!err.object_failure_is_permanent());
        assert_eq!(err.log_status().label(), "Superseded");
    }

    #[tokio::test]
    async fn local_cache_capacity_aborts_after_one_origin_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind local server");
        let address = listener.local_addr().expect("local address");
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_request_count = Arc::clone(&request_count);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                read_request(&mut stream).await;
                server_request_count.fetch_add(1, Ordering::Relaxed);
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("write response");
            }
        });
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let identity = test_log_identity();

        let result = run_hls_origin_resource_retry_loop_with_attempt_prepare(
            fetch_target(format!("http://{address}/segment.ts")),
            fetch_clients(false),
            &policy,
            &identity,
            |_| async { Ok(()) }.boxed(),
            |()| async {}.boxed(),
            |_response, _attempt, _deadline, ()| {
                async {
                    Err::<(), _>(HlsOriginResourceFetchError::LocalCacheCapacity {
                        required_session_bytes: 12,
                        required_global_bytes: 0,
                        projected_write_bytes: 12,
                        revision: super::super::HlsCacheCapacityRevision::for_test(),
                    })
                }
                .boxed()
            },
        )
        .await;

        server.abort();
        assert!(matches!(result, Err(HlsOriginResourceFetchError::LocalCacheCapacity { .. })));
        assert_eq!(request_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn storage_full_cache_commit_aborts_after_one_origin_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind local server");
        let address = listener.local_addr().expect("local address");
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_request_count = Arc::clone(&request_count);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                read_request(&mut stream).await;
                server_request_count.fetch_add(1, Ordering::Relaxed);
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("write response");
            }
        });
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let identity = test_log_identity();

        let result = run_hls_origin_resource_retry_loop_with_attempt_prepare(
            fetch_target(format!("http://{address}/segment.ts")),
            fetch_clients(false),
            &policy,
            &identity,
            |_| async { Ok(()) }.boxed(),
            |()| async {}.boxed(),
            |_response, _attempt, _deadline, ()| {
                async { Err::<(), _>(HlsOriginResourceFetchError::cache_commit(&io::Error::from_raw_os_error(28))) }
                    .boxed()
            },
        )
        .await;

        server.abort();
        assert!(matches!(result, Err(HlsOriginResourceFetchError::CacheCommit(failure)) if failure.storage_full()));
        assert_eq!(request_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn provider_wait_timeout_is_retryable_resource_failure() {
        let err = HlsOriginResourceFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::WaitTimedOut);

        assert!(err.retryable_failure());
        assert!(!err.aborts_without_retry());
    }

    #[test]
    fn missing_provider_account_aborts_resource_retry() {
        let err = HlsOriginResourceFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Missing);

        assert!(!err.retryable_failure());
        assert!(err.aborts_without_retry());
    }

    #[test]
    fn origin_resource_headers_drop_client_cookie_and_append_provider_cookie() {
        let mut client_headers = HeaderMap::new();
        client_headers.insert(header::COOKIE, HeaderValue::from_static("client=secret"));
        client_headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));

        let mut provider_headers = HeaderMap::new();
        provider_headers.insert(header::COOKIE, HeaderValue::from_static("sid=provider"));

        let headers = build_hls_origin_resource_headers_with_client_range(&client_headers, &provider_headers, None)
            .expect("origin headers");

        assert_eq!(headers.get(header::COOKIE).expect("cookie"), "sid=provider");
        assert!(!headers.contains_key(header::RANGE));
        assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("encoding"), "identity");
    }

    #[test]
    fn origin_resource_header_wrappers_share_scrubbing_identity_and_range_policy() {
        let mut client_headers = HeaderMap::new();
        client_headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer client"));
        client_headers.insert(header::COOKIE, HeaderValue::from_static("client=secret"));
        client_headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
        client_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        client_headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));
        let mut provider_headers = HeaderMap::new();
        provider_headers.insert(header::COOKIE, HeaderValue::from_static("sid=provider"));

        let without_range =
            build_hls_origin_resource_headers(&client_headers, &provider_headers, None).expect("full object headers");
        let parsed = build_hls_origin_resource_headers(
            &client_headers,
            &provider_headers,
            Some(ParsedByteRange { offset: 2, length: 4 }),
        )
        .expect("parsed range headers");
        let forwarded = build_hls_origin_resource_headers_with_client_range(
            &client_headers,
            &provider_headers,
            Some(HeaderValue::from_static("bytes=2-5")),
        )
        .expect("forwarded range headers");

        assert_eq!(parsed, forwarded);
        for headers in [&without_range, &parsed, &forwarded] {
            assert!(!headers.contains_key(header::AUTHORIZATION));
            assert_eq!(headers.get(header::COOKIE).expect("provider cookie"), "sid=provider");
            assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("identity"), "identity");
            assert_eq!(headers.get(header::ACCEPT_LANGUAGE).expect("safe input header"), "de");
        }
        assert!(!without_range.contains_key(header::RANGE));
        assert_eq!(parsed.get(header::RANGE).expect("parsed range"), "bytes=2-5");
    }
}
