use super::{
    apply_manifest_fetch_failure_signal, bind_refresh_request_to_app_state, build_manifest_refresh_timing,
    classify_manifest_fetch_failure, classify_origin_manifest_status, commit_fetched_manifest,
    fetch_and_commit_manifest_with_policy, fetch_hls_origin_manifest_request, fetched_manifest, manifest_body,
    manifest_hard_fetch_error, manifest_progress_from_highwater, mark_origin_refresh_started,
    mark_origin_refresh_started_with_outcome, no_delay_policy, post_refresh_live_manifest_snapshot,
    prepare_cross_host_baseline, record_committed_manifest_media_progress, record_committed_manifest_success,
    refresh_from_live_hls_entrypoint_with_retries, refresh_origin_work_generation_matches,
    refresh_session_with_origin_body, request_error_indicates_timeout, request_header_value, spawn_test_origin,
    test_acceptance_episode_timing, test_app_config, test_deterministic_timeline_conflict, test_origin_refresh_request,
    test_recovery_timing_policy, test_segment_repair_manager, test_session, three_segment_manifest_body,
    trigger_origin_refresh_sync, HlsLeaseCutoverTiming, HlsManifestAcceptanceTrigger,
    HlsManifestCommitProgressEvidence, HlsManifestCommitRequirement, HlsManifestFetchFailureKind,
    HlsManifestFetchFailureSignal, HlsManifestHttpResponseEvidence, HlsManifestProgress,
    HlsManifestRecoveryUnavailableReason, HlsManifestRejectLogReason, HlsManifestSequenceRelation,
    HlsOriginManifestFetchContext, HlsOriginManifestFetchRequest, HlsOriginRefreshTriggerOutcome,
    HlsPostRefreshAvailabilityAction, HlsPostRefreshAvailabilityReason, HlsTerminalCommitAcquisitionBudgetMs,
    HlsTerminalCommitPayload, HlsTerminalCommitRequest, HlsTerminalCommitWindow, HlsTerminalTailPreparationRequest,
    HlsTransitionMarginMs, LiveHlsOriginEntry, OriginManifestFetchError, OriginManifestStatusClass,
    OriginRefreshRequest, OriginRefreshState, RetryPolicy,
};
use crate::{
    media_reserve::{HlsLeaseReserveAvailabilityBasis, HlsLeaseReserveSnapshot},
    refresh::maybe_trigger_origin_refresh,
    terminal_tail::HlsLeasePlaybackMode,
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeasePendingDeadline, HlsBoundAccountAcquireErrorKind,
    HlsFreshManifestRequiredReason, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestAcceptanceDirective,
    HlsManifestAcceptanceExhaustionReason, HlsManifestCommitIdentity, HlsManifestDeliveryMode, HlsMapWorkerPool,
    HlsMediaContainer, HlsPlaybackFamilyKey, HlsProxyManager, HlsSegmentCache, HlsSegmentWorkerPool, HlsSession,
    HlsSessionKey, HlsSessionMode, HlsTerminalTailCompatibility, ProxySessionId, SegmentCacheStatus,
    SegmentFetchPriority, TimelineMapError, TransientPassthroughReason, TransientResourceKind,
};
use axum::http::{HeaderMap, StatusCode};
use shared::model::{HlsManifestRecoveryBurstLevel, HlsStripMode};
use std::{
    fmt::Write,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::RwLock;
use tuliprox_core::{
    model::{Config, HlsManifestRecoveryBurstConfig, StripConfig},
    utils::{
        content_coding::{ContentCoding, ContentCodingError},
        current_time_millis,
    },
};

#[test]
fn origin_refresh_failure_backoff_ramps_and_success_resets_counter() {
    let mut state = OriginRefreshState::default();

    state.mark_started(1_000);
    state.mark_failure(1_100);
    assert_eq!(state.consecutive_failures, 1);
    assert_eq!(state.last_error_at_ms, Some(1_100));
    assert_eq!(state.next_fetch_allowed_at_ms, 1_100);
    assert!(state.is_due(1_100));

    state.mark_started(1_200);
    state.mark_failure(1_300);
    assert_eq!(state.consecutive_failures, 2);
    assert_eq!(state.next_fetch_allowed_at_ms, 1_800);
    assert!(!state.is_due(1_799));
    assert!(state.is_due(1_800));

    state.mark_started(1_900);
    state.mark_failure(2_000);
    assert_eq!(state.consecutive_failures, 3);
    assert_eq!(state.next_fetch_allowed_at_ms, 3_000);

    state.mark_started(3_100);
    let success_timing = build_manifest_refresh_timing(Some(20_000), None, HlsManifestProgress::Advanced);
    assert_eq!(state.mark_success_with_timing(3_100, 3_200, success_timing), 10_000);
    assert_eq!(state.consecutive_failures, 0);
    assert_eq!(state.last_error_at_ms, None);
    assert_eq!(state.next_fetch_allowed_at_ms, 13_100);

    state.mark_started(13_100);
    state.mark_failure(13_200);
    assert_eq!(state.consecutive_failures, 1);
    assert_eq!(state.next_fetch_allowed_at_ms, 13_200);
}

#[test]
fn status_classification_matches_hls_retry_policy() {
    for status in [
        StatusCode::PROXY_AUTHENTICATION_REQUIRED,
        StatusCode::REQUEST_TIMEOUT,
        StatusCode::TOO_EARLY,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
    ] {
        assert_eq!(classify_origin_manifest_status(status), OriginManifestStatusClass::Retryable);
    }
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::GONE,
    ] {
        assert_eq!(classify_origin_manifest_status(status), OriginManifestStatusClass::PermanentFailure);
    }
}

