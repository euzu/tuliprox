//! What can go wrong fetching or accepting an origin manifest.
//!
//! One error type for the fetch (`OriginManifestFetchError`), one for the commit
//! (`HlsManifestCommitError`), and the reject taxonomy the logs quote. Kept apart
//! from the code that raises them because every other submodule here names them.

use super::selection_log::format_optional_highwater;
use crate::{
    deterministic_conflict::HlsDeterministicTimelineConflict, manifest_limits::HlsManifestLimitViolation,
    resource_identity::HlsMediaResourceSemanticKey, timeline::HlsResourceReplayDecision,
    HlsBoundAccountAcquireErrorKind,
};
use axum::http::StatusCode;
use std::{fmt, fmt::Write as _};
use tuliprox_core::utils::content_coding::{content_decoding_error_from_io, ContentCoding, ContentCodingError};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OriginManifestStatusClass {
    Success,
    Retryable,
    PermanentFailure,
    NonRetryableFailure,
}

pub fn classify_origin_manifest_status(status: StatusCode) -> OriginManifestStatusClass {
    if status.is_success() {
        return OriginManifestStatusClass::Success;
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
        return OriginManifestStatusClass::Retryable;
    }
    if matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
            | StatusCode::GONE
    ) {
        return OriginManifestStatusClass::PermanentFailure;
    }
    OriginManifestStatusClass::NonRetryableFailure
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsManifestRecoveryUnavailableReason {
    NoEstablishedBindingAfterResponse,
    BindingSuperseded,
}

impl fmt::Display for HlsManifestRecoveryUnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoEstablishedBindingAfterResponse => "no established binding after origin response",
            Self::BindingSuperseded => "manifest origin binding superseded",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OriginManifestFetchError {
    #[error("origin manifest returned permanent status {0}")]
    PermanentStatus(StatusCode),
    #[error("origin manifest returned retryable status {0}, retry_after_ms={1:?}")]
    RetryableStatus(StatusCode, Option<u64>),
    #[error("origin manifest retry attempts exhausted")]
    RetryExhausted,
    #[error("origin manifest recovery unavailable: {reason}")]
    RecoveryUnavailable { reason: HlsManifestRecoveryUnavailableReason },
    #[error("origin manifest has a deterministic timeline conflict")]
    DeterministicTimelineConflict(Box<HlsDeterministicTimelineConflict>),
    #[error("origin manifest returned non-retryable status {0}")]
    NonRetryableStatus(StatusCode),
    #[error("origin manifest request failed: {0}")]
    Request(String),
    #[error("origin manifest redirect failed: {0}")]
    Redirect(String),
    #[error("origin manifest request timed out")]
    Timeout,
    #[error("origin provider unavailable: {0:?}")]
    ProviderUnavailable(HlsBoundAccountAcquireErrorKind),
    #[error("origin manifest content coding failed: {0}")]
    ContentCoding(ContentCodingError),
    #[error("origin manifest decoding failed: coding={coding:?}")]
    ContentDecoding { coding: ContentCoding },
    #[error("decoded origin manifest exceeds configured limit {limit}")]
    DecodedBodyLimitExceeded { limit: usize },
    #[error("decoded origin manifest is not valid UTF-8: valid_up_to={valid_up_to} error_len={error_len:?}")]
    InvalidUtf8 { valid_up_to: usize, error_len: Option<usize> },
    #[error("origin manifest exceeds a local representation limit: {0}")]
    LocalRepresentationLimit(HlsManifestLimitViolation),
    #[error("origin manifest cannot be materialized as a transient client representation")]
    MalformedTransientRepresentation,
    #[error("HLS manifest commit generation exhausted")]
    CommitGenerationExhausted,
}

