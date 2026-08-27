mod episode;
mod error;
mod fetch;
mod fingerprint;
mod http;
mod model;
mod quality;
mod recovery;
mod selection_log;

#[allow(unused_imports)]
pub use self::http::resolved_hls_manifest_request_url_from_input;
#[cfg(test)]
pub(crate) use self::{
    error::{classify_origin_manifest_status, OriginManifestStatusClass},
    http::{hls_manifest_redirect_host, retry_after_delay_ms},
    quality::{origin_highwater_policy_limit, score_hls_manifest_recovery_candidate},
    selection_log::ManifestRecoverySelectionLogPhase,
};
pub use self::{
    error::{
        HlsManifestAcceptanceRejectReason, HlsManifestCommitError, HlsManifestRecoveryUnavailableReason,
        HlsManifestRejectLogReason, OriginManifestFetchError,
    },
    fetch::{fetch_hls_origin_manifest_request, HlsOriginManifestFetchRequest},
    fingerprint::{
        deterministic_conflict_receipt_is_current, deterministic_conflict_receipt_matches,
        deterministic_timeline_conflict_from_rejection,
    },
    model::{
        fetched_effective_manifest_host, FetchedOriginManifest, HlsManifestCommitAcceptanceMode,
        HlsManifestContinuityMode, HlsManifestFetchSelection, HlsManifestOriginQuality, HlsManifestOriginQualityScore,
        HlsManifestOriginRelation, HlsManifestRecoveryCandidateScoreReport, HlsManifestSequenceRelation,
        HlsOriginManifestFetchContext, LiveHlsOriginEntry, RetryPolicy, MAX_HLS_MANIFEST_BYTES,
    },
    quality::{
        evaluate_manifest_origin_quality_with_mode, next_committed_origin_highwater,
        score_hls_manifest_candidate_for_selection_log,
    },
    recovery::retry_hls_origin_manifest_recovery_chain,
    selection_log::log_hls_manifest_initial_selected,
};
use self::{
    fetch::{current_time_millis, next_retry_delay_ms},
    model::{
        request_hls_session_idle_timeout_secs_from_config, DEFAULT_HLS_TARGET_DURATION_SECS,
        HLS_COMMITTED_CONTENT_ANCHOR_PROBE_LIMIT,
    },
};
#[cfg(test)]
use crate::timeline::TimelineMapError;

#[cfg(any(test, feature = "test-support"))]
pub async fn refresh_from_live_hls_entrypoint_with_retries(
    origin_entry: &LiveHlsOriginEntry,
    headers: &axum::http::HeaderMap,
    client: &reqwest::Client,
    no_redirect_client: &reqwest::Client,
    use_manual_redirects: bool,
    origin_manifest_timeout_ms: u64,
    retry_policy: &RetryPolicy,
) -> Result<FetchedOriginManifest, OriginManifestFetchError> {
    fetch::refresh_from_live_hls_entrypoint_with_retries(
        origin_entry,
        headers,
        client,
        no_redirect_client,
        use_manual_redirects,
        origin_manifest_timeout_ms,
        retry_policy,
    )
    .await
}

#[cfg(test)]
mod tests;