pub(in crate::refresh::tests) type ManifestFetchFailureCase =
    (&'static str, OriginManifestFetchError, HlsManifestFetchFailureSignal);

pub(in crate::refresh::tests) fn manifest_response_failure_cases() -> Vec<ManifestFetchFailureCase> {
    use HlsManifestFetchFailureKind as Kind;
    use HlsManifestHttpResponseEvidence::{None as NoResponse, ValidResponse};

    vec![
        (
            "permanent status",
            OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND),
            HlsManifestFetchFailureSignal::hard(Kind::HttpStatus { status: StatusCode::NOT_FOUND }, ValidResponse),
        ),
        (
            "retryable status",
            OriginManifestFetchError::RetryableStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED, None),
            HlsManifestFetchFailureSignal::retryable(
                Kind::HttpStatus { status: StatusCode::PROXY_AUTHENTICATION_REQUIRED },
                ValidResponse,
            ),
        ),
        (
            "retry exhausted",
            OriginManifestFetchError::RetryExhausted,
            HlsManifestFetchFailureSignal::retryable(Kind::RetryExhausted, NoResponse),
        ),
        (
            "commit generation exhausted",
            OriginManifestFetchError::CommitGenerationExhausted,
            HlsManifestFetchFailureSignal::discarded(Kind::CommitGenerationExhausted),
        ),
        (
            "recovery unavailable after response",
            OriginManifestFetchError::RecoveryUnavailable {
                reason: HlsManifestRecoveryUnavailableReason::NoEstablishedBindingAfterResponse,
            },
            HlsManifestFetchFailureSignal::retryable(Kind::AcceptanceConflict, ValidResponse),
        ),
        (
            "recovery binding superseded",
            OriginManifestFetchError::RecoveryUnavailable {
                reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded,
            },
            HlsManifestFetchFailureSignal::discarded(Kind::Superseded),
        ),
        (
            "deterministic acceptance conflict",
            OriginManifestFetchError::DeterministicTimelineConflict(Box::new(test_deterministic_timeline_conflict())),
            HlsManifestFetchFailureSignal::retryable(Kind::AcceptanceConflict, ValidResponse),
        ),
        (
            "non-retryable status",
            OriginManifestFetchError::NonRetryableStatus(StatusCode::IM_A_TEAPOT),
            HlsManifestFetchFailureSignal::hard(Kind::HttpStatus { status: StatusCode::IM_A_TEAPOT }, ValidResponse),
        ),
        (
            "request timeout wording",
            OriginManifestFetchError::Request("Request timed out and no retries left".to_string()),
            HlsManifestFetchFailureSignal::retryable(Kind::Timeout, NoResponse),
        ),
        (
            "transport, connect, or DNS failure",
            OriginManifestFetchError::Request("dns lookup failed".to_string()),
            HlsManifestFetchFailureSignal::retryable(Kind::Transport, NoResponse),
        ),
        (
            "redirect failure",
            OriginManifestFetchError::Redirect("redirect location invalid".to_string()),
            HlsManifestFetchFailureSignal::retryable(Kind::Redirect, ValidResponse),
        ),
        (
            "timeout",
            OriginManifestFetchError::Timeout,
            HlsManifestFetchFailureSignal::retryable(Kind::Timeout, NoResponse),
        ),
    ]
}

pub(in crate::refresh::tests) fn manifest_content_failure_cases() -> Vec<ManifestFetchFailureCase> {
    use HlsManifestFetchFailureKind as Kind;
    use HlsManifestHttpResponseEvidence::{None as NoResponse, ValidResponse};

    vec![
        (
            "retryable provider acquire",
            OriginManifestFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::WaitTimedOut),
            HlsManifestFetchFailureSignal::retryable(
                Kind::ProviderAcquire { kind: HlsBoundAccountAcquireErrorKind::WaitTimedOut },
                NoResponse,
            ),
        ),
        (
            "hard provider acquire",
            OriginManifestFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Expired),
            HlsManifestFetchFailureSignal::hard(
                Kind::ProviderAcquire { kind: HlsBoundAccountAcquireErrorKind::Expired },
                NoResponse,
            ),
        ),
        (
            "invalid content-coding header",
            OriginManifestFetchError::ContentCoding(ContentCodingError::InvalidHeader),
            HlsManifestFetchFailureSignal::hard(Kind::InvalidContentCodingHeader, ValidResponse),
        ),
        (
            "unsupported content coding",
            OriginManifestFetchError::ContentCoding(ContentCodingError::Unsupported("unknown".to_string())),
            HlsManifestFetchFailureSignal::hard(Kind::UnsupportedContentCoding, ValidResponse),
        ),
        (
            "encoded partial content",
            OriginManifestFetchError::ContentCoding(ContentCodingError::EncodedPartialContent),
            HlsManifestFetchFailureSignal::hard(Kind::EncodedPartialContent, ValidResponse),
        ),
        (
            "content prefix read",
            OriginManifestFetchError::ContentCoding(ContentCodingError::PrefixRead(io::Error::other(
                "prefix read failed",
            ))),
            HlsManifestFetchFailureSignal::retryable(Kind::ContentPrefixRead, ValidResponse),
        ),
        (
            "content decoding",
            OriginManifestFetchError::ContentDecoding { coding: ContentCoding::Gzip },
            HlsManifestFetchFailureSignal::retryable(
                Kind::ContentDecoding { coding: ContentCoding::Gzip },
                ValidResponse,
            ),
        ),
        (
            "decoded body limit",
            OriginManifestFetchError::DecodedBodyLimitExceeded { limit: 1_024 },
            HlsManifestFetchFailureSignal::hard(Kind::DecodedBodyLimit, ValidResponse),
        ),
        (
            "invalid UTF-8",
            OriginManifestFetchError::InvalidUtf8 { valid_up_to: 7, error_len: Some(1) },
            HlsManifestFetchFailureSignal::hard(Kind::InvalidUtf8, ValidResponse),
        ),
    ]
}

#[test]
fn manifest_fetch_failures_have_exhaustive_typed_signals() {
    for (label, error, expected) in
        manifest_response_failure_cases().into_iter().chain(manifest_content_failure_cases())
    {
        assert_eq!(classify_manifest_fetch_failure(&error), expected, "unexpected signal: {label}");
        assert_eq!(manifest_hard_fetch_error(&error), expected.is_hard(), "unexpected disposition: {label}");
    }
}

#[test]
fn manifest_hard_fetch_error_matches_permanent_and_non_retryable_status_only() {
    assert!(manifest_hard_fetch_error(&OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND)));
    assert!(manifest_hard_fetch_error(&OriginManifestFetchError::NonRetryableStatus(StatusCode::IM_A_TEAPOT)));
    assert!(!manifest_hard_fetch_error(&OriginManifestFetchError::RetryableStatus(
        StatusCode::TOO_MANY_REQUESTS,
        None,
    )));
    assert!(!manifest_hard_fetch_error(&OriginManifestFetchError::Timeout));
    assert!(!manifest_hard_fetch_error(&OriginManifestFetchError::ProviderUnavailable(
        HlsBoundAccountAcquireErrorKind::WaitTimedOut,
    )));
    assert!(manifest_hard_fetch_error(&OriginManifestFetchError::ProviderUnavailable(
        HlsBoundAccountAcquireErrorKind::Expired,
    )));
}