impl OriginManifestFetchError {
    /// Returns a fixed/numeric diagnostic label without origin-controlled strings.
    pub fn log_label(&self) -> String {
        match self {
            Self::PermanentStatus(status) => format!("permanent_status status={}", status.as_u16()),
            Self::RetryableStatus(status, _) => format!("retryable_status status={}", status.as_u16()),
            Self::RetryExhausted => "retry_exhausted".to_string(),
            Self::RecoveryUnavailable { reason } => match reason {
                HlsManifestRecoveryUnavailableReason::NoEstablishedBindingAfterResponse => {
                    "recovery_unavailable_after_response".to_string()
                }
                HlsManifestRecoveryUnavailableReason::BindingSuperseded => {
                    "recovery_binding_superseded".to_string()
                }
            },
            Self::DeterministicTimelineConflict(conflict) => format!(
                "deterministic_timeline_conflict previous_proxy_tail={} existing_proxy_seq={} candidate_position={} candidate_origin_seq={} repeated_resource={} decision={}",
                format_optional_highwater(conflict.previous_proxy_tail),
                conflict.existing_proxy_seq,
                conflict.candidate_position,
                conflict.candidate_origin_seq,
                format_resource_identity_token(conflict.diagnostic_resource_token()),
                conflict.decision.as_log_value()
            ),
            Self::NonRetryableStatus(status) => format!("non_retryable_status status={}", status.as_u16()),
            Self::Request(_) => "request".to_string(),
            Self::Redirect(_) => "redirect".to_string(),
            Self::Timeout => "timeout".to_string(),
            Self::ProviderUnavailable(kind) => format!("provider_unavailable kind={}", kind.as_log_label()),
            Self::ContentCoding(error) => match error {
                ContentCodingError::InvalidHeader => "content_coding class=invalid_header".to_string(),
                ContentCodingError::Unsupported(_) => "content_coding class=unsupported".to_string(),
                ContentCodingError::EncodedPartialContent => "content_coding class=encoded_partial_content".to_string(),
                ContentCodingError::PrefixRead(error) => content_decoding_error_from_io(error).map_or_else(
                    || "content_coding class=prefix_read".to_string(),
                    |error| format!("content_decoding coding={}", error.coding.as_http_token()),
                ),
            },
            Self::ContentDecoding { coding, .. } => {
                format!("content_decoding coding={}", coding.as_http_token())
            }
            Self::DecodedBodyLimitExceeded { limit } => format!("decoded_body_limit limit={limit}"),
            Self::InvalidUtf8 { valid_up_to, error_len } => {
                format!("invalid_utf8 valid_up_to={valid_up_to} error_len={error_len:?}")
            }
            Self::LocalRepresentationLimit(violation) => format!(
                "local_representation_limit kind={} actual={} limit={}",
                violation.kind.as_log_value(),
                violation.actual,
                violation.limit
            ),
            Self::MalformedTransientRepresentation => "malformed_transient_representation".to_string(),
            Self::CommitGenerationExhausted => "commit_generation_exhausted".to_string(),
        }
    }
}

