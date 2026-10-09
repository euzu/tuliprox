use super::{
    panel_api_retry_after_from_header_value, panel_api_retryable_status, PANEL_API_DEFAULT_RETRY_AFTER_SECS,
    PANEL_API_MAX_RETRY_AFTER_SECS,
};
use axum::http::StatusCode;
use std::time::Duration;

#[test]
fn panel_api_retryable_status_covers_rate_limit_and_temporary_failures() {
    assert!(panel_api_retryable_status(StatusCode::TOO_MANY_REQUESTS));
    assert!(panel_api_retryable_status(StatusCode::REQUEST_TIMEOUT));
    assert!(panel_api_retryable_status(StatusCode::TOO_EARLY));
    assert!(panel_api_retryable_status(StatusCode::BAD_GATEWAY));
    assert!(!panel_api_retryable_status(StatusCode::BAD_REQUEST));
    assert!(!panel_api_retryable_status(StatusCode::UNAUTHORIZED));
    assert!(!panel_api_retryable_status(StatusCode::NOT_FOUND));
}

#[test]
fn panel_api_retry_after_header_is_short_and_bounded() {
    assert_eq!(panel_api_retry_after_from_header_value("2"), Some(Duration::from_secs(2)));
    assert_eq!(
        panel_api_retry_after_from_header_value("0"),
        Some(Duration::from_secs(PANEL_API_DEFAULT_RETRY_AFTER_SECS))
    );
    assert_eq!(
        panel_api_retry_after_from_header_value("600"),
        Some(Duration::from_secs(PANEL_API_MAX_RETRY_AFTER_SECS))
    );
    assert_eq!(panel_api_retry_after_from_header_value("not-a-delay"), None);
}