#[test]
fn failure_signal_advances_response_clock_only_with_http_evidence() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);

    for error in [
        OriginManifestFetchError::Timeout,
        OriginManifestFetchError::Request("connection refused".to_string()),
        OriginManifestFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::WaitTimedOut),
    ] {
        apply_manifest_fetch_failure_signal(&mut session, &error, 100);
        assert_eq!(session.origin_control.last_origin_response_at_ms, None);
        assert_eq!(
            session.origin_control.path_condition,
            super::super::super::origin_progress::HlsOriginPathCondition::RetryableFetchFailure
        );
    }

    apply_manifest_fetch_failure_signal(
        &mut session,
        &OriginManifestFetchError::RetryableStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED, None),
        200,
    );
    assert_eq!(session.origin_control.last_origin_response_at_ms, Some(200));

    apply_manifest_fetch_failure_signal(
        &mut session,
        &OriginManifestFetchError::Redirect("redirect location invalid".to_string()),
        250,
    );
    assert_eq!(session.origin_control.last_origin_response_at_ms, Some(250));

    apply_manifest_fetch_failure_signal(
        &mut session,
        &OriginManifestFetchError::ContentDecoding { coding: ContentCoding::Brotli },
        300,
    );
    assert_eq!(session.origin_control.last_origin_response_at_ms, Some(300));

    let hard_action = apply_manifest_fetch_failure_signal(
        &mut session,
        &OriginManifestFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Expired),
        400,
    );
    assert_eq!(session.origin_control.last_origin_response_at_ms, Some(300));
    assert_eq!(
        session.origin_control.path_condition,
        super::super::super::origin_progress::HlsOriginPathCondition::HardFetchFailure
    );
    assert_eq!(
        session.fresh_manifest_commit_required,
        Some(HlsFreshManifestRequiredReason::PreviousHardManifestFailure)
    );
    assert_eq!(
        hard_action,
        HlsPostRefreshAvailabilityAction::Reevaluate {
            reason: HlsPostRefreshAvailabilityReason::HardManifestFailure,
            origin_progress_generation: session.origin_control.progress_generation,
            media_readiness_generation: session.activity.media_readiness_generation,
        }
    );
}

#[test]
fn invalidated_origin_work_generation_rejects_late_completion_without_counting_failure() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let started_generation = session.start_origin_work();
    let mut refresh_state = OriginRefreshState::default();
    refresh_state.mark_started(100);

    assert!(refresh_origin_work_generation_matches(&session, Some(started_generation)));
    session.invalidate_queued_origin_work();
    assert!(!refresh_origin_work_generation_matches(&session, Some(started_generation)));
    assert!(refresh_origin_work_generation_matches(&session, None));

    refresh_state.mark_invalidated(200);
    assert!(!refresh_state.in_flight);
    assert_eq!(refresh_state.last_fetch_finished_at_ms, Some(200));
    assert_eq!(refresh_state.consecutive_failures, 0);
    assert_eq!(refresh_state.last_error_at_ms, None);
}

#[test]
fn request_error_timeout_detection_matches_global_helper_wording() {
    assert!(request_error_indicates_timeout("Request timed out and no retries left"));
    assert!(request_error_indicates_timeout("idle timeout while trying provider://demo"));
    assert!(!request_error_indicates_timeout("Request error: error sending request"));
}

#[test]
fn manifest_reject_log_reason_preserves_timeline_mapping_error() {
    assert_eq!(
        HlsManifestRejectLogReason::from(TimelineMapError::UnsupportedSegmentExtension).status_label(),
        "unsupported-segment-extension"
    );
    assert_eq!(
        HlsManifestRejectLogReason::from(TimelineMapError::ProxyMapIdOverflow).status_label(),
        "proxy-map-id-overflow"
    );
}

#[test]
fn empty_refresh_rampdown_halves_until_one_second() {
    let mut state = OriginRefreshState::default();
    let timing = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Unchanged);

    state.mark_started(0);
    assert_eq!(state.mark_success_with_timing(0, 100, timing), 3_000);
    assert_eq!(state.consecutive_empty_refreshes, 1);
    assert_eq!(state.next_fetch_allowed_at_ms, 3_000);

    state.mark_started(3_000);
    assert_eq!(state.mark_success_with_timing(3_000, 3_100, timing), 1_500);
    assert_eq!(state.consecutive_empty_refreshes, 2);
    assert_eq!(state.next_fetch_allowed_at_ms, 4_500);

    state.mark_started(4_500);
    assert_eq!(state.mark_success_with_timing(4_500, 4_600, timing), 1_000);
    assert_eq!(state.consecutive_empty_refreshes, 3);
    assert_eq!(state.next_fetch_allowed_at_ms, 5_500);

    state.mark_started(5_500);
    assert_eq!(state.mark_success_with_timing(5_500, 5_600, timing), 1_000);
    assert_eq!(state.consecutive_empty_refreshes, 4);
    assert_eq!(state.next_fetch_allowed_at_ms, 6_500);
}

#[test]
fn advanced_or_rollover_refresh_resets_empty_refresh_counter() {
    let mut state = OriginRefreshState::default();
    let unchanged = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Unchanged);
    let advanced = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Advanced);
    let rollover = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Rollover);

    state.mark_started(0);
    assert_eq!(state.mark_success_with_timing(0, 100, unchanged), 3_000);
    state.mark_started(3_000);
    assert_eq!(state.mark_success_with_timing(3_000, 3_100, unchanged), 1_500);
    assert_eq!(state.consecutive_empty_refreshes, 2);

    state.mark_started(4_500);
    assert_eq!(state.mark_success_with_timing(4_500, 4_600, advanced), 6_000);
    assert_eq!(state.consecutive_empty_refreshes, 0);
    assert_eq!(state.next_fetch_allowed_at_ms, 10_500);

    state.mark_started(10_500);
    assert_eq!(state.mark_success_with_timing(10_500, 10_600, unchanged), 3_000);
    assert_eq!(state.consecutive_empty_refreshes, 1);

    state.mark_started(13_500);
    assert_eq!(state.mark_success_with_timing(13_500, 13_600, rollover), 6_000);
    assert_eq!(state.consecutive_empty_refreshes, 0);
    assert_eq!(state.next_fetch_allowed_at_ms, 19_500);
}

