//! Transport-neutral HTTP execution primitives.
//!
//! Moved out of the Axum endpoint layer so the recording subsystem can
//! drive resumable VOD/Series downloads through the same code path
//! that the legacy `/file/download` handler did, without the endpoint
//! module having to expose it.
//!
//! Everything in this module is pure HTTP semantics: Range request
//! construction, `Content-Range` parsing, retryability classification,
//! and resume-response validation. The bytes-on-disk loop, the
//! queue-mutation boundary, the event-emission calls, and the
//! provider-handle bookkeeping all stay in the caller; this module
//! only knows about `reqwest` types and the resume contract.
//!
//! The dependency direction stays `app -> dvr`: this module never
//! touches Axum extractors or responses, and it never calls into the
//! queue. Callers pass in the queue's task view, an `reqwest::Client`,
//! the cancel/control signals, and the `ResumeValidator` they captured
//! when the partial file was opened.

use reqwest::{header::HeaderMap, StatusCode};

/// Classify an HTTP status as worth retrying. 5xx, 408, 429.
pub fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::REQUEST_TIMEOUT
}

/// Classify a `reqwest::Error` as transient (timeout, connect, or
/// known transient message).
pub fn is_retryable_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || retryable_transport_error_message(&err.to_string())
}

/// Substring match against the common transient transport messages.
/// Lower-cased once per call.
pub fn retryable_transport_error_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("timed out")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("temporary failure")
        || msg.contains("temporarily unavailable")
        || msg.contains("network is unreachable")
        || msg.contains("dns")
        || msg.contains("name or service not known")
        || msg.contains("connection closed before message completed")
        || msg.contains("unexpected eof")
}

/// Parse the start byte from a `Content-Range: bytes START-END/TOTAL`
/// header value. Returns `None` for anything not in that exact shape
/// — the partial-content contract requires it, and the resume gate
/// below must not accept a response whose start we cannot prove.
pub fn parse_content_range_start(content_range: &str) -> Option<u64> {
    let bytes_prefix = content_range.strip_prefix("bytes ")?;
    let start_end = bytes_prefix.split('-').next()?;
    start_end.parse::<u64>().ok()
}

/// Parse the total from a `Content-Range: bytes START-END/TOTAL`
/// header value. `*` and malformed numbers return `None`.
pub fn parse_content_range_total(content_range: &str) -> Option<u64> {
    let total = content_range.split('/').next_back()?;
    total.parse::<u64>().ok()
}

/// Parse a `Content-Range` header out of a response. Returns `None`
/// when the header is missing entirely (a `200 OK` response to a
/// Range request).
pub fn parse_content_range_total_from_headers(headers: &HeaderMap) -> Option<u64> {
    headers.get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(parse_content_range_total)
}

/// Resolve the total expected size from a successful response.
///
/// - On `206 Partial Content`, take the `Content-Range` total and
///   fall back to `content_length + existing_size` when the header is
///   missing (some CDNs omit it).
/// - On `200 OK`, the body length is the total and there is no
///   existing partial to add.
/// - Anything else returns `None`; the caller treats `None` as
///   "unknown total" and decides not to enforce the byte-count
///   invariant.
pub fn compute_total_size(response: &reqwest::Response, existing_size: u64) -> Option<u64> {
    if response.status() == StatusCode::PARTIAL_CONTENT {
        parse_content_range_total_from_headers(response.headers())
            .or_else(|| response.content_length().map(|len| len.saturating_add(existing_size)))
    } else if response.status().is_success() {
        response.content_length()
    } else {
        None
    }
}

/// What a successful resume response must satisfy. Captured when
/// the partial file is opened so a server-side change between
/// pause and resume cannot silently corrupt the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumeValidator {
    /// Byte offset we asked for with `Range: bytes=START-`.
    pub expected_offset: u64,
    /// Optional `ETag` captured at partial-open time. Reject a resume
    /// whose response carries a different `ETag`: the resource was
    /// replaced mid-recording.
    pub expected_etag: Option<String>,
    /// Optional `Last-Modified` captured at partial-open time. Same
    /// rationale as `expected_etag`.
    pub expected_last_modified: Option<String>,
    /// Optional total expected size from the original response's
    /// `Content-Range` or `Content-Length`. The resume response must
    /// agree.
    pub expected_total: Option<u64>,
}

