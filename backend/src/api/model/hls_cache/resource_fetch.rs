use super::{
    append_hls_provider_session_headers, force_identity_without_range, hls_object_body_deadline, safe_origin_log_value,
    safe_proxy_session_id, scrub_hls_origin_headers, HlsBoundAccountAcquireErrorKind, ProxySessionId,
    SegmentFetchPolicy,
};
use crate::processing::parser::hls::origin_manifest::ParsedByteRange;
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
pub enum HlsResourceStatusClass {
    Success,
    Retryable,
    Permanent,
    NonRetryable,
}

pub fn classify_hls_resource_status(status: StatusCode) -> HlsResourceStatusClass {
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
    Other,
}

impl HlsResourceFetchKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Segment => "Segment",
            Self::Map => "Map",
            Self::Key => "Key",
            Self::Other => "Resource",
        }
    }

    pub fn operation(self) -> &'static str {
        match self {
            Self::Segment => "segment fetch",
            Self::Map => "map fetch",
            Self::Key => "key fetch",
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
    pub fn log_context(&self) -> HlsResourceFetchLogContext<'_> {
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
    InvalidOriginUrl,
    InvalidByteRange,
    UnexpectedByteRangeStatus,
    CacheCommit(HlsCacheCommitFailure),
    ProviderUnavailable(HlsBoundAccountAcquireErrorKind),
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
        let raw_os_error = self
            .raw_os_error
            .map_or_else(|| "none".to_string(), |code| code.to_string());
        format!(
            "CacheCommitError(io_kind={} raw_os_error={} storage_full={} message=\"{}\")",
            self.io_kind, raw_os_error, self.storage_full, self.message
        )
    }

    pub fn storage_full(&self) -> bool { self.storage_full }
}

impl HlsOriginResourceFetchError {
    pub fn cache_commit(err: &io::Error) -> Self { Self::CacheCommit(HlsCacheCommitFailure::from_io_error(err)) }

    pub fn retryable_failure(&self) -> bool {
        matches!(
            self,
            Self::RetryableStatus(_) | Self::Transport(_) | Self::Redirect | Self::Timeout
        )
            || matches!(self, Self::CacheCommit(failure) if !failure.storage_full())
            || matches!(self, Self::ProviderUnavailable(kind) if kind.is_retryable_resource_failure())
    }

    pub fn permanent_status(&self) -> Option<StatusCode> {
        match self {
            Self::PermanentStatus(status) | Self::NonRetryableStatus(status) => Some(*status),
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
            | Self::UnexpectedByteRangeStatus => true,
            Self::ProviderUnavailable(kind) => !kind.is_retryable_resource_failure(),
            Self::RetryableStatus(_)
            | Self::Transport(_)
            | Self::Redirect
            | Self::Timeout
            | Self::CacheCommit(_) => false,
        }
    }
}

pub type HlsOriginResourceCommitFuture<T> = BoxFuture<'static, Result<T, HlsOriginResourceFetchError>>;
pub type HlsOriginResourceAttemptPrepareFuture<G> = BoxFuture<'static, Result<G, HlsOriginResourceFetchError>>;
pub type HlsOriginResourceAttemptCleanupFuture = BoxFuture<'static, ()>;

pub async fn run_hls_origin_resource_retry_loop<T, F>(
    target: HlsOriginResourceFetchTarget,
    clients: HlsOriginResourceClients,
    policy: &SegmentFetchPolicy,
    session_log_id: &str,
    mut commit: F,
) -> Result<T, HlsOriginResourceFetchError>
where
    T: Send + 'static,
    F: FnMut(reqwest::Response, HlsResourceFetchAttempt) -> HlsOriginResourceCommitFuture<T>,
{
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        clients,
        policy,
        session_log_id,
        |_| async { Ok(()) }.boxed(),
        |()| async {}.boxed(),
        move |response, attempt, ()| commit(response, attempt),
    )
    .await
}