#[test]
fn failure_backoff_does_not_increment_empty_refresh_counter() {
    let mut state = OriginRefreshState::default();
    let unchanged = build_manifest_refresh_timing(None, Some(12_000), HlsManifestProgress::Unchanged);

    state.mark_started(0);
    assert_eq!(state.mark_success_with_timing(0, 100, unchanged), 3_000);
    state.mark_started(3_000);
    state.mark_failure(3_100);

    assert_eq!(state.consecutive_failures, 1);
    assert_eq!(state.consecutive_empty_refreshes, 1);
}

#[test]
fn manifest_progress_tracks_highwater_advancement() {
    assert_eq!(
        manifest_progress_from_highwater(None, Some(10), HlsManifestSequenceRelation::NoPreviousHighwater),
        HlsManifestProgress::Advanced
    );
    assert_eq!(
        manifest_progress_from_highwater(Some(10), Some(11), HlsManifestSequenceRelation::Next),
        HlsManifestProgress::Advanced
    );
    assert_eq!(
        manifest_progress_from_highwater(Some(10), Some(10), HlsManifestSequenceRelation::Same),
        HlsManifestProgress::Unchanged
    );
    assert_eq!(
        manifest_progress_from_highwater(Some(10), Some(9), HlsManifestSequenceRelation::Backward),
        HlsManifestProgress::Unchanged
    );
    assert_eq!(
        manifest_progress_from_highwater(Some(10), Some(1), HlsManifestSequenceRelation::RolloverCandidate),
        HlsManifestProgress::Rollover
    );
    assert_eq!(
        manifest_progress_from_highwater(None, None, HlsManifestSequenceRelation::NoOriginHighwater),
        HlsManifestProgress::Unchanged
    );
}

#[test]
fn hls_cutover_policy_only_advanced_and_rollover_commits_advance_progress_generation() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let initial_generation = session.origin_control.progress_generation;

    record_committed_manifest_media_progress(
        &mut session.origin_control,
        HlsManifestCommitProgressEvidence::CacheTimeline(build_manifest_refresh_timing(
            None,
            Some(4_000),
            HlsManifestProgress::Unchanged,
        )),
        Some(4),
        1_000,
    );
    assert_eq!(session.origin_control.progress_generation, initial_generation);
    assert_eq!(session.origin_control.last_media_progress_at_ms, None);

    record_committed_manifest_media_progress(
        &mut session.origin_control,
        HlsManifestCommitProgressEvidence::Transient(build_manifest_refresh_timing(
            None,
            Some(4_000),
            HlsManifestProgress::Advanced,
        )),
        Some(4),
        1_500,
    );
    assert_eq!(session.origin_control.progress_generation, initial_generation);
    assert_eq!(session.origin_control.last_media_progress_at_ms, None);

    record_committed_manifest_media_progress(
        &mut session.origin_control,
        HlsManifestCommitProgressEvidence::CacheTimeline(build_manifest_refresh_timing(
            None,
            Some(4_000),
            HlsManifestProgress::Advanced,
        )),
        Some(4),
        2_000,
    );
    assert_eq!(session.origin_control.progress_generation, initial_generation.saturating_add(1));
    assert_eq!(session.origin_control.last_media_progress_at_ms, Some(2_000));

    record_committed_manifest_media_progress(
        &mut session.origin_control,
        HlsManifestCommitProgressEvidence::CacheTimeline(build_manifest_refresh_timing(
            None,
            Some(4_000),
            HlsManifestProgress::Rollover,
        )),
        Some(4),
        3_000,
    );
    assert_eq!(session.origin_control.progress_generation, initial_generation.saturating_add(2));
    assert_eq!(session.origin_control.last_media_progress_at_ms, Some(3_000));
}

#[test]
fn hls_cutover_policy_cache_timeline_progress_keeps_recovery_success_bookkeeping() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let burst_plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
    session.origin_control.begin_acceptance_episode(
        1_000,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &test_acceptance_episode_timing(1_000, burst_plan),
    );
    session.origin_control.acceptance_episode.as_mut().expect("acceptance episode").complete();
    session.origin_refresh.consecutive_empty_refreshes = 2;
    session.origin_refresh.mark_started(10_000);

    let (bookkeeping_timing, applied_interval_ms) = record_committed_manifest_success(
        &mut session,
        HlsManifestCommitProgressEvidence::CacheTimeline(build_manifest_refresh_timing(
            None,
            Some(12_000),
            HlsManifestProgress::Advanced,
        )),
        10_000,
        10_100,
    );

    assert_eq!(bookkeeping_timing.progress, HlsManifestProgress::Advanced);
    assert_eq!(applied_interval_ms, 6_000);
    assert_eq!(session.origin_refresh.consecutive_empty_refreshes, 0);
    assert!(session.origin_control.acceptance_episode.is_none());
    assert_eq!(session.origin_control.recovery_samples.p95_ms(), Some(9_100));
}

#[test]
fn refresh_commit_trims_forward_manifest_with_published_stale_resource_prefix() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let request = test_origin_refresh_request(test_session());
    let baseline = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:471\n\
         #EXTINF:4,\n471.ts\n#EXTINF:4,\n472.ts\n#EXTINF:4,\n473.ts\n\
         #EXTINF:4,\n474.ts\n#EXTINF:4,\n475.ts\n#EXTINF:4,\n476.ts\n",
    );
    let replay_then_new = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:476\n\
         #EXTINF:4,\n474.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n",
    );

    commit_fetched_manifest(&mut session, &baseline, &request, 100).expect("baseline refresh commits");
    for segment in session.segments.values_mut() {
        segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 101 };
    }
    session.render_and_store_manifest(101).expect("baseline publishes");
    commit_fetched_manifest(&mut session, &replay_then_new, &request, 200)
        .expect("stale prefix is safely trimmed by refresh commit");

    assert_eq!(session.proxy_next_seq, Some(8));
    assert_eq!(session.publishable_origin_head_proxy_seq, Some(0));
    assert!(session.segments.get(&6).expect("first genuine media").discontinuity_before);
    for proxy_seq in [6_u64, 7] {
        session.segments.get_mut(&proxy_seq).expect("new segment").status =
            SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 201 };
    }
    let rendered = session.render_and_store_manifest(201).expect("forward refresh renders without replay");
    assert_eq!(rendered.first_proxy_seq, 2);
    assert!(rendered.body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,"));
    let identities = rendered
        .segment_proxy_seqs
        .iter()
        .filter_map(|proxy_seq| session.segments.get(proxy_seq)?.media_resource_identity())
        .collect::<Vec<_>>();
    assert!(identities
        .iter()
        .enumerate()
        .all(|(index, identity)| { identities[..index].iter().all(|previous| !previous.matches(*identity)) }));
}