/// Why a resume response is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeValidationError {
    /// Server returned `200 OK` despite our `Range` request — the
    /// body starts at offset 0. Appending it would clobber the
    /// partial.
    IgnoredRange,
    /// `Content-Range` start byte does not match what we asked for.
    StartMismatch { expected: u64, got: u64 },
    /// `Content-Range` total does not match what we recorded.
    TotalMismatch { expected: u64, got: u64 },
    /// `ETag` changed between partial open and resume. The resource
    /// was replaced — do not append.
    ETagMismatch { expected: String, got: String },
    /// `Last-Modified` changed between partial open and resume.
    LastModifiedMismatch { expected: String, got: String },
    /// Server returned `416 Range Not Satisfiable`. `complete` is
    /// `true` when the response carries a `Content-Range` that proves
    /// the partial already covers the whole resource (treat as
    /// completion); `false` when the range is genuinely past the end
    /// and we have to surface the error to the caller.
    Unsatisfiable { complete: bool },
}

impl std::fmt::Display for ResumeValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IgnoredRange => f.write_str("server ignored Range header"),
            Self::StartMismatch { expected, got } => {
                write!(f, "resume start mismatch: expected {expected}, got {got}")
            }
            Self::TotalMismatch { expected, got } => {
                write!(f, "resume total mismatch: expected {expected}, got {got}")
            }
            Self::ETagMismatch { expected, got } => {
                write!(f, "resume ETag mismatch: expected {expected:?}, got {got:?}")
            }
            Self::LastModifiedMismatch { expected, got } => {
                write!(f, "resume Last-Modified mismatch: expected {expected:?}, got {got:?}")
            }
            Self::Unsatisfiable { complete: true } => f.write_str("range already satisfied by partial"),
            Self::Unsatisfiable { complete: false } => f.write_str("range unsatisfiable and partial does not cover"),
        }
    }
}

impl std::error::Error for ResumeValidationError {}

/// Pull the validator-relevant headers off a response. Exposed so
/// callers can pass them to [`validate_resume_response`] without
/// holding the full `reqwest::Response`.
#[derive(Debug, Clone, Default)]
pub struct ResponseSnapshot {
    pub status: StatusCode,
    pub content_range: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// The `ETag` of a response, but only when it is a strong validator.
///
/// RFC 9110 forbids a weak validator in a range request precisely because it
/// only promises semantic equivalence: two responses can share a weak tag and
/// still differ byte for byte. Appending such a response to a partial file
/// would corrupt it silently, so a weak tag is treated as no validator at all.
pub fn strong_etag(etag: &str) -> Option<&str> {
    let trimmed = etag.trim();
    if trimmed.starts_with("W/") || trimmed.starts_with("w/") || !trimmed.starts_with('"') {
        return None;
    }
    Some(trimmed)
}

impl ResponseSnapshot {
    /// Capture validator-relevant headers.
    pub fn from_response(response: &reqwest::Response) -> Self {
        Self {
            status: response.status(),
            content_range: response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            etag: response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string),
            last_modified: response
                .headers()
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        }
    }
}

