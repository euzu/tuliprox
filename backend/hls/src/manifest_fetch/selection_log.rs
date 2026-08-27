//! The log lines that explain a manifest selection after the fact.
//!
//! Recovery decisions are hard to reconstruct from state alone, so each phase -
//! initial attempt, candidate scored, candidate rejected, selection made, retry
//! scheduled - emits one structured line. Collected here so the decision code
//! reads as decisions.

use super::{
    error::{HlsManifestRejectLogReason, OriginManifestFetchError},
    HlsManifestRecoveryCandidateScoreReport, HlsOriginManifestFetchContext,
};
// Only the cfg-gated retry logger names this type.
#[cfg(any(test, feature = "test-support"))]
use crate::api::LiveHlsOriginEntry;
use crate::{hls_origin_log_value, safe_session_key};
use log::{debug, warn};

pub async fn log_hls_manifest_initial_selected(
    context: &HlsOriginManifestFetchContext,
    report: &HlsManifestRecoveryCandidateScoreReport,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    debug!(
        "Manifest '{}' initial selected: host={} media-sequence={} highwater={} score={}",
        session_label,
        report.quality.effective_host.as_deref().map_or_else(|| "none".to_string(), hls_origin_log_value),
        report.media_sequence,
        format_optional_highwater(report.quality.origin_highwater),
        report.quality.score.as_log_value()
    );
}

pub(super) async fn log_manifest_recovery_candidate_scored(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    report: &HlsManifestRecoveryCandidateScoreReport,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    debug!(
        "Manifest '{}' candidate {} of {} scored: host={} media-sequence={} highwater={} score={}",
        session_label,
        candidate_index + 1,
        candidates,
        report.quality.effective_host.as_deref().map_or_else(|| "none".to_string(), hls_origin_log_value),
        report.media_sequence,
        format_optional_highwater(report.quality.origin_highwater),
        report.quality.score.as_log_value()
    );
}

pub(super) async fn log_manifest_recovery_candidate_rejected(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    host: Option<&str>,
    highwater: Option<u64>,
    reason: &HlsManifestRejectLogReason,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    debug!(
        "Manifest '{}' candidate {} of {} rejected: host={} highwater={} reason={}",
        session_label,
        candidate_index + 1,
        candidates,
        host.map_or_else(|| "none".to_string(), hls_origin_log_value),
        format_optional_highwater(highwater),
        reason.status_label()
    );
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ManifestRecoverySelectionLogPhase {
    Recovery,
    Burst,
}

impl ManifestRecoverySelectionLogPhase {
    pub const fn from_candidate_count(candidates: usize) -> Self {
        if candidates > 1 {
            Self::Burst
        } else {
            Self::Recovery
        }
    }

    pub const fn as_log_label(self) -> &'static str {
        match self {
            Self::Recovery => "recovery",
            Self::Burst => "burst",
        }
    }
}

pub(super) async fn log_manifest_initial_attempt(context: &HlsOriginManifestFetchContext) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let input_source = context.origin_entry.to_input_source();
    debug!(
        "Manifest '{}' attempting URL attempt initial: request_url={} reason=origin-refresh",
        session_label,
        hls_origin_log_value(input_source.url.as_str())
    );
}

pub(super) async fn log_manifest_recovery_selected(
    context: &HlsOriginManifestFetchContext,
    candidate_index: usize,
    candidates: usize,
    report: &HlsManifestRecoveryCandidateScoreReport,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let phase = ManifestRecoverySelectionLogPhase::from_candidate_count(candidates);
    debug!(
        "Manifest '{}' {} selected candidate {} of {}: host={} media-sequence={} highwater={} score={}",
        session_label,
        phase.as_log_label(),
        candidate_index + 1,
        candidates,
        report.quality.effective_host.as_deref().map_or_else(|| "none".to_string(), hls_origin_log_value),
        report.media_sequence,
        format_optional_highwater(report.quality.origin_highwater),
        report.quality.score.as_log_value()
    );
}

pub fn format_optional_highwater(highwater: Option<u64>) -> String {
    highwater.map_or_else(|| "none".to_string(), |value| value.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManifestRetryLogKind {
    InitialFetch,
    PinnedHostRecovery,
}

pub(super) async fn log_manifest_retry_scheduled(
    context: &HlsOriginManifestFetchContext,
    retry_kind: ManifestRetryLogKind,
    attempt_index: usize,
    attempts: usize,
    delay_ms: u64,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    fetch_error: Option<&OriginManifestFetchError>,
) {
    let session_label = {
        let session = context.session.read().await;
        safe_session_key(&session.key)
    };
    let status = manifest_retry_status_label(retry_kind, reject_reason, fetch_error);
    warn!(
        "Manifest '{}' retry scheduled: status {} attempt {} of {} next_delay_ms={delay_ms}",
        session_label,
        status,
        attempt_index + 1,
        attempts
    );
}

pub(super) fn manifest_retry_status_label(
    retry_kind: ManifestRetryLogKind,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    fetch_error: Option<&OriginManifestFetchError>,
) -> String {
    match (retry_kind, reject_reason, fetch_error) {
        (_, Some(reason), Some(err)) => format!("{} error={}", reason.status_label(), err.log_label()),
        (_, Some(reason), None) => reason.status_label(),
        (ManifestRetryLogKind::InitialFetch, None, Some(err)) => {
            format!("initial-fetch error={}", err.log_label())
        }
        (ManifestRetryLogKind::InitialFetch, None, None) => "initial-fetch".to_string(),
        (ManifestRetryLogKind::PinnedHostRecovery, None, Some(err)) => {
            format!("pinned-host-recovery error={}", err.log_label())
        }
        (ManifestRetryLogKind::PinnedHostRecovery, None, None) => "pinned-host-recovery".to_string(),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn log_origin_refresh_retry_scheduled(
    origin_entry: &LiveHlsOriginEntry,
    attempt_index: usize,
    delay_ms: u64,
    detail: impl AsRef<str>,
) {
    warn!(
        "HLS origin manifest refresh retry scheduled: request_url={} attempt={} {} delay_ms={delay_ms}",
        hls_origin_log_value(origin_entry.url().as_str()),
        attempt_index + 1,
        detail.as_ref()
    );
}