#[tokio::test]
async fn background_replay_failure_registers_post_refresh_reevaluation_without_client_demand() {
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let now_ms = current_time_millis();
    let (session, _) = ctx
        .hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "post-refresh-conflict"), b"secret", now_ms)
        .await;
    let conflicting_body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
         #EXTINF:4,\n490.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n491.ts\n";
    let server = spawn_test_origin(Arc::new(move |_path| (200, Vec::new(), conflicting_body.to_string()))).await;
    let manifest_url = format!("{}/live/user/pass/12345.m3u8", server.base_url);
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), ctx);
    request.origin_entry = LiveHlsOriginEntry::parse(&manifest_url).expect("local replay origin entry");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.now_ms = now_ms;
    let mut baseline = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:480\n\
         #EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n#EXTINF:4,\n482.ts\n",
    );
    baseline.final_manifest_url = manifest_url.clone();
    baseline.resolved_request_url = manifest_url;
    baseline.redirect_host = Some("127.0.0.1".to_string());
    {
        let mut session = session.write().await;
        commit_fetched_manifest(&mut session, &baseline, &request, now_ms).expect("baseline commits");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: now_ms };
        }
        session.render_and_store_manifest(now_ms).expect("baseline publishes");
        session.segments.get_mut(&1).expect("deferred boundary segment").status =
            SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Prefetch, deferred_at_ms: now_ms };
    }
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("post-refresh-conflict-lease".to_string());
    let mut lease = HlsAccessLease::pending(
        lease_id,
        HlsPlaybackFamilyKey::new("post-refresh-user", "post-refresh-client"),
        proxy_session_id,
        "post-refresh-user".to_string(),
        "post-refresh-token".to_string(),
        1,
        "post-refresh-stream".to_string(),
        1,
        now_ms,
        60_000,
    );
    lease.state = crate::HlsAccessLeaseState::Activated;
    lease.active_until_ms = Some(now_ms.saturating_add(60_000));
    lease.pending_deadline = None;
    lease.last_manifest_snapshot = Some(post_refresh_live_manifest_snapshot());
    ctx.hls_proxy.prepare_access_lease(lease).await;

    assert!(trigger_origin_refresh_sync(request).await);

    assert_eq!(
        session.read().await.origin_control.path_condition,
        super::super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict
    );
    assert_eq!(ctx.hls_proxy.availability_reevaluations().owner_count(), 1);
}

#[tokio::test]
async fn background_success_does_not_schedule_failure_reevaluation() {
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let now_ms = current_time_millis();
    let (session, _) = ctx
        .hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "post-refresh-success"), b"secret", now_ms)
        .await;
    let server = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), ctx);
    request.origin_entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
        .expect("local successful origin entry");
    request.now_ms = now_ms;

    assert!(trigger_origin_refresh_sync(request).await);

    assert!(session.read().await.origin_seq_highwater.is_some());
    assert_eq!(ctx.hls_proxy.availability_reevaluations().owner_count(), 0);
}

pub(in crate::refresh::tests) async fn assert_requirement_set_after_refresh_start_survives_fresh_commit(
    replacement_reason: HlsFreshManifestRequiredReason,
) {
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(1_000);
        session.last_effective_manifest_host = Some("origin.example.com".to_string());
        session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
    }
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
    };
    assert!(mark_origin_refresh_started(&mut request, 100).await);

    let fetched =
        fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nseg10.ts\n");
    let result = {
        let mut session = session.write().await;
        session.require_fresh_manifest_commit(replacement_reason);
        commit_fetched_manifest(&mut session, &fetched, &request, 200)
    };

    assert!(result.is_ok());
    assert_eq!(session.read().await.fresh_manifest_commit_required, Some(replacement_reason));
}

#[tokio::test]
async fn fresh_commit_preserves_new_or_same_reason_requirement_set_after_refresh_start() {
    assert_requirement_set_after_refresh_start_survives_fresh_commit(
        HlsFreshManifestRequiredReason::ExpiredRevalidation,
    )
    .await;
    assert_requirement_set_after_refresh_start_survives_fresh_commit(
        HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
    )
    .await;
}

#[tokio::test]
async fn concurrent_maybe_trigger_origin_refresh_starts_singleflight_once() {
    let session = test_session();
    let entry = LiveHlsOriginEntry::parse("http://127.0.0.1:9/live/user/pass/12345.m3u8").expect("valid origin entry");
    let client = reqwest::Client::new();
    let no_redirect_client =
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client builds");
    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client,
        no_redirect_client,
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 1,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 3 },
        retry_policy: RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 },
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        disabled_headers: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    };

    let mut handles = Vec::new();
    for _ in 0..8 {
        let request = request.clone();
        handles.push(tokio::spawn(async move { maybe_trigger_origin_refresh(request).await }));
    }

    let started = futures::future::join_all(handles)
        .await
        .into_iter()
        .filter(|result| result.as_ref().is_ok_and(|started| *started))
        .count();
    assert_eq!(started, 1);
}

#[tokio::test]
async fn fresh_manifest_commit_bypasses_refresh_debounce() {
    let session = test_session();
    session.write().await.origin_refresh.next_fetch_allowed_at_ms = 10_000;
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };

    assert!(mark_origin_refresh_started(&mut request, 1_000).await);
    assert!(session.read().await.origin_refresh.in_flight);
}

#[tokio::test]
async fn committed_manifest_refresh_still_obeys_debounce() {
    let session = test_session();
    session.write().await.origin_refresh.next_fetch_allowed_at_ms = 10_000;
    let mut request = test_origin_refresh_request(Arc::clone(&session));

    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 1_000).await,
        HlsOriginRefreshTriggerOutcome::DebouncedUntil { retry_at_ms: 10_000 }
    );
    assert!(!session.read().await.origin_refresh.in_flight);
}

