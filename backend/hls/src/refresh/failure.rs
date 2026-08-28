//! Why a manifest fetch failed, and how long to wait before retrying.
//!
//! `classify_manifest_fetch_failure` turns an `OriginManifestFetchError` into a
//! hard/soft verdict plus the HTTP evidence the log lines quote;
//! `apply_manifest_fetch_failure_signal` writes the resulting backoff onto the
//! session. The backoff schedule is read from the environment once and cached.

use super::{HlsPostRefreshAvailabilityAction, HlsPostRefreshAvailabilityReason};
use crate::{
    manifest_acceptance::{HlsManifestAcceptanceEpisode, HlsManifestAcceptanceExhaustionReason},
    manifest_fetch::{HlsManifestRecoveryUnavailableReason, OriginManifestFetchError},
    safe_session_key, HlsFreshManifestRequiredReason,
};
use log::debug;
use tuliprox_core::utils::content_coding::{ContentCoding, ContentCodingError};

const FIRST_FAILURE_BACKOFF_MS: u64 = 0;
const SECOND_FAILURE_BACKOFF_MS: u64 = 500;
const LATER_FAILURE_BACKOFF_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsManifestFetchFailureKind {
    Timeout,
    RetryExhausted,
    AcceptanceConflict,
    Superseded,
    HttpStatus { status: axum::http::StatusCode },
    Transport,
    Redirect,
    ProviderAcquire { kind: crate::HlsBoundAccountAcquireErrorKind },
    InvalidContentCodingHeader,
    UnsupportedContentCoding,
    EncodedPartialContent,
    ContentPrefixRead,
    ContentDecoding { coding: ContentCoding },
    DecodedBodyLimit,
    InvalidUtf8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsManifestFetchFailureDisposition {
    Retryable,
    Hard,
    Discarded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsManifestHttpResponseEvidence {
    None,
    ValidResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HlsManifestFetchFailureSignal {
    pub(super) kind: HlsManifestFetchFailureKind,
    pub(super) disposition: HlsManifestFetchFailureDisposition,
    pub(super) response_evidence: HlsManifestHttpResponseEvidence,
}

impl HlsManifestFetchFailureSignal {
    pub(super) const fn retryable(
        kind: HlsManifestFetchFailureKind,
        response_evidence: HlsManifestHttpResponseEvidence,
    ) -> Self {
        Self { kind, disposition: HlsManifestFetchFailureDisposition::Retryable, response_evidence }
    }

    pub(super) const fn hard(
        kind: HlsManifestFetchFailureKind,
        response_evidence: HlsManifestHttpResponseEvidence,
    ) -> Self {
        Self { kind, disposition: HlsManifestFetchFailureDisposition::Hard, response_evidence }
    }

    pub(super) const fn discarded(kind: HlsManifestFetchFailureKind) -> Self {
        Self {
            kind,
            disposition: HlsManifestFetchFailureDisposition::Discarded,
            response_evidence: HlsManifestHttpResponseEvidence::None,
        }
    }

    pub(super) const fn is_hard(self) -> bool { matches!(self.disposition, HlsManifestFetchFailureDisposition::Hard) }

    const fn is_discarded(self) -> bool { matches!(self.disposition, HlsManifestFetchFailureDisposition::Discarded) }

    const fn has_valid_http_response(self) -> bool {
        matches!(self.response_evidence, HlsManifestHttpResponseEvidence::ValidResponse)
    }
}

/// Parse a "first,second,later" millisecond triple; anything else yields `None`.
fn parse_refresh_failure_backoff_schedule(value: &str) -> Option<[u64; 3]> {
    let parts: Vec<u64> = value.split(',').map(str::trim).map(str::parse).collect::<Result<_, _>>().ok()?;
    match parts.as_slice() {
        [first, second, later] => Some([*first, *second, *later]),
        _ => None,
    }
}

/// Failure backoff schedule; overridable via `TULIPROX_HLS_REFRESH_BACKOFF_MS` ("first,second,later").
pub(super) fn refresh_failure_backoff_schedule() -> [u64; 3] {
    static SCHEDULE: std::sync::LazyLock<[u64; 3]> = std::sync::LazyLock::new(|| {
        let default = [FIRST_FAILURE_BACKOFF_MS, SECOND_FAILURE_BACKOFF_MS, LATER_FAILURE_BACKOFF_MS];
        std::env::var("TULIPROX_HLS_REFRESH_BACKOFF_MS")
            .ok()
            .and_then(|value| parse_refresh_failure_backoff_schedule(&value))
            .unwrap_or(default)
    });
    *SCHEDULE
}

pub(super) fn apply_manifest_fetch_failure_signal(
    session: &mut crate::HlsSession,
    err: &OriginManifestFetchError,
    failed_at_ms: u64,
) -> HlsPostRefreshAvailabilityAction {
    let signal = classify_manifest_fetch_failure(err);
    if signal.is_discarded() {
        return HlsPostRefreshAvailabilityAction::None;
    }
    if signal.has_valid_http_response() {
        session.origin_control.record_origin_response(failed_at_ms);
    }
    let preserves_acceptance_conflict = !signal.is_hard()
        && session.origin_control.path_condition == crate::origin_progress::HlsOriginPathCondition::AcceptanceConflict
        && matches!(
            session
                .origin_control
                .acceptance_episode
                .as_ref()
                .and_then(HlsManifestAcceptanceEpisode::exhaustion_reason),
            Some(
                HlsManifestAcceptanceExhaustionReason::NoProgress
                    | HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate
            )
        );
    let deterministic_conflict = matches!(err, OriginManifestFetchError::DeterministicTimelineConflict(_));
    let path_condition = if deterministic_conflict {
        crate::origin_progress::HlsOriginPathCondition::AcceptanceConflict
    } else if signal.is_hard() {
        session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
        crate::origin_progress::HlsOriginPathCondition::HardFetchFailure
    } else if preserves_acceptance_conflict {
        crate::origin_progress::HlsOriginPathCondition::AcceptanceConflict
    } else {
        crate::origin_progress::HlsOriginPathCondition::RetryableFetchFailure
    };
    session.origin_control.path_condition = path_condition;
    debug!(
        "HLS manifest fetch failure signal recorded: session={} kind={:?} disposition={:?} response_evidence={:?} failures={}",
        safe_session_key(&session.key),
        signal.kind,
        signal.disposition,
        signal.response_evidence,
        session.origin_refresh.consecutive_failures
    );
    let reason = if deterministic_conflict {
        Some(HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict)
    } else if signal.is_hard() {
        Some(HlsPostRefreshAvailabilityReason::HardManifestFailure)
    } else {
        None
    };
    reason.map_or(HlsPostRefreshAvailabilityAction::None, |reason| HlsPostRefreshAvailabilityAction::Reevaluate {
        reason,
        origin_progress_generation: session.origin_control.progress_generation,
        media_readiness_generation: session.activity.media_readiness_generation,
    })
}

pub(super) fn classify_manifest_fetch_failure(err: &OriginManifestFetchError) -> HlsManifestFetchFailureSignal {
    use HlsManifestFetchFailureKind as Kind;
    use HlsManifestHttpResponseEvidence::{None as NoResponse, ValidResponse};

    match err {
        OriginManifestFetchError::PermanentStatus(status) | OriginManifestFetchError::NonRetryableStatus(status) => {
            HlsManifestFetchFailureSignal::hard(Kind::HttpStatus { status: *status }, ValidResponse)
        }
        OriginManifestFetchError::RetryableStatus(status, _) => {
            HlsManifestFetchFailureSignal::retryable(Kind::HttpStatus { status: *status }, ValidResponse)
        }
        OriginManifestFetchError::RetryExhausted => {
            HlsManifestFetchFailureSignal::retryable(Kind::RetryExhausted, NoResponse)
        }
        OriginManifestFetchError::RecoveryUnavailable {
            reason: HlsManifestRecoveryUnavailableReason::NoEstablishedBindingAfterResponse,
        }
        | OriginManifestFetchError::DeterministicTimelineConflict(_) => {
            HlsManifestFetchFailureSignal::retryable(Kind::AcceptanceConflict, ValidResponse)
        }
        OriginManifestFetchError::RecoveryUnavailable {
            reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded,
        } => HlsManifestFetchFailureSignal::discarded(Kind::Superseded),
        OriginManifestFetchError::Request(message) if request_error_indicates_timeout(message) => {
            HlsManifestFetchFailureSignal::retryable(Kind::Timeout, NoResponse)
        }
        OriginManifestFetchError::Request(_) => HlsManifestFetchFailureSignal::retryable(Kind::Transport, NoResponse),
        OriginManifestFetchError::Redirect(_) => {
            HlsManifestFetchFailureSignal::retryable(Kind::Redirect, ValidResponse)
        }
        OriginManifestFetchError::Timeout => HlsManifestFetchFailureSignal::retryable(Kind::Timeout, NoResponse),
        OriginManifestFetchError::ProviderUnavailable(kind) => {
            let failure_kind = Kind::ProviderAcquire { kind: *kind };
            if kind.is_retryable_resource_failure() {
                HlsManifestFetchFailureSignal::retryable(failure_kind, NoResponse)
            } else {
                HlsManifestFetchFailureSignal::hard(failure_kind, NoResponse)
            }
        }
        OriginManifestFetchError::ContentCoding(error) => match error {
            ContentCodingError::InvalidHeader => {
                HlsManifestFetchFailureSignal::hard(Kind::InvalidContentCodingHeader, ValidResponse)
            }
            ContentCodingError::Unsupported(_) => {
                HlsManifestFetchFailureSignal::hard(Kind::UnsupportedContentCoding, ValidResponse)
            }
            ContentCodingError::EncodedPartialContent => {
                HlsManifestFetchFailureSignal::hard(Kind::EncodedPartialContent, ValidResponse)
            }
            ContentCodingError::PrefixRead(_) => {
                HlsManifestFetchFailureSignal::retryable(Kind::ContentPrefixRead, ValidResponse)
            }
        },
        OriginManifestFetchError::ContentDecoding { coding } => {
            HlsManifestFetchFailureSignal::retryable(Kind::ContentDecoding { coding: *coding }, ValidResponse)
        }
        OriginManifestFetchError::DecodedBodyLimitExceeded { .. } => {
            HlsManifestFetchFailureSignal::hard(Kind::DecodedBodyLimit, ValidResponse)
        }
        OriginManifestFetchError::InvalidUtf8 { .. } => {
            HlsManifestFetchFailureSignal::hard(Kind::InvalidUtf8, ValidResponse)
        }
    }
}

pub(super) fn manifest_hard_fetch_error(err: &OriginManifestFetchError) -> bool {
    classify_manifest_fetch_failure(err).is_hard()
}

pub(super) fn request_error_indicates_timeout(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("timeout") || lower.contains("timed out")
}

#[cfg(test)]
mod backoff_schedule_tests {
    use super::parse_refresh_failure_backoff_schedule;

    #[test]
    fn parses_valid_three_value_override() {
        assert_eq!(parse_refresh_failure_backoff_schedule("100, 200,300"), Some([100, 200, 300]));
    }

    #[test]
    fn rejects_malformed_values() {
        assert_eq!(parse_refresh_failure_backoff_schedule("abc,200,300"), None);
        assert_eq!(parse_refresh_failure_backoff_schedule(""), None);
    }

    #[test]
    fn rejects_wrong_number_of_parts() {
        assert_eq!(parse_refresh_failure_backoff_schedule("100,200"), None);
        assert_eq!(parse_refresh_failure_backoff_schedule("100,200,300,400"), None);
    }
}