fn format_resource_identity_token(resource_identity: [u8; 8]) -> String {
    let mut token = String::with_capacity(resource_identity.len().saturating_mul(2));
    for byte in resource_identity {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

#[derive(Debug)]
pub enum HlsManifestCommitError {
    TimelineRejected { reason: HlsManifestRejectLogReason },
    RetryCurrentTarget,
    LocalRepresentationLimit(HlsManifestLimitViolation),
    MalformedTransientRepresentation,
    CommitGenerationExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsManifestAcceptanceRejectReason {
    MissingOriginHighwater,
    ForwardTooFar { previous: u64, origin: u64, window: Option<u64> },
    BackwardOutsideRollover { previous: u64, origin: u64, window: Option<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsManifestRejectLogReason {
    MissingOriginHighwater,
    ForwardTooFar {
        previous: u64,
        origin: u64,
        window: Option<u64>,
    },
    BackwardOutsideRollover {
        previous: u64,
        origin: u64,
        window: Option<u64>,
    },
    PinnedHostRecoveryRejected,
    UnsupportedSegmentExtension,
    UnsupportedMapExtension,
    ProxySequenceOverflow,
    ProxyMapIdOverflow,
    MissingKeyResource,
    PublishedResourceReplay {
        previous_proxy_tail: Option<u64>,
        existing_proxy_seq: u64,
        candidate_position: usize,
        candidate_origin_seq: u64,
        resource_key: HlsMediaResourceSemanticKey,
        decision: HlsResourceReplayDecision,
    },
    OriginSequenceResourceConflict {
        existing_proxy_seq: u64,
        candidate_origin_seq: u64,
    },
    SwitchResourceUnavailable,
    SwitchEncryptionKeyNotReady,
    SwitchMapResetUnsupported,
    CriticalHandoffLockContentionExhausted,
    StagedSwitchInvalidated,
    MalformedTransientTimeline,
}

impl HlsManifestRejectLogReason {
    pub fn status_label(&self) -> String {
        match self {
            Self::MissingOriginHighwater => "missing-origin-highwater".to_string(),
            Self::ForwardTooFar { previous, origin, window } => {
                format!(
                    "forward-too-far previous={previous} origin={origin} window={}",
                    format_optional_highwater(*window)
                )
            }
            Self::BackwardOutsideRollover { previous, origin, window } => {
                format!(
                    "backward-outside-rollover previous={previous} origin={origin} window={}",
                    format_optional_highwater(*window)
                )
            }
            Self::PinnedHostRecoveryRejected => "pinned-host-recovery-rejected".to_string(),
            Self::UnsupportedSegmentExtension => "unsupported-segment-extension".to_string(),
            Self::UnsupportedMapExtension => "unsupported-map-extension".to_string(),
            Self::ProxySequenceOverflow => "proxy-sequence-overflow".to_string(),
            Self::ProxyMapIdOverflow => "proxy-map-id-overflow".to_string(),
            Self::MissingKeyResource => "missing-key-resource".to_string(),
            Self::PublishedResourceReplay {
                previous_proxy_tail,
                existing_proxy_seq,
                candidate_position,
                candidate_origin_seq,
                resource_key,
                decision,
            } => {
                let resource_identity = format_resource_identity_token(resource_key.diagnostic_token());
                format!(
                    "published-resource-replay previous_proxy_tail={} existing_proxy_seq={existing_proxy_seq} candidate_position={candidate_position} candidate_origin_seq={candidate_origin_seq} repeated_resource={resource_identity} decision={}",
                    format_optional_highwater(*previous_proxy_tail),
                    decision.as_log_value()
                )
            }
            Self::OriginSequenceResourceConflict { existing_proxy_seq, candidate_origin_seq } => {
                format!(
                    "origin-sequence-resource-conflict existing_proxy_seq={existing_proxy_seq} candidate_origin_seq={candidate_origin_seq}"
                )
            }
            Self::SwitchResourceUnavailable => "switch-resource-unavailable".to_string(),
            Self::SwitchEncryptionKeyNotReady => "switch-encryption-key-not-ready".to_string(),
            Self::SwitchMapResetUnsupported => "switch-map-reset-unsupported".to_string(),
            Self::CriticalHandoffLockContentionExhausted => "critical-handoff-lock-contention-exhausted".to_string(),
            Self::StagedSwitchInvalidated => "staged-switch-invalidated".to_string(),
            Self::MalformedTransientTimeline => "malformed-transient-timeline".to_string(),
        }
    }
}

pub(super) fn is_hls_retryable_initial_manifest_fetch_error(err: &OriginManifestFetchError) -> bool {
    matches!(
        err,
        OriginManifestFetchError::RetryableStatus(_, _)
            | OriginManifestFetchError::Request(_)
            | OriginManifestFetchError::Redirect(_)
            | OriginManifestFetchError::Timeout
            | OriginManifestFetchError::ContentDecoding { .. }
            | OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(_))
    )
}

pub(super) fn is_hls_retryable_manifest_reject_fetch_error(err: &OriginManifestFetchError) -> bool {
    match err {
        OriginManifestFetchError::RetryableStatus(_, _)
        | OriginManifestFetchError::Request(_)
        | OriginManifestFetchError::Redirect(_)
        | OriginManifestFetchError::Timeout
        | OriginManifestFetchError::RetryExhausted
        | OriginManifestFetchError::ContentDecoding { .. }
        | OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(_)) => true,
        OriginManifestFetchError::PermanentStatus(_)
        | OriginManifestFetchError::NonRetryableStatus(_)
        | OriginManifestFetchError::ProviderUnavailable(_)
        | OriginManifestFetchError::RecoveryUnavailable { .. }
        | OriginManifestFetchError::DeterministicTimelineConflict(_)
        | OriginManifestFetchError::ContentCoding(_)
        | OriginManifestFetchError::DecodedBodyLimitExceeded { .. }
        | OriginManifestFetchError::InvalidUtf8 { .. }
        | OriginManifestFetchError::LocalRepresentationLimit(_)
        | OriginManifestFetchError::MalformedTransientRepresentation
        | OriginManifestFetchError::CommitGenerationExhausted => false,
    }
}