pub(in crate::refresh::tests) fn cutover_live_manifest_snapshot() -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1_000),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: 1_000,
        first_proxy_seq: 40,
        last_proxy_seq: 41,
        visible_segments: Arc::from([
            HlsLeaseManifestSegment {
                proxy_seq: 40,
                duration_ms: 4_000,
                uri: "/live/40.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 41,
                duration_ms: 4_000,
                uri: "/live/41.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
        ]),
        discontinuity_sequence: 0,
        target_duration_ms: 4_000,
        playlist_duration_ms: 8_000,
        last_visible_media_end_ms: 8_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

pub(in crate::refresh::tests) async fn assert_recovery_supersedes_terminal_preparation(
    hls_proxy: &HlsProxyManager,
    session: &Arc<RwLock<HlsSession>>,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    preparation: &super::super::super::lease::HlsTerminalTailPreparation,
) {
    assert_eq!(
        hls_proxy.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
            session,
            lease_id,
            proxy_session_id,
            preparation,
            now_ms: 2_100,
            payload: HlsTerminalCommitPayload::Unavailable(HlsTerminalTailCompatibility::MissingAsset),
            asset_revision_guard:
                super::super::super::terminal_commit::HlsTerminalAssetRevisionGuard::matching_for_test(None),
        }),
        super::super::super::terminal_commit::HlsTerminalCommitOutcome::RecoveryCommitted
    );
    let lease = hls_proxy
        .access_lease_response_snapshot(lease_id, proxy_session_id, 2_100)
        .await
        .expect("recovered lease remains live");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert!(!session.read().await.has_terminal_tail_protections());
}

pub(in crate::refresh::tests) async fn prepare_failed_acceptance_episode(session: &Arc<RwLock<HlsSession>>) -> u64 {
    let mut session = session.write().await;
    let burst_plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    session.origin_control.begin_acceptance_episode(
        1_000,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &test_acceptance_episode_timing(1_000, burst_plan),
    );
    let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
    episode.record_full_burst();
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::AllFailed);
    episode.hold_after_uncommitted_burst(None, Some(2_000));
    session.origin_control.progress_generation
}

#[tokio::test]
async fn hls_cutover_policy_advanced_commit_supersedes_preparation_and_keeps_lease_live() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) =
        hls_proxy.get_or_create_session_with_outcome(HlsSessionKey::new(1, "12345"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    hls_proxy.access_leases().write().await.prepare_access_lease(HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new("user", "client"),
        proxy_session_id.clone(),
        "user".to_string(),
        "session-token".to_string(),
        1,
        "12345".to_string(),
        12345,
        1_000,
        60_000,
    ));
    let publication_guard = hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, 1_000)
        .await
        .expect("live manifest publication");
    assert!(hls_proxy
        .commit_access_lease_manifest_publication(
            &lease_id,
            &proxy_session_id,
            publication_guard,
            cutover_live_manifest_snapshot(),
            1_000,
        )
        .await
        .is_committed());
    let prepared_progress_generation = prepare_failed_acceptance_episode(&session).await;
    let transition_margin = HlsTransitionMarginMs::from_millis(4_000);
    let guaranteed_reserve_ms = transition_margin
        .as_millis()
        .saturating_add(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis());
    let reserve = HlsLeaseReserveSnapshot {
        availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
        guaranteed_media_horizon_ms: 8_000_u64.saturating_add(guaranteed_reserve_ms),
        conservative_playback_position_ms: 8_000,
        guaranteed_reserve_ms,
        initial_hidden_ready_duration_ms: 0,
        transition_margin,
        key_readiness_valid_until_ms: None,
        recovery_required: true,
        cutover_required: false,
    };
    let cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(2_000, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
    let preparation = hls_proxy
        .prepare_access_lease_terminal_tail(HlsTerminalTailPreparationRequest {
            lease_id: &lease_id,
            proxy_session_id: &proxy_session_id,
            manifest_snapshot_generation: 1,
            cursor_generation: 0,
            reserve,
            cutover_timing,
            commit_window: HlsTerminalCommitWindow::AcquisitionOpen,
            now_ms: 2_000,
            origin_progress_generation: prepared_progress_generation,
            media_readiness_generation: 0,
            last_media_progress_at_ms: None,
        })
        .await
        .expect("exhausted acceptance permits terminal preparation");

    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
    let fetched = fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:42\n#EXTINF:4.0,\n42.ts\n");
    {
        let mut session = session.write().await;
        let (progress_evidence, _, _) =
            commit_fetched_manifest(&mut session, &fetched, &request, 2_100).expect("recovery manifest commits");
        assert_eq!(progress_evidence.refresh_timing().progress, HlsManifestProgress::Advanced);
        assert_eq!(session.origin_control.progress_generation, prepared_progress_generation.saturating_add(1));
        assert_eq!(session.origin_control.last_media_progress_at_ms, Some(2_100));
    }

    assert_recovery_supersedes_terminal_preparation(&hls_proxy, &session, &proxy_session_id, &lease_id, &preparation)
        .await;
}

#[tokio::test]
async fn successful_manifest_commit_shortens_pending_leases_without_response_path() {
    let session = test_session();
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let now_ms = super::super::current_time_millis();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("user", "client"),
            proxy_session_id.clone(),
            "user".to_string(),
            "session-token".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            90_000,
        ))
        .await;
    let server = spawn_test_origin(Arc::new(|_path| {
        (200, Vec::new(), "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string())
    }))
    .await;
    let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
        .expect("valid origin entry");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.origin_entry = entry;
    request.now_ms = now_ms;

    assert!(Box::pin(trigger_origin_refresh_sync(request)).await);

    let lease = hls_proxy
        .access_leases()
        .write()
        .await
        .response_snapshot(&lease_id, &proxy_session_id, super::super::current_time_millis())
        .expect("pending lease should remain available");
    let Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms }) = lease.pending_deadline else {
        panic!("pending lease should be shortened to follow-up");
    };
    assert!(deadline_ms < now_ms.saturating_add(90_000));
    assert!(deadline_ms <= super::super::current_time_millis().saturating_add(10_000));
}

