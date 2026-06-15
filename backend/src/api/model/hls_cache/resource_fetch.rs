use super::safe_origin_log_value;
use axum::http::StatusCode;
use log::{debug, warn};
use std::time::Duration;

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
}

impl HlsResourceFetchKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Segment => "Segment",
            Self::Map => "Map",
            Self::Key => "Key",
        }
    }

    pub fn operation(self) -> &'static str {
        match self {
            Self::Segment => "segment fetch",
            Self::Map => "map fetch",
            Self::Key => "key fetch",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HlsResourceFetchLogContext<'a> {
    pub kind: HlsResourceFetchKind,
    pub object_id: &'a str,
    pub origin_url: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub enum HlsResourceFetchLogStatus {
    Http(StatusCode),
    Timeout,
    TransportError,
    RedirectError,
    CacheCommitError,
    ProviderUnavailable,
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
            Self::CacheCommitError => "CacheCommitError".to_string(),
            Self::ProviderUnavailable => "ProviderUnavailable".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HlsResourceFetchAttempt {
    pub attempt_index: usize,
    pub attempts: usize,
}

pub fn log_hls_resource_attempt_started(
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
) {
    if let Some(origin_url) = context.origin_url {
        debug!(
            "{} '{}' attempting URL attempt {} of {}: {}",
            context.kind.label(),
            context.object_id,
            attempt.attempt_index,
            attempt.attempts,
            safe_origin_log_value(origin_url)
        );
    } else {
        debug!(
            "{} '{}' attempting URL attempt {} of {}",
            context.kind.label(),
            context.object_id,
            attempt.attempt_index,
            attempt.attempts
        );
    }
}

pub fn log_hls_resource_attempt_succeeded(
    context: HlsResourceFetchLogContext<'_>,
    elapsed: Duration,
) {
    debug!(
        "{} '{}' success: {} took {:.3}s",
        context.kind.label(),
        context.object_id,
        context.kind.operation(),
        elapsed.as_secs_f64()
    );
}

pub fn log_hls_resource_retry_scheduled(
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    status: HlsResourceFetchLogStatus,
    next_delay_ms: u64,
) {
    warn!(
        "{} '{}' retry scheduled: status {} attempt {} of {} next_delay_ms={}",
        context.kind.label(),
        context.object_id,
        status.label(),
        attempt.attempt_index,
        attempt.attempts,
        next_delay_ms
    );
}

pub fn log_hls_resource_fetch_failed(
    context: HlsResourceFetchLogContext<'_>,
    attempt: HlsResourceFetchAttempt,
    status: HlsResourceFetchLogStatus,
) {
    warn!(
        "{} '{}' failed: status {} attempt {} of {}",
        context.kind.label(),
        context.object_id,
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
        "HLS origin object fetch timed out: session={} kind={} object={} attempt={} of {} deadline_ms={}",
        safe_origin_log_value(session),
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
    use super::HlsResourceFetchLogStatus;
    use axum::http::StatusCode;

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
}