/// Validate a resume response against the validator captured when the
/// partial was opened. Returns `Ok(())` when the response may be
/// appended; `Err(ResumeValidationError)` otherwise.
///
/// Callers that only want the headers once should construct a
/// [`ResponseSnapshot`] and pass it here. The function does not hold
/// onto the response.
pub fn validate_resume_response(
    snapshot: &ResponseSnapshot,
    validator: &ResumeValidator,
) -> Result<(), ResumeValidationError> {
    if validator.expected_offset == 0 {
        // No partial existed. A `200 OK` is the right answer; a `206`
        // for a zero-length Range is a server bug but harmless. Skip
        // the start/total check.
        if snapshot.status == StatusCode::OK {
            return Ok(());
        }
    }

    // The partial cannot be a prefix of something smaller than itself.
    // Resuming from its length would append to a file that is already not this
    // resource, and finish it as complete. Equality is the legitimate case
    // where the partial is the whole thing; the server answers 416 for it.
    if let Some(expected_total) = validator.expected_total {
        if validator.expected_offset > expected_total {
            return Err(ResumeValidationError::TotalMismatch {
                expected: expected_total,
                got: validator.expected_offset,
            });
        }
    }

    match snapshot.status {
        StatusCode::PARTIAL_CONTENT => {
            let content_range = snapshot.content_range.as_deref().unwrap_or("");
            let start = parse_content_range_start(content_range)
                .ok_or(ResumeValidationError::StartMismatch { expected: validator.expected_offset, got: 0 })?;
            if start != validator.expected_offset {
                return Err(ResumeValidationError::StartMismatch { expected: validator.expected_offset, got: start });
            }
            if let Some(expected_total) = validator.expected_total {
                if let Some(got_total) = parse_content_range_total(content_range) {
                    if got_total != expected_total {
                        return Err(ResumeValidationError::TotalMismatch { expected: expected_total, got: got_total });
                    }
                }
            }
        }
        StatusCode::OK if validator.expected_offset > 0 => return Err(ResumeValidationError::IgnoredRange),
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let content_range = snapshot.content_range.as_deref().unwrap_or("");
            // If the partial already covers the whole resource, the
            // server's 416 is the truthful signal that no bytes
            // remain. `complete = true` lets the caller promote the
            // partial to final without retrying. A 416 without a
            // `Content-Range` proves the range is past the end and
            // the partial does not cover it.
            let complete =
                parse_content_range_total(content_range).is_some_and(|total| total == validator.expected_offset);
            return Err(ResumeValidationError::Unsatisfiable { complete });
        }
        _ => {}
    }

    if let Some(expected) = validator.expected_etag.as_ref() {
        // A response that downgrades to a weak tag can no longer prove the
        // bytes are unchanged, so it fails the comparison rather than passing
        // it by absence.
        if let Some(got) = snapshot.etag.as_ref() {
            let got = strong_etag(got).unwrap_or(got.as_str());
            if got != expected {
                return Err(ResumeValidationError::ETagMismatch { expected: expected.clone(), got: got.to_owned() });
            }
        }
    }
    if let Some(expected) = validator.expected_last_modified.as_ref() {
        if let Some(got) = snapshot.last_modified.as_ref() {
            if got != expected {
                return Err(ResumeValidationError::LastModifiedMismatch {
                    expected: expected.clone(),
                    got: got.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        status: StatusCode,
        content_range: Option<&str>,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> ResponseSnapshot {
        ResponseSnapshot {
            status,
            content_range: content_range.map(str::to_string),
            etag: etag.map(str::to_string),
            last_modified: last_modified.map(str::to_string),
        }
    }

    #[test]
    fn a_weak_etag_is_not_a_usable_resume_validator() {
        // Two responses can share a weak tag and still differ byte for byte,
        // so appending to a partial on that evidence would corrupt the file.
        assert_eq!(strong_etag("W/\"abc\""), None);
        assert_eq!(strong_etag("w/\"abc\""), None);
        assert_eq!(strong_etag("abc"), None, "an unquoted tag is not a valid entity-tag");
        assert_eq!(strong_etag("\"abc\""), Some("\"abc\""));
        assert_eq!(strong_etag("  \"abc\"  "), Some("\"abc\""));
    }

    #[test]
    fn a_resume_whose_etag_weakens_is_rejected() {
        let validator = ResumeValidator {
            expected_offset: 100,
            expected_etag: Some("\"strong\"".to_string()),
            ..ResumeValidator::default()
        };
        let mut snap = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/200"), None, None);
        snap.etag = Some("W/\"strong\"".to_string());
        assert!(matches!(validate_resume_response(&snap, &validator), Err(ResumeValidationError::ETagMismatch { .. })));
    }

    #[test]
    fn parse_content_range_start_extracts_start_byte() {
        assert_eq!(parse_content_range_start("bytes 100-199/1000"), Some(100));
        assert_eq!(parse_content_range_start("bytes 0-0/42"), Some(0));
        assert_eq!(parse_content_range_start("bytes abc-1/10"), None);
        assert_eq!(parse_content_range_start("items 0-1/2"), None);
        assert_eq!(parse_content_range_start(""), None);
    }

    #[test]
    fn parse_content_range_total_extracts_total() {
        assert_eq!(parse_content_range_total("bytes 100-199/1000"), Some(1000));
        assert_eq!(parse_content_range_total("bytes 0-0/0"), Some(0));
        assert_eq!(parse_content_range_total("bytes 100-199/*"), None);
        assert_eq!(parse_content_range_total("not a range"), None);
    }

    #[test]
    fn is_retryable_status_classifies_5xx_and_429_408() {
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::OK));
    }

    #[test]
    fn retryable_transport_error_message_detects_common_transient_failures() {
        assert!(retryable_transport_error_message("operation timed out"));
        assert!(retryable_transport_error_message("Connection reset by peer"));
        assert!(retryable_transport_error_message("temporary failure in name resolution"));
        assert!(retryable_transport_error_message("temporarily unavailable"));
        assert!(retryable_transport_error_message("DNS lookup failed"));
        assert!(retryable_transport_error_message("connection closed before message completed"));
        assert!(retryable_transport_error_message("unexpected eof"));
        assert!(!retryable_transport_error_message("invalid URL"));
        assert!(!retryable_transport_error_message("permission denied"));
    }

    // --- Resume validation: the Step-2 contract ---

    #[test]
    fn resume_validation_accepts_exact_content_range_start() {
        let validator = ResumeValidator {
            expected_offset: 100,
            expected_etag: Some("v1".into()),
            expected_last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
            expected_total: Some(1000),
        };
        let snap = snapshot(
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 100-199/1000"),
            Some("v1"),
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(validate_resume_response(&snap, &validator), Ok(()));
    }

    #[test]
    fn resume_validation_rejects_inconsistent_total() {
        let validator = ResumeValidator { expected_offset: 100, expected_total: Some(1000), ..Default::default() };
        let snap = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/2000"), None, None);
        assert_eq!(
            validate_resume_response(&snap, &validator),
            Err(ResumeValidationError::TotalMismatch { expected: 1000, got: 2000 })
        );
    }

    #[test]
    fn resume_validation_rejects_etag_mismatch() {
        let validator =
            ResumeValidator { expected_offset: 100, expected_etag: Some("v1".into()), ..Default::default() };
        let snap = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/1000"), Some("v2"), None);
        assert_eq!(
            validate_resume_response(&snap, &validator),
            Err(ResumeValidationError::ETagMismatch { expected: "v1".into(), got: "v2".into() })
        );
    }

    #[test]
    fn resume_validation_rejects_last_modified_mismatch() {
        let validator =
            ResumeValidator { expected_offset: 100, expected_last_modified: Some("old".into()), ..Default::default() };
        let snap = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/1000"), None, Some("new"));
        assert_eq!(
            validate_resume_response(&snap, &validator),
            Err(ResumeValidationError::LastModifiedMismatch { expected: "old".into(), got: "new".into() })
        );
    }

    #[test]
    fn resume_validation_rejects_ignored_range_with_200() {
        let validator = ResumeValidator { expected_offset: 100, expected_total: Some(1000), ..Default::default() };
        let snap = snapshot(StatusCode::OK, None, None, None);
        assert_eq!(validate_resume_response(&snap, &validator), Err(ResumeValidationError::IgnoredRange));
    }

    #[test]
    fn resume_validation_accepts_200_when_no_partial_exists() {
        let validator = ResumeValidator::default();
        let snap = snapshot(StatusCode::OK, None, None, None);
        assert_eq!(validate_resume_response(&snap, &validator), Ok(()));
    }

    #[test]
    fn fresh_transfer_rejects_partial_response_starting_after_zero() {
        let validator = ResumeValidator::default();
        let snap = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/1000"), None, None);
        assert_eq!(
            validate_resume_response(&snap, &validator),
            Err(ResumeValidationError::StartMismatch { expected: 0, got: 100 })
        );
    }

    #[test]
    fn resume_validation_flags_416_as_complete_when_total_matches_partial() {
        let validator = ResumeValidator { expected_offset: 1000, expected_total: Some(1000), ..Default::default() };
        let snap = snapshot(StatusCode::RANGE_NOT_SATISFIABLE, Some("bytes 1000-999/1000"), None, None);
        assert_eq!(
            validate_resume_response(&snap, &validator),
            Err(ResumeValidationError::Unsatisfiable { complete: true })
        );
    }

    #[test]
    fn resume_validation_flags_416_as_incomplete_when_total_exceeds_partial() {
        let validator = ResumeValidator { expected_offset: 100, expected_total: Some(1000), ..Default::default() };
        let snap = snapshot(StatusCode::RANGE_NOT_SATISFIABLE, Some("bytes 1000-999/1000"), None, None);
        assert_eq!(
            validate_resume_response(&snap, &validator),
            Err(ResumeValidationError::Unsatisfiable { complete: false })
        );
    }

    #[test]
    fn resume_validation_treats_416_without_content_range_as_incomplete() {
        let validator = ResumeValidator { expected_offset: 100, expected_total: Some(1000), ..Default::default() };
        let snap = snapshot(StatusCode::RANGE_NOT_SATISFIABLE, None, None, None);
        assert_eq!(
            validate_resume_response(&snap, &validator),
            Err(ResumeValidationError::Unsatisfiable { complete: false })
        );
    }

    #[test]
    fn resume_validation_rejects_start_mismatch() {
        let validator = ResumeValidator { expected_offset: 100, expected_total: Some(1000), ..Default::default() };
        let snap = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 200-299/1000"), None, None);
        assert_eq!(
            validate_resume_response(&snap, &validator),
            Err(ResumeValidationError::StartMismatch { expected: 100, got: 200 })
        );
    }

    #[test]
    fn resume_validation_skips_total_check_when_validator_omits_total() {
        let validator = ResumeValidator { expected_offset: 100, expected_total: None, ..Default::default() };
        // Total disagrees but the validator did not pin one — the gate
        // must accept, because legacy tasks persisted before the total
        // field landed have no recorded total.
        let snap = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/9999"), None, None);
        assert_eq!(validate_resume_response(&snap, &validator), Ok(()));
    }
    #[test]
    fn last_modified_is_accepted_when_no_strong_etag_was_offered() {
        // The fallback validator: weaker than an ETag, but it is what some
        // providers give, and without it every interruption restarts at zero.
        let response = snapshot(
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 100-199/200"),
            None,
            Some("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        let validator = ResumeValidator {
            expected_offset: 100,
            expected_total: Some(200),
            expected_etag: None,
            expected_last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        };
        assert!(validate_resume_response(&response, &validator).is_ok());
    }

    #[test]
    fn a_resume_with_no_validator_at_all_is_still_checked_on_offsets() {
        // Nothing proves the bytes are the same resource, so the only thing
        // left to verify is that the server resumed where we asked.
        let response = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/200"), None, None);
        let validator =
            ResumeValidator { expected_offset: 100, expected_total: Some(200), ..ResumeValidator::default() };
        assert!(validate_resume_response(&response, &validator).is_ok());

        let wrong_place = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 50-199/200"), None, None);
        assert!(validate_resume_response(&wrong_place, &validator).is_err(), "a different offset is still a mismatch");
    }

    #[test]
    fn a_partial_longer_than_the_total_is_refused() {
        // The local file cannot be a prefix of a smaller resource, so resuming
        // from its length would append to something that is not the same file.
        let response = snapshot(StatusCode::PARTIAL_CONTENT, Some("bytes 300-399/200"), None, None);
        let validator =
            ResumeValidator { expected_offset: 300, expected_total: Some(200), ..ResumeValidator::default() };
        assert!(validate_resume_response(&response, &validator).is_err());
    }
}