#[tokio::test]
async fn endlist_only_manifest_commits_complete_body_with_consistent_finalized_state() {
    let mut origin_body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n".to_string();
    for sequence in 1..=8 {
        let _ = write!(origin_body, "#EXTINF:4.0,\n{sequence}.ts\n");
    }
    origin_body.push_str("#EXT-X-ENDLIST\n");
    let origin_body: &'static str = Box::leak(origin_body.into_boxed_str());
    let session = refresh_session_with_origin_body(origin_body).await;
    let session = session.read().await;
    let body = session.transient.last_manifest_body.as_ref().expect("finalized transient body");

    assert!(matches!(
        session.mode,
        HlsSessionMode::TransientPassthrough {
            reason: TransientPassthroughReason::UnsupportedTag { ref tag }
        } if tag == "#EXT-X-ENDLIST"
    ));
    let stored_body_lifecycle = tuliprox_parser::hls::origin_manifest::parse_manifest_semantics(body).lifecycle();
    assert_eq!(session.transient.last_manifest_finalized(), stored_body_lifecycle.is_finalized());
    assert_eq!(
        session.transient.last_manifest_window_policy(),
        tuliprox_parser::hls::origin_manifest::HlsManifestWindowPolicy::PreserveFullManifest
    );
    assert_eq!(session.transient.current_manifest_resource_ids().len(), 8);
    assert_eq!(body.matches("/r/").count(), 8);
    assert!(body.contains("#EXT-X-ENDLIST"));
    assert!(stored_body_lifecycle.is_finalized());
}

#[tokio::test]
async fn compatible_aes_128_manifest_commits_normal_timeline_with_opaque_key_resource() {
    let session = refresh_session_with_origin_body(
        "#EXTM3U\n#EXT-X-VERSION:5\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"origin-key.bin\",IV=0x00000000000000000000000000000001,KEYFORMAT=\"identity\",KEYFORMATVERSIONS=\"1\"\n\
         #EXTINF:4,\n1.ts\n#EXTINF:4,\n2.ts\n#EXTINF:4,\n3.ts\n#EXTINF:4,\n4.ts\n#EXTINF:4,\n5.ts\n#EXTINF:4,\n6.ts\n",
    )
    .await;
    let session = session.read().await;

    assert_eq!(session.mode, HlsSessionMode::NormalCacheTimeline);
    assert_eq!(session.transient.resources.len(), 1);
    assert!(session.transient.resources.values().all(|resource| resource.kind == TransientResourceKind::Key));
    assert!(session.segments.values().all(|segment| {
        segment.encryption.as_ref().is_some_and(|encryption| {
            encryption.resource_extension == "bin"
                && session.transient.resources.contains_key(&encryption.resource_id)
                && !encryption.resource_id.0.contains("origin-key.bin")
        })
    }));
    assert!(
        session.last_rendered_manifest.is_none(),
        "a refresh fixture without a usable lease must not publish unavailable media"
    );
}

#[tokio::test]
async fn cold_recovery_directive_is_suppressed_until_baseline() {
    let server = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;

    assert!(trigger_origin_refresh_sync(request).await);

    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let manifest_requests =
        server.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, 1);
    assert_ne!(manifest_requests, plan.total_candidates());
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(0));
    assert!(session.origin_control.manifest_origin_binding.is_some());
    assert!(session.origin_control.acceptance_episode.is_none());
}

#[tokio::test]
async fn shared_initial_manifest_decoder_failure_retries_until_success() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let server = spawn_test_origin(Arc::new(move |_path| {
        if hits_for_handler.fetch_add(1, Ordering::SeqCst) < 2 {
            return (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string());
        }
        (200, Vec::new(), manifest_body())
    }))
    .await;
    let origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    let context = HlsOriginManifestFetchContext {
        app_config: test_app_config(),
        session: test_session(),
        origin_entry,
        headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("no-redirect client"),
        use_manual_redirects: false,
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        retry_policy: no_delay_policy(),
        recovery_timing_policy: test_recovery_timing_policy(2_000),
        acceptance_timing_seed: None,
    };

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("shared initial manifest should retry decoder failures");

    assert_eq!(fetched.body, manifest_body());
    assert_eq!(fetched.attempts, 3);
    assert_eq!(server.requests.lock().await.len(), 3);
    assert!(server
        .raw_requests
        .lock()
        .await
        .iter()
        .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
}

#[tokio::test]
async fn shared_initial_manifest_waits_for_next_attempt_base_delay() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let server = spawn_test_origin(Arc::new(move |_path| {
        if hits_for_handler.fetch_add(1, Ordering::SeqCst) == 0 {
            return (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string());
        }
        (200, Vec::new(), manifest_body())
    }))
    .await;
    let origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    let context = HlsOriginManifestFetchContext {
        app_config: test_app_config(),
        session: test_session(),
        origin_entry,
        headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("no-redirect client"),
        use_manual_redirects: false,
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        retry_policy: RetryPolicy { delays_ms: [0, 100, 250, 500, 750], jitter_max_ms: 0 },
        recovery_timing_policy: test_recovery_timing_policy(2_000),
        acceptance_timing_seed: None,
    };
    let started_at = std::time::Instant::now();

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("second logical attempt should succeed after its base delay");

    assert_eq!(fetched.attempts, 2);
    assert_eq!(server.requests.lock().await.len(), 2);
    assert!(started_at.elapsed() >= std::time::Duration::from_millis(100));
}