/// Runs the shared HLS origin-resource retry policy with an optional per-attempt guard.
///
/// The prepare callback runs after the attempt log and before the HTTP request. If the HTTP
/// request fails before the commit callback takes ownership of the guard, the cleanup callback is
/// invoked by the runner. Once the commit callback receives a guard, it owns its cleanup path. This
/// lets direct passthrough callers return a guard with the response body while cache-commit callers
/// release it after the commit finishes.
pub async fn run_hls_origin_resource_retry_loop_with_attempt_prepare<T, G, P, C, F>(
    target: HlsOriginResourceFetchTarget,
    clients: HlsOriginResourceClients,
    policy: &SegmentFetchPolicy,
    session_log_id: &str,
    mut prepare_attempt: P,
    mut cleanup_attempt: C,
    mut commit: F,
) -> Result<T, HlsOriginResourceFetchError>
where
    T: Send + 'static,
    G: Send + 'static,
    P: FnMut(HlsResourceFetchAttempt) -> HlsOriginResourceAttemptPrepareFuture<G>,
    C: FnMut(G) -> HlsOriginResourceAttemptCleanupFuture,
    F: FnMut(reqwest::Response, HlsResourceFetchAttempt, G) -> HlsOriginResourceCommitFuture<T>,
{
    let attempts = policy.retry_delays_ms.len();
    for attempt_index in 0..attempts {
        let attempt = HlsResourceFetchAttempt { attempt_index, attempts };
        let delay_ms = policy.retry_delay_ms(attempt_index);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        log_hls_resource_attempt_started(session_log_id, target.log_context(), attempt);
        let attempt_started_at = Instant::now();
        let result = match prepare_attempt(attempt).await {
            Ok(guard) => match fetch_hls_origin_resource_response(&target, &clients).await {
                Ok(response) => commit(response, attempt, guard).await,
                Err(err) => {
                    cleanup_attempt(guard).await;
                    Err(err)
                }
            },
            Err(err) => Err(err),
        };

        match result {
            Ok(output) => {
                log_hls_resource_attempt_succeeded(session_log_id, target.log_context(), attempt_started_at.elapsed());
                return Ok(output);
            }
            Err(err) if err.aborts_without_retry() || attempt_index + 1 == attempts => {
                if matches!(err, HlsOriginResourceFetchError::Timeout) {
                    log_hls_resource_timeout(
                        session_log_id,
                        target.log_context(),
                        attempt,
                        hls_object_body_deadline(policy.origin_segment_timeout_ms).as_millis(),
                    );
                }
                log_hls_resource_fetch_failed(session_log_id, target.log_context(), attempt, err.log_status());
                return Err(err);
            }
            Err(err) => {
                if matches!(err, HlsOriginResourceFetchError::Timeout) {
                    log_hls_resource_timeout(
                        session_log_id,
                        target.log_context(),
                        attempt,
                        hls_object_body_deadline(policy.origin_segment_timeout_ms).as_millis(),
                    );
                }
                log_hls_resource_retry_scheduled(
                    session_log_id,
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

pub async fn fetch_hls_origin_resource_response(
    target: &HlsOriginResourceFetchTarget,
    clients: &HlsOriginResourceClients,
) -> Result<reqwest::Response, HlsOriginResourceFetchError> {
    let url = Url::parse(&target.origin_url).map_err(|_| HlsOriginResourceFetchError::InvalidOriginUrl)?;
    let response = if clients.use_manual_redirects {
        fetch_hls_origin_resource_with_manual_redirects(&url, target.headers.clone(), &clients.no_redirect_client)
            .await?
    } else {
        clients
            .client
            .get(url)
            .headers(target.headers.clone())
            .send()
            .await
            .map_err(|err| {
                HlsOriginResourceFetchError::Transport(
                    sanitize_sensitive_info(err.to_string().as_str()).to_string(),
                )
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
        let response = client
            .get(current_url.clone())
            .headers(current_headers.clone())
            .send()
            .await
            .map_err(|err| {
                HlsOriginResourceFetchError::Transport(
                    sanitize_sensitive_info(err.to_string().as_str()).to_string(),
                )
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
    let mut headers = source_headers.clone();
    scrub_hls_origin_headers(&mut headers, None);
    force_identity_without_range(&mut headers);
    append_hls_provider_session_headers(&mut headers, provider_session_headers);
    if let Some(byte_range) = byte_range {
        let end = byte_range
            .offset
            .checked_add(byte_range.length)
            .and_then(|end_exclusive| end_exclusive.checked_sub(1))
            .ok_or(HlsOriginResourceFetchError::InvalidByteRange)?;
        let range_value = format!("bytes={}-{}", byte_range.offset, end);
        let range_value = HeaderValue::from_str(&range_value)
            .map_err(|_| HlsOriginResourceFetchError::InvalidByteRange)?;
        headers.insert(header::RANGE, range_value);
    }
    Ok(headers)
}

pub fn build_hls_origin_resource_headers_with_client_range(
    source_headers: &HeaderMap,
    provider_session_headers: &HeaderMap,
    client_range: Option<HeaderValue>,
) -> HeaderMap {
    let mut headers = source_headers.clone();
    scrub_hls_origin_headers(&mut headers, None);
    headers.remove(header::RANGE);
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    append_hls_provider_session_headers(&mut headers, provider_session_headers);
    if let Some(range) = client_range {
        headers.insert(header::RANGE, range);
    }
    headers
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
pub struct HlsResourceFetchLogContext<'a> {
    pub kind: HlsResourceFetchKind,
    pub source: HlsResourceFetchSource,
    pub object_id: &'a str,
    pub origin_url: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub enum HlsResourceFetchLogStatus {
    Http(StatusCode),
    Timeout,
    TransportError,
    RedirectError,
    CacheCommitError(HlsCacheCommitFailure),
    ProviderUnavailable(HlsBoundAccountAcquireErrorKind),
}

impl HlsResourceFetchLogStatus {
    pub fn label(self) -> String {
        match self {
            Self::Http(status) => {
                let reason = status.canonical_reason().unwrap_or("Unknown");
                format!("{} {reason}", status.as_u16())
            }
            Self::Timeout => "Timeout".to_string(),
            Self::TransportError => "TransportError".to_string(),
            Self::RedirectError => "RedirectError".to_string(),
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

fn safe_resource_fetch_session_id(session: &str) -> String {
    safe_proxy_session_id(&ProxySessionId(session.to_string()))
}

pub fn log_hls_resource_attempt_started(
    session: &str,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
) {
    if let Some(origin_url) = context.origin_url {
        debug!(
            "{} '{}' attempting URL attempt {} of {}: session={} source={} {}",
            context.kind.label(),
            context.object_id,
            attempt.attempt_index,
            attempt.attempts,
            safe_resource_fetch_session_id(session),
            context.source.as_log_value(),
            safe_origin_log_value(origin_url)
        );
    } else {
        debug!(
            "{} '{}' attempting URL attempt {} of {}: session={} source={}",
            context.kind.label(),
            context.object_id,
            attempt.attempt_index,
            attempt.attempts,
            safe_resource_fetch_session_id(session),
            context.source.as_log_value()
        );
    }
}

pub fn log_hls_resource_attempt_succeeded(
    session: &str,
    context: HlsResourceFetchLogContext<'_>,
    elapsed: Duration,
) {
    debug!(
        "{} '{}' success: session={} source={} {} took {:.3}s",
        context.kind.label(),
        context.object_id,
        safe_resource_fetch_session_id(session),
        context.source.as_log_value(),
        context.kind.operation(),
        elapsed.as_secs_f64()
    );
}

pub fn log_hls_resource_retry_scheduled(
    session: &str,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    status: HlsResourceFetchLogStatus,
    next_delay_ms: u64,
) {
    warn!(
        "{} '{}' retry scheduled: session={} source={} status {} attempt {} of {} next_delay_ms={}",
        context.kind.label(),
        context.object_id,
        safe_resource_fetch_session_id(session),
        context.source.as_log_value(),
        status.label(),
        attempt.attempt_index,
        attempt.attempts,
        next_delay_ms
    );
}

pub fn log_hls_resource_fetch_failed(
    session: &str,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    status: HlsResourceFetchLogStatus,
) {
    warn!(
        "{} '{}' failed: session={} source={} status {} attempt {} of {}",
        context.kind.label(),
        context.object_id,
        safe_resource_fetch_session_id(session),
        context.source.as_log_value(),
        status.label(),
        attempt.attempt_index,
        attempt.attempts
    );
}

pub fn log_hls_resource_timeout(
    session: &str,
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    deadline_ms: u128,
) {
    warn!(
        "HLS origin object fetch timed out: session={} source={} kind={} object={} attempt={} of {} deadline_ms={}",
        safe_resource_fetch_session_id(session),
        context.source.as_log_value(),
        context.kind.label(),
        context.object_id,
        attempt.attempt_index,
        attempt.attempts,
        deadline_ms
    );
}

pub fn retry_after_secs_from_ms(retry_after_ms: u64) -> u64 {
    retry_after_ms.saturating_add(999).saturating_div(1_000).max(1)
}

#[cfg(test)]
mod tests {
    use super::{
        build_hls_origin_resource_headers_with_client_range, HlsCacheCommitFailure, HlsOriginResourceFetchError,
        HlsResourceFetchLogStatus,
    };
    use crate::api::model::HlsBoundAccountAcquireErrorKind;
    use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
    use std::io;

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
    fn cache_commit_failure_marks_enospc_as_storage_full() {
        let failure = HlsCacheCommitFailure::from_io_error(&io::Error::from_raw_os_error(28));

        assert!(failure.storage_full());
        assert!(HlsResourceFetchLogStatus::CacheCommitError(failure)
            .label()
            .contains("storage_full=true"));
    }

    #[test]
    fn storage_full_cache_commit_failure_is_not_retryable() {
        let err = HlsOriginResourceFetchError::cache_commit(&io::Error::from_raw_os_error(28));

        assert!(!err.retryable_failure());
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

        let headers =
            build_hls_origin_resource_headers_with_client_range(&client_headers, &provider_headers, None);

        assert_eq!(headers.get(header::COOKIE).expect("cookie"), "sid=provider");
        assert!(!headers.contains_key(header::RANGE));
        assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("encoding"), "identity");
    }
}