#[tokio::test]
async fn shared_initial_manifest_decoder_failures_stop_at_attempt_budget() {
    let server = spawn_test_origin(Arc::new(|_path| {
        (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string())
    }))
    .await;
    let origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    let retry_policy = no_delay_policy();
    let expected_attempts = retry_policy.attempt_count();
    let context = HlsOriginManifestFetchContext {
        app_config: test_app_config(),
        session: test_session(),
        origin_entry,
        headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("no-redirect client"),
        use_manual_redirects: false,
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        retry_policy,
        recovery_timing_policy: test_recovery_timing_policy(2_000),
        acceptance_timing_seed: None,
    };

    let error = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect_err("decoder failures must exhaust the logical attempt budget");

    assert!(matches!(error, OriginManifestFetchError::ContentDecoding { .. }));
    assert_eq!(server.requests.lock().await.len(), expected_attempts);
    assert!(server
        .raw_requests
        .lock()
        .await
        .iter()
        .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
}

#[tokio::test]
async fn shared_refresh_metrics_use_successful_manifest_attempt_count() {
    let manifest_hits = Arc::new(AtomicUsize::new(0));
    let manifest_hits_for_handler = Arc::clone(&manifest_hits);
    let server = spawn_test_origin(Arc::new(move |path| {
        if path != "/live/user/pass/12345.m3u8" {
            return (404, Vec::new(), String::new());
        }
        if manifest_hits_for_handler.fetch_add(1, Ordering::SeqCst) < 2 {
            return (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string());
        }
        (200, Vec::new(), manifest_body())
    }))
    .await;
    let session = test_session();
    let mut request = test_origin_refresh_request(session);
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    let metrics = Arc::clone(request.segment_worker_pool.metrics());

    assert!(Box::pin(trigger_origin_refresh_sync(request)).await);

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.refresh_started, 1);
    assert_eq!(snapshot.refresh_completed, 1);
    assert_eq!(snapshot.refresh_retried, 2);
    assert_eq!(snapshot.refresh_failed, 0);
    let manifest_requests = server
        .raw_requests
        .lock()
        .await
        .iter()
        .filter(|request| request.lines().next().is_some_and(|line| line.contains(" /live/user/pass/12345.m3u8 ")))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(manifest_requests.len(), 3);
    assert!(manifest_requests
        .iter()
        .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
}

#[tokio::test]
async fn retryable_407_retries_until_success() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let server = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        if hit < 2 {
            return (407, Vec::new(), "retry".to_string());
        }
        (200, Vec::new(), manifest_body())
    }))
    .await;
    let entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");

    let fetched = refresh_from_live_hls_entrypoint_with_retries(
        &entry,
        &HeaderMap::new(),
        &reqwest::Client::new(),
        &reqwest::Client::new(),
        false,
        2_000,
        &no_delay_policy(),
    )
    .await
    .expect("refresh eventually succeeds");

    assert_eq!(fetched.attempts, 3);
    assert_eq!(server.requests.lock().await.len(), 3);
}

#[tokio::test]
async fn permanent_404_does_not_retry() {
    let server = spawn_test_origin(Arc::new(|_path| (404, Vec::new(), "missing".to_string()))).await;
    let entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");

    let err = refresh_from_live_hls_entrypoint_with_retries(
        &entry,
        &HeaderMap::new(),
        &reqwest::Client::new(),
        &reqwest::Client::new(),
        false,
        2_000,
        &no_delay_policy(),
    )
    .await
    .expect_err("404 is permanent");

    assert!(matches!(err, OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND)));
    assert_eq!(server.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn recovery_unavailable_after_valid_response_preserves_response_evidence() {
    let origin = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), three_segment_manifest_body(758)))).await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    {
        let mut session = session.write().await;
        session.origin_control.record_origin_response(50);
    }
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/new/index.m3u8", origin.base_url)).expect("candidate entry URL");

    let Err(error) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("cross-host response without recovery binding must remain uncommitted");
    };

    assert!(matches!(
        error,
        OriginManifestFetchError::RecoveryUnavailable {
            reason: HlsManifestRecoveryUnavailableReason::NoEstablishedBindingAfterResponse,
        }
    ));
    assert_eq!(error.log_label(), "recovery_unavailable_after_response");
    assert_eq!(error.to_string(), "origin manifest recovery unavailable: no established binding after origin response");
    assert_eq!(
        classify_manifest_fetch_failure(&error),
        HlsManifestFetchFailureSignal::retryable(
            HlsManifestFetchFailureKind::AcceptanceConflict,
            HlsManifestHttpResponseEvidence::ValidResponse,
        )
    );
    assert_eq!(origin.requests.lock().await.len(), 1);
    {
        let session = session.read().await;
        assert_eq!(session.origin_control.last_origin_response_at_ms, Some(50));
        assert!(session.origin_control.manifest_origin_binding.is_none());
        assert!(session.origin_control.acceptance_episode.is_none());
    }

    let evidence_origin =
        spawn_test_origin(Arc::new(|_path| (200, Vec::new(), three_segment_manifest_body(758)))).await;
    let evidence_session = test_session();
    prepare_cross_host_baseline(&evidence_session).await;
    let progress_generation = {
        let mut session = evidence_session.write().await;
        session.origin_control.record_origin_response(50);
        session.origin_control.progress_generation
    };
    let mut evidence_request = test_origin_refresh_request(Arc::clone(&evidence_session));
    evidence_request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/new/index.m3u8", evidence_origin.base_url))
            .expect("evidence candidate entry URL");
    let metrics = Arc::clone(evidence_request.segment_worker_pool.metrics());

    assert!(trigger_origin_refresh_sync(evidence_request).await);
    assert_eq!(evidence_origin.requests.lock().await.len(), 1);
    {
        let session = evidence_session.read().await;
        assert!(session.origin_control.last_origin_response_at_ms.is_some_and(|recorded_at_ms| recorded_at_ms > 50));
        assert_eq!(session.origin_control.progress_generation, progress_generation);
        assert!(session.origin_control.manifest_origin_binding.is_none());
        assert!(session.origin_control.acceptance_episode.is_none());
    }
    let metrics = metrics.snapshot();
    assert_eq!(metrics.refresh_started, 1);
    assert_eq!(metrics.refresh_failed, 1);
    assert_eq!(metrics.refresh_completed, 0);
    assert_eq!(metrics.refresh_retried, 0);
    assert_eq!(metrics.refresh_skipped, 0);
}

#[tokio::test]
async fn cold_start_uses_one_initial_fetch_and_commits_a_fresh_baseline_before_recovery() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let server = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(1_000);
        session.last_effective_manifest_host = Some("stale.example.com".to_string());
    }
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };

    assert!(trigger_origin_refresh_sync(request).await);

    let manifest_requests =
        server.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, 1);
    assert_ne!(manifest_requests, plan.total_candidates());
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(0));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("127.0.0.1"));
    assert!(session.origin_control.acceptance_episode.is_none());
}

#[tokio::test]
async fn cold_start_hard_error_is_preserved_by_fetch_policy() {
    let server = spawn_test_origin(Arc::new(|_path| (404, Vec::new(), "missing".to_string()))).await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;

    let Err(error) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("cold hard status must be returned without synthetic recovery");
    };

    assert!(matches!(error, OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND)));
    assert_eq!(server.requests.lock().await.len(), 1);
    let session = session.read().await;
    assert!(session.origin_control.acceptance_episode.is_none());
    assert_eq!(
        session.origin_control.progress_phase,
        super::super::super::origin_progress::HlsOriginProgressPhase::Cold
    );
}
