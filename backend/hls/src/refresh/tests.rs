//! Tests for the refresh flow.
//!
//! These drive `refresh_and_commit` and the trigger entry points end to end, so
//! they stay with the orchestration in `super` rather than with the individual
//! steps in `commit`, `switch_staging`, `failure` and `timing`.

use super::{
    super::{
        availability_reevaluation::HlsAvailabilityReevaluationRegistration,
        deterministic_conflict::HlsDeterministicTimelineConflict,
        manager::{HlsTerminalCommitPayload, HlsTerminalCommitRequest, HlsTerminalTailPreparationRequest},
        manifest_acceptance::HlsManifestAcceptanceTrigger,
        manifest_fetch::{
            classify_origin_manifest_status, deterministic_timeline_conflict_from_rejection,
            fetch_hls_origin_manifest_request, hls_manifest_redirect_host, origin_highwater_policy_limit,
            refresh_from_live_hls_entrypoint_with_retries, resolved_hls_manifest_request_url_from_input,
            retry_after_delay_ms, retry_hls_origin_manifest_recovery_chain,
            score_hls_manifest_recovery_candidate as score_manifest_recovery_candidate, FetchedOriginManifest,
            HlsManifestCommitAcceptanceMode, HlsManifestCommitError, HlsManifestFetchSelection,
            HlsManifestOriginQualityScore, HlsManifestRecoveryUnavailableReason, HlsManifestRejectLogReason,
            HlsManifestSequenceRelation, HlsOriginManifestFetchContext, HlsOriginManifestFetchRequest,
            LiveHlsOriginEntry, ManifestRecoverySelectionLogPhase, OriginManifestFetchError, OriginManifestStatusClass,
            RetryPolicy,
        },
        manifest_origin_binding::HlsManifestOriginBinding,
        prepared_terminal_bundle::HlsPreparedTerminalBundleKey,
        recovery_timing::{
            HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsLeaseCutoverTiming,
            HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsRecoveryEncryptionReadiness, HlsRecoveryEtaMs,
            HlsRecoveryMapWorkload, HlsRecoveryMediumReadiness, HlsRecoveryObjectReadiness, HlsRecoverySegmentWorkload,
            HlsRecoveryTimingPolicy, HlsRecoveryWorkload, HlsRecoveryWorkloadEnvelope,
            HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitWindow, HlsTerminalMediaPreparationState,
            HlsTransitionMarginMs,
        },
        runtime_custom_tail::HlsRuntimeCustomTailReason,
        session_store::HlsSessionIncarnation,
        terminal_commit::HlsTerminalAssetRevisionGuard,
        terminal_pending::{HlsTerminalPendingOwnerKey, HlsTerminalPendingRegistration},
        terminal_tail::HlsTerminalAssetIdentity,
    },
    cancel_superseded_terminal_work_after_media_progress,
    commit::{
        commit_fetched_manifest, key_resource_extension, record_committed_manifest_media_progress,
        refresh_origin_work_generation_matches, transient_reason_log_fields, HlsManifestCommitProgressEvidence,
    },
    current_time_millis,
    failure::{
        apply_manifest_fetch_failure_signal, classify_manifest_fetch_failure, manifest_hard_fetch_error,
        request_error_indicates_timeout, HlsManifestFetchFailureKind, HlsManifestFetchFailureSignal,
        HlsManifestHttpResponseEvidence,
    },
    fetch_and_commit_manifest_with_policy, manifest_fetch_context, manifest_recovery_trigger,
    mark_origin_refresh_started, mark_origin_refresh_started_with_outcome, record_committed_manifest_success,
    refresh_and_commit,
    timing::{
        build_manifest_refresh_timing, compute_origin_refresh_interval_ms, format_millis_as_seconds,
        format_optional_millis_as_seconds, manifest_progress_from_highwater, HlsManifestProgress,
    },
    trigger_origin_refresh_sync, HlsManifestCommitRequirement, HlsManifestRefreshCompletionDiagnostic,
    HlsOriginRefreshTriggerOutcome, HlsPostRefreshAvailabilityAction, HlsPostRefreshAvailabilityReason,
    HlsPostRefreshRuntime, OriginRefreshRequest, OriginRefreshState,
};
use crate::{
    build_rewrite_secret_fingerprint, is_hls_provisioning_gap_segment, is_hls_provisioning_segment,
    manifest_fetch::fetched_effective_manifest_host,
    media_reserve::{HlsLeaseReserveAvailabilityBasis, HlsLeaseReserveSnapshot},
    refresh::maybe_trigger_origin_refresh,
    terminal_tail::HlsLeasePlaybackMode,
    timeline::HLS_PROVISIONING_ORIGIN_EPOCH,
    CacheAccessState, GarbageCollectionPolicy, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeasePendingDeadline,
    HlsAccessLeaseStore, HlsAccessLeaseTiming, HlsBoundAccountAcquireErrorKind, HlsFreshManifestRequiredReason,
    HlsGarbageCollector, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestAcceptanceDirective,
    HlsManifestAcceptanceExhaustionReason, HlsManifestCommitIdentity, HlsManifestDeliveryMode, HlsMapWorkerPool,
    HlsMediaContainer, HlsOriginAccountBinding, HlsOriginIoContext, HlsPlaybackFamilyKey,
    HlsPreparedTerminalBundleState, HlsProxyManager, HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentWorkerPool,
    HlsSession, HlsSessionKey, HlsSessionMode, HlsSessionStore, HlsTerminalTailCompatibility, MapCacheStatus,
    OriginSegmentKey, ProxySessionId, RenderedManifest, RenderedManifestStoreOutcome, SegmentCacheKey,
    SegmentCacheStatus, SegmentEntry, SegmentFetchPolicy, SegmentFetchPriority, TimelineMapError,
    TransientObjectFetchDecision, TransientPassthroughReason, TransientResourceFile, TransientResourceId,
    TransientResourceKind,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use shared::model::{
    ConfigPaths, ConfigProviderDto, HlsManifestRecoveryBurstLevel, HlsSegmentRepairMode, HlsStripMode,
    ProviderUrlSelectionPolicy,
};
use std::{
    fmt::Write,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{oneshot, Mutex, Notify, RwLock},
};
use tuliprox_core::{
    model::{
        AppConfig, Config, ConfigProvider, CustomStreamResponse, HlsManifestRecoveryBurstConfig,
        HlsSegmentRepairConfig, ReverseProxyDisabledHeaderConfig, SourcesConfig, StripConfig,
    },
    utils::content_coding::{ContentCoding, ContentCodingError},
};
use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;
use tuliprox_session::ConnectionKind;
use url::Url;

async fn retry_test_manifest_recovery_chain(
    request: &OriginRefreshRequest,
    target_url: Url,
    reject_reason: HlsManifestRejectLogReason,
) -> Result<super::CommittedOriginManifest, OriginManifestFetchError> {
    let fetch_context = manifest_fetch_context(request);
    retry_hls_origin_manifest_recovery_chain(
        &fetch_context,
        test_manifest_origin_binding(target_url),
        Some(reject_reason),
        None,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::commit_manifest_recovery_candidate(request, fetched, acceptance_mode),
    )
    .await
}

fn path_has_extension(path: &str, extension: &str) -> bool {
    std::path::Path::new(path).extension().is_some_and(|actual| actual.eq_ignore_ascii_case(extension))
}

fn test_manifest_origin_binding(target_url: Url) -> HlsManifestOriginBinding {
    HlsManifestOriginBinding::new(target_url, None).expect("concrete HTTP test binding")
}

fn test_session() -> Arc<RwLock<HlsSession>> {
    Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)))
}

fn repeated_transient_manifest(segment_count: usize) -> String {
    let mut body = String::from(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n",
    );
    for _ in 0..segment_count {
        body.push_str("#EXTINF:4,\nsame.ts\n");
    }
    body
}

fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
    Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
        max_level: HlsSegmentRepairMode::Off,
        apply_to_first_segments: 1,
        max_parallel_repairs: 1,
        ..Default::default()
    }))
}

fn test_app_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::empty()),
        api_proxy: Arc::new(ArcSwapOption::empty()),
        file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::empty()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::default()),
    })
}

#[test]
fn origin_refresh_state_starts_only_when_due_and_not_in_flight() {
    let mut state = OriginRefreshState { next_fetch_allowed_at_ms: 100, ..OriginRefreshState::default() };
    assert!(!state.is_due(99));
    assert!(state.is_due(100));
    state.mark_started(100);
    assert!(!state.is_due(101));
}

#[test]
fn normal_key_extension_is_always_compatible_with_the_transient_route() {
    let extensions = [
        key_resource_extension("https://origin.example/live/key.php?token=secret"),
        key_resource_extension("https://origin.example/live/opaque"),
        key_resource_extension("https://origin.example/live/key.BIN"),
        key_resource_extension("https://origin.example/live/key.key"),
    ];

    assert_eq!(extensions, ["key", "key", "bin", "key"]);
    for extension in extensions {
        let file_name = format!("{}.{}", "abcdefghijklmnop", extension);
        let parsed = TransientResourceFile::parse(&file_name).expect("normalized key route parses");
        assert_eq!(parsed.resource_id, TransientResourceId("abcdefghijklmnop".to_string()));
        assert_eq!(parsed.extension, extension);
    }
}

fn test_deterministic_timeline_conflict() -> HlsDeterministicTimelineConflict {
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
         #EXTINF:4,\n490.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n491.ts\n",
    );
    deterministic_timeline_conflict_from_rejection(
        &fetched,
        &HlsManifestRejectLogReason::PublishedResourceReplay {
            previous_proxy_tail: Some(2),
            existing_proxy_seq: 0,
            candidate_position: 1,
            candidate_origin_seq: 491,
            resource_key: super::super::resource_identity::HlsMediaResourceIdentity::from_url(
                "http://origin.example.com/live/final/480.ts",
                None,
            )
            .semantic_key(),
            decision: super::super::timeline::HlsResourceReplayDecision::RejectContradictoryOrder,
        },
    )
    .expect("published replay rejection produces deterministic evidence")
}

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

type ManifestFetchFailureCase = (&'static str, OriginManifestFetchError, HlsManifestFetchFailureSignal);

fn manifest_response_failure_cases() -> Vec<ManifestFetchFailureCase> {
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

fn manifest_content_failure_cases() -> Vec<ManifestFetchFailureCase> {
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
fn deterministic_conflict_returns_generation_bound_post_refresh_action() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.origin_control.progress_generation = 7;
    session.activity.media_readiness_generation = 11;

    let action = apply_manifest_fetch_failure_signal(
        &mut session,
        &OriginManifestFetchError::DeterministicTimelineConflict(Box::new(test_deterministic_timeline_conflict())),
        100,
    );

    assert_eq!(
        action,
        HlsPostRefreshAvailabilityAction::Reevaluate {
            reason: HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
            origin_progress_generation: 7,
            media_readiness_generation: 11,
        }
    );
    assert_eq!(session.origin_control.last_origin_response_at_ms, Some(100));
    assert_eq!(
        session.origin_control.path_condition,
        super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict
    );
    assert!(session.fresh_manifest_commit_required.is_none());
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
            super::super::origin_progress::HlsOriginPathCondition::RetryableFetchFailure
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
        super::super::origin_progress::HlsOriginPathCondition::HardFetchFailure
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
fn retry_exhaustion_preserves_qualified_acceptance_conflict_but_not_all_failed_transport_state() {
    for (reason, expected) in [
        (
            HlsManifestAcceptanceExhaustionReason::NoCommittableCandidate,
            super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict,
        ),
        (
            HlsManifestAcceptanceExhaustionReason::AllFailed,
            super::super::origin_progress::HlsOriginPathCondition::RetryableFetchFailure,
        ),
    ] {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let burst_plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
        session.origin_control.begin_acceptance_episode(
            100,
            burst_plan,
            HlsManifestAcceptanceTrigger::Observe,
            &test_acceptance_episode_timing(100, burst_plan),
        );
        let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
        episode.record_full_burst();
        episode.record_exhaustion(reason);
        episode.hold_after_uncommitted_burst(None, None);
        session.origin_control.path_condition =
            super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict;

        apply_manifest_fetch_failure_signal(&mut session, &OriginManifestFetchError::RetryExhausted, 200);

        assert_eq!(session.origin_control.path_condition, expected);
    }
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
fn manifest_highwater_policy_limit_uses_target_duration_fallback() {
    assert_eq!(origin_highwater_policy_limit(60, None), Some(4));
    assert_eq!(origin_highwater_policy_limit(61, None), Some(5));
    assert_eq!(origin_highwater_policy_limit(60, Some(12)), Some(5));
}

#[test]
fn manifest_recovery_burst_levels_map_to_candidate_counts() {
    let cases = [
        (HlsManifestRecoveryBurstLevel::Off, 1, 1),
        (HlsManifestRecoveryBurstLevel::Friendly, 2, 1),
        (HlsManifestRecoveryBurstLevel::Cautious, 3, 1),
        (HlsManifestRecoveryBurstLevel::Balanced, 4, 1),
        (HlsManifestRecoveryBurstLevel::Intense, 5, 1),
        (HlsManifestRecoveryBurstLevel::Aggressive, 6, 1),
        (HlsManifestRecoveryBurstLevel::Beast, 6, 2),
    ];
    for (level, expected_slots, expected_lanes) in cases {
        let plan = level.plan();
        let expected_candidates = expected_slots * expected_lanes;
        assert_eq!(plan.slots, expected_slots);
        assert_eq!(plan.lanes_per_slot, expected_lanes);
        assert_eq!(plan.total_candidates(), expected_candidates);
        assert_eq!(level.total_candidates(), expected_candidates);
    }
}

#[test]
fn retry_after_header_is_parsed_as_milliseconds() {
    let mut headers = HeaderMap::new();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
    assert_eq!(retry_after_delay_ms(&headers), Some(3_000));
}

#[test]
fn resolved_hls_manifest_request_url_uses_provider_index_locally() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec!["http://provider-a.example".into(), "http://provider-b.example".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
        dns: None,
    }));
    let provider_entry =
        LiveHlsOriginEntry::parse_with_url_failover_provider("provider://demo/live/u/p/1.m3u8", Some(provider))
            .unwrap();

    let resolved =
        resolved_hls_manifest_request_url_from_input(&provider_entry.to_input_source(), Some(1), provider_entry.url());
    assert_eq!(resolved.as_str(), "http://provider-b.example/live/u/p/1.m3u8");
    assert!(!resolved.as_str().contains("provider://"));

    let direct_entry = LiveHlsOriginEntry::parse("http://origin.example/live/u/p/1.m3u8").unwrap();
    assert_eq!(
        resolved_hls_manifest_request_url_from_input(&direct_entry.to_input_source(), Some(1), direct_entry.url())
            .as_str(),
        "http://origin.example/live/u/p/1.m3u8"
    );
}

#[test]
fn manifest_timing_log_values_are_seconds_or_none() {
    assert_eq!(format_optional_millis_as_seconds(Some(4_500)), "4.500");
    assert_eq!(format_optional_millis_as_seconds(None), "none");
    assert_eq!(format_millis_as_seconds(2_000), "2.000");
}

#[test]
fn refresh_interval_uses_half_reference_duration_without_upper_clamp() {
    assert_eq!(compute_origin_refresh_interval_ms(Some(8_000), None), 4_000);
    assert_eq!(compute_origin_refresh_interval_ms(Some(500), None), 1_000);
    assert_eq!(compute_origin_refresh_interval_ms(Some(20_000), None), 10_000);
    assert_eq!(compute_origin_refresh_interval_ms(None, None), 2_000);
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

#[tokio::test]
async fn hls_terminal_commit_media_progress_cancels_pending_owner_after_session_lock_release() {
    let session = test_session();
    let request = test_origin_refresh_request(Arc::clone(&session));
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let coordinator = request.hls_proxy.terminal_pending();
    let (started_tx, started_rx) = oneshot::channel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let owner_key = HlsTerminalPendingOwnerKey {
        session_incarnation: HlsSessionIncarnation::for_test(1),
        proxy_session_id,
        lease_id: HlsAccessLeaseId("progress-cancel".to_string()),
        lease_issued_at_ms: 10,
        expected_admission_generation: 20,
        manifest_snapshot_generation: 30,
        cursor_generation: 40,
        decision_generation: 50,
        reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
        bundle_key: HlsPreparedTerminalBundleKey {
            asset: HlsTerminalAssetIdentity { revision: 60, fingerprint: [6; 32] },
            target_duration_ms: 4_000,
            segment_count: super::HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        },
        latest_safe_commit_at_ms: 10_000,
    };
    let asset_guard = HlsTerminalAssetRevisionGuard::matching_for_test(Some(owner_key.bundle_key.asset));
    assert_eq!(
        coordinator.register(owner_key, &asset_guard, move |ownership| async move {
            assert!(started_tx.send(()).is_ok());
            ownership.cancelled().await;
            assert!(cancelled_tx.send(()).is_ok());
        }),
        HlsTerminalPendingRegistration::Scheduled
    );
    assert!(started_rx.await.is_ok());

    let commit_result = Ok((
        HlsManifestCommitProgressEvidence::CacheTimeline(build_manifest_refresh_timing(
            None,
            Some(4_000),
            HlsManifestProgress::Advanced,
        )),
        false,
        false,
    ));
    cancel_superseded_terminal_work_after_media_progress(&request, &commit_result).await;

    assert!(cancelled_rx.await.is_ok());
    assert_eq!(coordinator.owner_count(), 0);
}

#[test]
fn hls_cutover_policy_transient_success_preserves_recovery_episode_and_empty_refresh_evidence() {
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
        HlsManifestCommitProgressEvidence::Transient(build_manifest_refresh_timing(
            None,
            Some(12_000),
            HlsManifestProgress::Advanced,
        )),
        10_000,
        10_100,
    );

    assert_eq!(bookkeeping_timing.progress, HlsManifestProgress::Unchanged);
    assert_eq!(applied_interval_ms, 1_000);
    assert_eq!(session.origin_refresh.consecutive_empty_refreshes, 3);
    assert_eq!(session.origin_refresh.next_fetch_allowed_at_ms, 11_000);
    assert!(session.origin_control.acceptance_episode.is_some());
    assert_eq!(session.origin_control.recovery_samples.p95_ms(), None);
    assert_eq!(session.origin_control.last_origin_response_at_ms, Some(10_100));
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
fn recovery_selection_log_phase_distinguishes_single_candidate_from_burst() {
    assert_eq!(ManifestRecoverySelectionLogPhase::from_candidate_count(1), ManifestRecoverySelectionLogPhase::Recovery);
    assert_eq!(ManifestRecoverySelectionLogPhase::from_candidate_count(2), ManifestRecoverySelectionLogPhase::Burst);
    assert_eq!(ManifestRecoverySelectionLogPhase::Recovery.as_log_label(), "recovery");
    assert_eq!(ManifestRecoverySelectionLogPhase::Burst.as_log_label(), "burst");
}

#[test]
fn transient_reason_log_fields_include_unsupported_tag() {
    let reason = TransientPassthroughReason::UnsupportedTag { tag: "#EXT-X-PART".to_string() };

    assert_eq!(transient_reason_log_fields(&reason), "reason=unsupported_tag tag=#EXT-X-PART");
}

#[test]
fn different_host_candidate_is_not_committed_immediately() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("previous.example.com".to_string());
    let binding = HlsManifestOriginBinding::new(
        Url::parse("https://previous.example.com/live/index.m3u8?token=baseline").expect("baseline URL"),
        Some(0),
    )
    .expect("baseline binding");
    session.origin_control.manifest_origin_binding = Some(binding.clone());
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:758\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg758.ts\n#EXTINF:4.0,\nseg759.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(matches!(result, Err(HlsManifestCommitError::RetryCurrentTarget)));
    assert!(session.transient.last_manifest_body.is_none());
    assert_eq!(session.origin_control.manifest_origin_binding.as_ref(), Some(&binding));
}

#[test]
fn fresh_revalidation_rebases_normal_manifest_on_the_pinned_host() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.origin_seq_highwater = Some(1_000);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    let mut request = test_origin_refresh_request(test_session());
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    let fetched =
        fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nseg10.ts\n");

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(result.is_ok());
    assert_eq!(session.origin_seq_highwater, Some(10));
    assert_eq!(session.origin_epoch, 1);
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("origin.example.com"));
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

async fn spawn_rotating_parent_origin() -> TestOriginServer {
    let rotating_parent = Arc::new(AtomicUsize::new(0));
    let handler_counter = Arc::clone(&rotating_parent);
    spawn_test_origin(Arc::new(move |_path| {
        let parent = handler_counter.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        (
            200,
            Vec::new(),
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
                 #EXTINF:4,\n/stream/{parent:016x}/1745190_490.ts\n\
                 #EXTINF:4,\n/stream/{parent:016x}/1745180_480.ts\n\
                 #EXTINF:4,\n/stream/{parent:016x}/1745191_491.ts\n"
            ),
        )
    }))
    .await
}

#[tokio::test]
async fn rotating_volatile_parent_proves_one_deterministic_conflict() {
    let baseline_body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:480\n\
         #EXTINF:4,\n/stream/aaaaaaaaaaaaaaaa/1745180_480.ts\n\
         #EXTINF:4,\n/stream/aaaaaaaaaaaaaaaa/1745181_481.ts\n\
         #EXTINF:4,\n/stream/aaaaaaaaaaaaaaaa/1745182_482.ts\n";
    let server = spawn_rotating_parent_origin().await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    let manifest_url = format!("{}/live/user/pass/12345.m3u8", server.base_url);
    request.origin_entry = LiveHlsOriginEntry::parse(&manifest_url).expect("local replay origin entry");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let plan = request.manifest_recovery_burst.level.plan();
    let mut baseline = fetched_manifest(baseline_body);
    baseline.final_manifest_url = manifest_url.clone();
    baseline.resolved_request_url = manifest_url;
    baseline.redirect_host = Some("127.0.0.1".to_string());
    {
        let mut session = session.write().await;
        commit_fetched_manifest(&mut session, &baseline, &request, 100).expect("baseline commits");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 101 };
        }
        session.render_and_store_manifest(101).expect("baseline publishes");
        assert_eq!(session.published_resource_history.generation(), 3);
    }

    let Err(first) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("one complete candidate landscape must prove the replay conflict");
    };
    let OriginManifestFetchError::DeterministicTimelineConflict(first_conflict) = first else {
        panic!("deterministic replay conflict must not collapse to retry exhaustion");
    };
    assert_eq!(first_conflict.previous_proxy_tail, Some(2));
    assert_eq!(first_conflict.existing_proxy_seq, 0);
    assert_eq!(first_conflict.candidate_position, 1);
    assert_eq!(first_conflict.candidate_origin_seq, 491);
    assert_eq!(server.requests.lock().await.len(), plan.total_candidates().saturating_add(1));
    {
        let session = session.read().await;
        let episode = session.origin_control.acceptance_episode.as_ref().expect("acceptance episode retained");
        assert_eq!(episode.full_bursts_completed, 1);
        assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
        assert!(episode.deterministic_conflict_receipt().is_some());
        assert!(matches!(
            super::super::manifest_acceptance::manifest_acceptance_episode_status(
                Some(episode),
                episode.generation,
                request.now_ms,
            ),
            super::super::manifest_acceptance::HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted {
                reason: HlsManifestAcceptanceExhaustionReason::DeterministicTimelineConflict,
                ..
            }
        ));
        assert_eq!(session.proxy_next_seq, Some(3), "replayed content receives no proxy sequence");
    }

    let request_count_before_receipt_sample = server.requests.lock().await.len();
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Observe;
    let Err(repeated) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("an unchanged ordinary sample must remain rejected");
    };
    assert!(matches!(
        repeated,
        OriginManifestFetchError::DeterministicTimelineConflict(ref conflict)
            if conflict.as_ref() == first_conflict.as_ref()
    ));
    assert_eq!(
        server.requests.lock().await.len(),
        request_count_before_receipt_sample.saturating_add(1),
        "an unchanged receipt permits one ordinary request but no second burst"
    );

    {
        let mut session = session.write().await;
        session.published_resource_history.record(
            super::super::resource_identity::HlsMediaResourceIdentity::from_url(
                "http://127.0.0.1/live/user/pass/unrelated-published.ts",
                None,
            ),
            99,
        );
    }
    let request_count_before_changed_landscape = server.requests.lock().await.len();
    let Err(changed) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("changed published evidence must still reject the replay");
    };
    assert!(matches!(changed, OriginManifestFetchError::DeterministicTimelineConflict(_)));
    assert_eq!(
        server.requests.lock().await.len(),
        request_count_before_changed_landscape.saturating_add(plan.total_candidates()),
        "changed receipt evidence authorizes exactly one newly configured full burst"
    );
    let session = session.read().await;
    assert_eq!(session.proxy_next_seq, Some(3), "the replay stays blocked after reevaluation");
    assert!(!session.segments.contains_key(&3));
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
        super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict
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

#[tokio::test]
async fn successful_refresh_wakes_sleeping_post_refresh_owner() {
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let now_ms = current_time_millis();
    let progressed_body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:481\n\
         #EXTINF:4,\n481.ts\n#EXTINF:4,\n482.ts\n#EXTINF:4,\n483.ts\n";
    let server = spawn_test_origin(Arc::new(move |_path| (200, Vec::new(), progressed_body.to_string()))).await;
    let (session, _) = ctx
        .hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "post-refresh-wakeup"), b"secret", now_ms)
        .await;
    let manifest_url = format!("{}/live/user/pass/12345.m3u8", server.base_url);
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), ctx);
    request.origin_entry = LiveHlsOriginEntry::parse(&manifest_url).expect("local successful origin entry");
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
        session.origin_control.path_condition =
            super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict;
    }
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("post-refresh-wakeup-lease".to_string());
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

    assert!(mark_origin_refresh_started(&mut request, now_ms).await);
    let coordinator = ctx.hls_proxy.availability_reevaluations();
    let initial_permits = coordinator.available_task_permits_for_test();
    let (origin_progress_generation, media_readiness_generation) = {
        let session = session.read().await;
        (session.origin_control.progress_generation, session.activity.media_readiness_generation)
    };
    assert_eq!(
        super::super::availability::register_post_refresh_availability_reevaluation(
            ctx.clone(),
            Arc::clone(&session),
            request.clone(),
            HlsPostRefreshAvailabilityAction::Reevaluate {
                reason: HlsPostRefreshAvailabilityReason::HardManifestFailure,
                origin_progress_generation,
                media_readiness_generation,
            },
        )
        .await,
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
    assert_eq!(coordinator.owner_count(), 1);
    assert_eq!(coordinator.available_task_permits_for_test(), initial_permits.saturating_sub(1));
    let progress_generation_before = session.read().await.origin_control.progress_generation;

    refresh_and_commit(request, now_ms).await;
    for _ in 0..256 {
        if coordinator.owner_count() == 0 && coordinator.available_task_permits_for_test() == initial_permits {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert!(session.read().await.origin_control.progress_generation > progress_generation_before);
    assert_eq!(coordinator.owner_count(), 0);
    assert_eq!(coordinator.available_task_permits_for_test(), initial_permits);
}

async fn assert_requirement_set_after_refresh_start_survives_fresh_commit(
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

#[tokio::test]
async fn concurrent_refresh_suppression_reports_in_flight_state() {
    let session = test_session();
    session.write().await.origin_refresh.mark_started(900);
    let mut request = test_origin_refresh_request(Arc::clone(&session));

    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 1_000).await,
        HlsOriginRefreshTriggerOutcome::InFlight
    );
    assert_eq!(session.read().await.origin_refresh.last_fetch_started_at_ms, Some(900));
}

#[tokio::test]
async fn hls_manifest_acceptance_directive_stale_guard_prevents_refresh_start() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) = hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "stale-pressure-guard"), b"secret", 100)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let owner_key = hls_proxy
        .availability_reevaluation_owner_key(&session, &proxy_session_id)
        .await
        .expect("registered session has recovery-pressure evidence");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.recovery_pressure_guard =
        Some(super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key));
    session.write().await.origin_control.progress_generation = owner_key.origin_progress_generation.saturating_add(1);

    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 100).await,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureSuperseded
    );
    let session = session.read().await;
    assert!(!session.origin_refresh.in_flight);
    assert!(session.origin_control.acceptance_episode.is_none());
}

#[tokio::test]
async fn hls_availability_reevaluation_cursor_evidence_supersedes_refresh_guard() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) = hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "cursor-pressure-guard"), b"secret", 100)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("cursor-pressure-guard".to_string());
    {
        let mut leases = hls_proxy.access_leases().write().await;
        assert!(leases.prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("user", "client"),
            proxy_session_id.clone(),
            "user".to_string(),
            "session-token".to_string(),
            1,
            "cursor-pressure-guard".to_string(),
            1,
            100,
            60_000,
        )));
        assert!(leases
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                100,
                HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
            )
            .is_activated());
    }
    let owner_key = hls_proxy
        .availability_reevaluation_owner_key(&session, &proxy_session_id)
        .await
        .expect("live lease has availability evidence");
    {
        let mut leases = hls_proxy.access_leases().write().await;
        let identity = leases
            .response_snapshot(&lease_id, &proxy_session_id, 100)
            .and_then(|lease| lease.media_identity())
            .expect("live lease identity");
        assert!(leases
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 20, 101,)
            .is_some());
    }
    let current_owner_key = hls_proxy
        .availability_reevaluation_owner_key(&session, &proxy_session_id)
        .await
        .expect("updated availability evidence");
    assert!(current_owner_key.availability_evidence_generation > owner_key.availability_evidence_generation);
    assert_eq!(current_owner_key.origin_progress_generation, owner_key.origin_progress_generation);
    assert_eq!(current_owner_key.media_readiness_generation, owner_key.media_readiness_generation);

    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.recovery_pressure_guard =
        Some(super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key));
    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 102).await,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureSuperseded
    );
    assert!(!session.read().await.origin_refresh.in_flight);
}

#[tokio::test]
async fn hls_availability_reevaluation_guard_contention_is_typed() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) = hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "contended-pressure-guard"), b"secret", 100)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let owner_key =
        hls_proxy.availability_reevaluation_owner_key(&session, &proxy_session_id).await.expect("session evidence");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.recovery_pressure_guard =
        Some(super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key));
    let lease_guard = hls_proxy.access_leases().write().await;

    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 100).await,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureStateContention
    );
    assert!(!session.read().await.origin_refresh.in_flight);
    drop(lease_guard);
}

#[tokio::test]
async fn hls_availability_reevaluation_other_session_evidence_keeps_guard_current() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) =
        hls_proxy.get_or_create_session_with_outcome(HlsSessionKey::new(1, "guard-session-a"), b"secret", 100).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let owner_key =
        hls_proxy.availability_reevaluation_owner_key(&session, &proxy_session_id).await.expect("session evidence");
    let other_proxy_session_id = ProxySessionId("guard-session-b".to_string());
    assert!(hls_proxy.access_leases().write().await.prepare_access_lease(HlsAccessLease::pending(
        HlsAccessLeaseId("guard-session-b".to_string()),
        HlsPlaybackFamilyKey::new("other", "client"),
        other_proxy_session_id,
        "other".to_string(),
        "other-session".to_string(),
        1,
        "guard-session-b".to_string(),
        2,
        100,
        60_000,
    )));
    let guard = super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key);

    assert!(matches!(
        hls_proxy.with_current_recovery_pressure_session(&session, &guard, |_| 7_u8),
        super::super::availability_reevaluation::HlsRecoveryPressureGuardAccess::Acquired(7)
    ));
}

#[tokio::test]
async fn hls_availability_reevaluation_removed_lease_supersedes_old_guard() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) = hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "removed-pressure-guard"), b"secret", 100)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("removed-pressure-guard".to_string());
    assert!(hls_proxy.access_leases().write().await.prepare_access_lease(HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new("user", "client"),
        proxy_session_id.clone(),
        "user".to_string(),
        "session-token".to_string(),
        1,
        "removed-pressure-guard".to_string(),
        1,
        100,
        60_000,
    )));
    let owner_key =
        hls_proxy.availability_reevaluation_owner_key(&session, &proxy_session_id).await.expect("lease evidence");
    assert!(hls_proxy.access_leases().write().await.remove_access_lease(&lease_id).is_some());
    let guard = super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key);

    assert!(matches!(
        hls_proxy.with_current_recovery_pressure_session(&session, &guard, |_| ()),
        super::super::availability_reevaluation::HlsRecoveryPressureGuardAccess::Superseded
    ));
    assert!(!session.read().await.origin_refresh.in_flight);
}

fn cutover_live_manifest_snapshot() -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
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

async fn assert_recovery_supersedes_terminal_preparation(
    hls_proxy: &HlsProxyManager,
    session: &Arc<RwLock<HlsSession>>,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    preparation: &super::super::lease::HlsTerminalTailPreparation,
) {
    assert_eq!(
        hls_proxy.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
            session,
            lease_id,
            proxy_session_id,
            preparation,
            now_ms: 2_100,
            payload: HlsTerminalCommitPayload::Unavailable(HlsTerminalTailCompatibility::MissingAsset),
            asset_revision_guard: super::super::terminal_commit::HlsTerminalAssetRevisionGuard::matching_for_test(None),
        }),
        super::super::terminal_commit::HlsTerminalCommitOutcome::RecoveryCommitted
    );
    let lease = hls_proxy
        .access_lease_response_snapshot(lease_id, proxy_session_id, 2_100)
        .await
        .expect("recovered lease remains live");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert!(!session.read().await.has_terminal_tail_protections());
}

async fn prepare_failed_acceptance_episode(session: &Arc<RwLock<HlsSession>>) -> u64 {
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
    let now_ms = super::current_time_millis();
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
        .response_snapshot(&lease_id, &proxy_session_id, super::current_time_millis())
        .expect("pending lease should remain available");
    let Some(HlsAccessLeasePendingDeadline::FollowUp { deadline_ms }) = lease.pending_deadline else {
        panic!("pending lease should be shortened to follow-up");
    };
    assert!(deadline_ms < now_ms.saturating_add(90_000));
    assert!(deadline_ms <= super::current_time_millis().saturating_add(10_000));
}

async fn refresh_session_with_origin_body(body: &'static str) -> Arc<RwLock<HlsSession>> {
    let session = test_session();
    let server = spawn_test_origin(Arc::new(move |_path| (200, Vec::new(), body.to_string()))).await;
    let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
        .expect("valid origin entry");
    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 3 },
        retry_policy: no_delay_policy(),
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

    assert!(trigger_origin_refresh_sync(request).await);
    {
        let session = session.read().await;
        assert!(
            !session.segments.is_empty() || session.transient.last_manifest_body.is_some(),
            "synchronous controlled origin work must commit origin state: mode={:?} path_condition={:?} failures={}",
            session.mode,
            session.origin_control.path_condition,
            session.origin_refresh.consecutive_failures
        );
    }
    session
}

#[tokio::test]
async fn refresh_stores_headers_after_hls_origin_policy() {
    let session = test_session();
    let server = spawn_test_origin(Arc::new(|_path| {
        (200, Vec::new(), "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string())
    }))
    .await;
    let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
        .expect("valid origin entry");
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
    headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
    headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
    headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
    headers.insert(HeaderName::from_static("x-blocked"), HeaderValue::from_static("blocked"));
    headers.insert(HeaderName::from_static("cf-ray"), HeaderValue::from_static("cf"));
    headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));

    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers,
        origin_provider_session_headers: HeaderMap::new(),
        disabled_headers: Some(ReverseProxyDisabledHeaderConfig {
            referer_header: false,
            x_header: true,
            cloudflare_header: true,
            custom_header: Vec::new(),
        }),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        now_ms: 100,
        origin_io: None,
        post_refresh_runtime: None,
    };

    assert!(maybe_trigger_origin_refresh(request).await);
    for _ in 0..50 {
        if session.read().await.last_rendered_manifest.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let session = session.read().await;
    assert!(!session.origin_request_headers.contains_key(header::AUTHORIZATION));
    assert!(!session.origin_request_headers.contains_key(header::COOKIE));
    assert!(!session.origin_request_headers.contains_key("proxy-authorization"));
    assert!(!session.origin_request_headers.contains_key(header::HOST));
    assert!(!session.origin_request_headers.contains_key("x-blocked"));
    assert!(!session.origin_request_headers.contains_key("cf-ray"));
    assert_eq!(session.origin_request_headers.get(header::ACCEPT_LANGUAGE).expect("language"), "de");
}

#[tokio::test]
async fn accepted_manifest_commit_stores_provider_session_cookie_separately() {
    let session = test_session();
    let server = spawn_test_origin(Arc::new(|_path| {
        (
            200,
            vec![
                ("Set-Cookie", "sid=abc; Path=/; HttpOnly".to_string()),
                ("Set-Cookie", "pref=1; SameSite=Lax".to_string()),
            ],
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string(),
        )
    }))
    .await;
    let entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
        .expect("valid origin entry");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;

    assert!(Box::pin(trigger_origin_refresh_sync(request)).await);

    let session = session.read().await;
    assert!(!session.origin_request_headers.contains_key(header::COOKIE));
    assert_eq!(
        session.origin_provider_session_headers.get(header::COOKIE).expect("provider cookie"),
        "sid=abc; pref=1"
    );
}

#[test]
fn origin_account_binding_change_clears_provider_session_headers() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "1"), b"secret", 100);
    session.origin_provider_session_headers.insert(header::COOKIE, HeaderValue::from_static("sid=abc"));
    session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
        Arc::<str>::from("input"),
        Arc::<str>::from("account-a"),
        &session.proxy_session_id.clone(),
        100,
    )));
    assert!(session.origin_provider_session_headers.is_empty());

    session.origin_provider_session_headers.insert(header::COOKIE, HeaderValue::from_static("sid=next"));
    session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
        Arc::<str>::from("input"),
        Arc::<str>::from("account-a"),
        &session.proxy_session_id.clone(),
        200,
    )));
    assert!(!session.origin_provider_session_headers.is_empty());

    session.replace_origin_account_binding(Some(HlsOriginAccountBinding::new(
        Arc::<str>::from("input"),
        Arc::<str>::from("account-b"),
        &session.proxy_session_id.clone(),
        300,
    )));
    assert!(session.origin_provider_session_headers.is_empty());
}

#[tokio::test]
async fn ext_x_key_manifest_commits_transient_rewrite() {
    let session = refresh_session_with_origin_body(
        "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
    )
    .await;
    let session = session.read().await;
    let body = session.transient.last_manifest_body.as_ref().expect("transient body");

    assert!(matches!(
        session.mode,
        HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey }
    ));
    assert!(body.contains("/hls/shared/live/"));
    assert!(body.contains("/r/"));
    assert!(!body.contains("/hls/user/"));
    assert_eq!(session.transient.resources.len(), 2);
    assert_eq!(session.target_duration, Some(12));
    assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
}

#[tokio::test]
async fn finalized_event_manifest_commits_full_transient_lifecycle() {
    let mut origin_body =
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n".to_string();
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
        } if tag == "#EXT-X-PLAYLIST-TYPE"
    ));
    let stored_body_lifecycle = tuliprox_parser::hls::origin_manifest::parse_manifest_semantics(body).lifecycle();
    assert_eq!(session.transient.last_manifest_finalized(), stored_body_lifecycle.is_finalized());
    assert_eq!(
        session.transient.last_manifest_window_policy(),
        tuliprox_parser::hls::origin_manifest::HlsManifestWindowPolicy::PreserveFullManifest
    );
    assert_eq!(session.transient.current_manifest_resource_ids().len(), 8);
    assert_eq!(session.transient.last_manifest_playlist_duration_ms, Some(32_000));
    assert_eq!(session.transient.last_manifest_valid_until_ms(), None);
    assert!(body.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
    assert!(body.contains("#EXT-X-ENDLIST"));
    assert_eq!(body.matches("/r/").count(), 8);
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

#[test]
fn duplicate_media_units_are_rejected_locally_before_transient_commit() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let request = test_origin_refresh_request(test_session());
    let body = repeated_transient_manifest(crate::manifest_limits::MAX_HLS_LEASE_SNAPSHOT_SEGMENTS + 1);

    let result = commit_fetched_manifest(&mut session, &fetched_manifest(&body), &request, 100);

    assert!(matches!(
        result,
        Err(HlsManifestCommitError::LocalRepresentationLimit(violation))
            if violation.kind == crate::manifest_limits::HlsManifestLimitKind::LeaseSnapshotSegments
    ));
    assert!(session.transient.last_manifest_body.is_none());
    let signal = classify_manifest_fetch_failure(&OriginManifestFetchError::LocalRepresentationLimit(
        crate::manifest_limits::HlsManifestLimitViolation::new(
            crate::manifest_limits::HlsManifestLimitKind::LeaseSnapshotSegments,
            crate::manifest_limits::MAX_HLS_LEASE_SNAPSHOT_SEGMENTS + 1,
            crate::manifest_limits::MAX_HLS_LEASE_SNAPSHOT_SEGMENTS,
        ),
    ));
    assert_eq!(signal, HlsManifestFetchFailureSignal::discarded(HlsManifestFetchFailureKind::LocalRepresentationLimit));
}

#[test]
fn transient_commit_generation_exhaustion_is_not_reported_as_manifest_limit() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.set_manifest_commit_generation_for_test(u64::MAX);
    let request = test_origin_refresh_request(test_session());
    let manifest_limit_rejections_before = request.segment_worker_pool.metrics().snapshot().manifest_limit_rejections;
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\nseg.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(matches!(result, Err(HlsManifestCommitError::CommitGenerationExhausted)));
    assert_eq!(
        request.segment_worker_pool.metrics().snapshot().manifest_limit_rejections,
        manifest_limit_rejections_before
    );
    assert!(session.transient.last_manifest_body.is_none());
}

#[tokio::test]
async fn initial_local_representation_limit_does_not_enter_origin_recovery() {
    let session = test_session();
    session.write().await.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let request = test_origin_refresh_request(Arc::clone(&session));
    let fetch_context = manifest_fetch_context(&request);
    let fetched =
        fetched_manifest(&repeated_transient_manifest(crate::manifest_limits::MAX_HLS_LEASE_SNAPSHOT_SEGMENTS + 1));
    let recovery_binding =
        test_manifest_origin_binding(Url::parse("http://127.0.0.1:9/recovery.m3u8").expect("test recovery URL"));

    let result =
        super::commit_initial_fetched_manifest(&request, &fetch_context, fetched, Some(recovery_binding), false).await;

    assert!(matches!(
        result,
        Err(OriginManifestFetchError::LocalRepresentationLimit(violation))
            if violation.kind == crate::manifest_limits::HlsManifestLimitKind::LeaseSnapshotSegments
    ));
}

#[tokio::test]
async fn initial_transient_manifest_without_snapshot_template_is_rejected_locally() {
    let session = test_session();
    session.write().await.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let request = test_origin_refresh_request(Arc::clone(&session));
    let fetch_context = manifest_fetch_context(&request);
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\nsegment.ts\n#EXT-X-ENDLIST\n",
    );
    let recovery_binding =
        test_manifest_origin_binding(Url::parse("http://127.0.0.1:9/recovery.m3u8").expect("test recovery URL"));

    let result =
        super::commit_initial_fetched_manifest(&request, &fetch_context, fetched, Some(recovery_binding), false).await;

    assert!(matches!(result, Err(OriginManifestFetchError::MalformedTransientRepresentation)));
    assert_eq!(
        classify_manifest_fetch_failure(&OriginManifestFetchError::MalformedTransientRepresentation),
        HlsManifestFetchFailureSignal::discarded(HlsManifestFetchFailureKind::MalformedTransientRepresentation)
    );
    let session = session.read().await;
    assert!(session.transient.last_manifest_body.is_none());
    assert!(session.transient.last_manifest_template().is_none());
    assert!(session.transient.last_manifest_commit_identity().is_none());
    assert_eq!(session.transient.manifest_generation(), 0);
    assert!(session.transient.resources.is_empty());
    assert!(crate::manifest_commit::hls_committed_manifest_body_for_request(
        &session,
        crate::manifest_commit::HlsCachedManifestOptions::initial(Duration::ZERO),
        0,
        100,
    )
    .is_none());
}

#[test]
fn rejected_transient_candidate_preserves_committed_state_and_pending_handoff() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let mut request = test_origin_refresh_request(test_session());
    request.transient_resource_ttl_ms = 1;
    let baseline = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\nsegment.ts\n",
    );
    assert!(commit_fetched_manifest(&mut session, &baseline, &request, 100).is_ok());
    session.mark_pending_handoff_discontinuity(7);
    let baseline_body = session.transient.last_manifest_body.clone().expect("baseline manifest");
    let baseline_generation = session.transient.manifest_generation();
    let baseline_resources = session.transient.resources.keys().cloned().collect::<std::collections::HashSet<_>>();
    let baseline_highwater = session.origin_seq_highwater;
    let key_format = "x".repeat(crate::manifest_limits::MAX_HLS_ENCRYPTION_DIRECTIVE_BYTES);
    let candidate = fetched_manifest(&format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\",KEYFORMAT=\"{key_format}\"\n\
         #EXTINF:4,\nsegment.ts\n"
    ));

    let result = commit_fetched_manifest(&mut session, &candidate, &request, 102);

    assert!(matches!(
        result,
        Err(HlsManifestCommitError::LocalRepresentationLimit(violation))
            if violation.kind
                == crate::manifest_limits::HlsManifestLimitKind::LeaseSnapshotEncryptionDirectiveBytes
    ));
    assert!(Arc::ptr_eq(
        session.transient.last_manifest_body.as_ref().expect("baseline remains current"),
        &baseline_body
    ));
    assert_eq!(session.transient.manifest_generation(), baseline_generation);
    assert_eq!(
        session.transient.resources.keys().cloned().collect::<std::collections::HashSet<_>>(),
        baseline_resources
    );
    assert_eq!(session.pending_handoff_discontinuity_sequence, Some(7));
    assert_eq!(session.origin_seq_highwater, baseline_highwater);
}

#[test]
fn transient_manifest_without_snapshot_template_preserves_deliverable_commit() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    let request = test_origin_refresh_request(test_session());
    let baseline = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4,\nsegment.ts\n#EXT-X-ENDLIST\n",
    );
    assert!(commit_fetched_manifest(&mut session, &baseline, &request, 100).is_ok());
    session.mark_pending_handoff_discontinuity(7);
    let baseline_body = session.transient.last_manifest_body.clone().expect("baseline manifest");
    let baseline_template = session.transient.last_manifest_template().expect("baseline template");
    let baseline_generation = session.transient.manifest_generation();
    let baseline_finalized_generation =
        session.transient.current_finalized_manifest_generation().expect("baseline finalized generation");
    let baseline_finalized_generation_count = session.transient.finalized_manifest_generation_count();
    let baseline_commit_identity = session.transient.last_manifest_commit_identity().expect("baseline identity");
    let baseline_resources = session.transient.resources.keys().cloned().collect::<std::collections::HashSet<_>>();
    let baseline_manifest_resource_ids = session.transient.current_manifest_resource_ids().clone();
    let baseline_highwater = session.origin_seq_highwater;
    let candidate = fetched_manifest(
        "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:2\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"rotated-key.bin\"\n#EXTINF:4,\nreplacement.ts\n#EXT-X-ENDLIST\n",
    );

    let result = commit_fetched_manifest(&mut session, &candidate, &request, 102);

    assert!(matches!(result, Err(HlsManifestCommitError::MalformedTransientRepresentation)));
    assert!(Arc::ptr_eq(
        session.transient.last_manifest_body.as_ref().expect("baseline remains current"),
        &baseline_body
    ));
    assert!(Arc::ptr_eq(
        &session.transient.last_manifest_template().expect("baseline template remains current"),
        &baseline_template
    ));
    assert_eq!(session.transient.manifest_generation(), baseline_generation);
    assert_eq!(session.transient.current_finalized_manifest_generation(), Some(baseline_finalized_generation));
    assert_eq!(session.transient.finalized_manifest_generation_count(), baseline_finalized_generation_count);
    assert_eq!(session.transient.last_manifest_commit_identity(), Some(baseline_commit_identity));
    assert_eq!(
        session.transient.resources.keys().cloned().collect::<std::collections::HashSet<_>>(),
        baseline_resources
    );
    assert_eq!(session.transient.current_manifest_resource_ids(), &baseline_manifest_resource_ids);
    assert_eq!(session.pending_handoff_discontinuity_sequence, Some(7));
    assert_eq!(session.origin_seq_highwater, baseline_highwater);
    assert!(session
        .transient
        .last_manifest_template()
        .zip(session.transient.last_manifest_commit_identity())
        .is_some());
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

#[test]
fn transient_commit_accepts_plausible_same_redirect_host_rollover_and_resets_highwater() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.mark_authorized_media_access(100);
    let previous_manifest =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
    session.transient.replace_manifest_with_semantics(previous_manifest.clone(), 10, None);
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(result.is_ok());
    assert_ne!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
    assert_eq!(session.origin_seq_highwater, Some(0));
    assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
}

#[test]
fn transient_commit_rejects_same_host_backward_manifest_outside_rollover_window() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.mark_authorized_media_access(100);
    let previous_manifest =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
    session.transient.replace_manifest_with_semantics(previous_manifest.clone(), 10, None);
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:226\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(matches!(result, Err(HlsManifestCommitError::TimelineRejected { .. })));
    assert_eq!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
    assert_eq!(session.origin_seq_highwater, Some(758));
}

#[test]
fn transient_commit_rebases_expired_session_highwater() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.target_duration = Some(12);
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.mark_authorized_media_access(1_000);
    let previous_manifest =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string();
    session.transient.replace_manifest_with_semantics(previous_manifest.clone(), 10, None);
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 40_000);

    assert!(result.is_ok());
    assert_ne!(session.transient.last_manifest_body.as_deref(), Some(previous_manifest.as_str()));
    assert_eq!(session.origin_seq_highwater, Some(900));
}

#[test]
fn transient_commit_accepts_monotonic_media_sequence_and_updates_highwater() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.transient.replace_manifest_with_semantics(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:757\n#EXTINF:4.0,\n/hls/shared/live/session/lease/r/old.ts\n".to_string(),
        10,
        None,
    );
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:759\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg759.ts\n#EXTINF:4.0,\nseg760.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(result.is_ok());
    assert_eq!(session.origin_seq_highwater, Some(760));
    assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
}

#[test]
fn transient_commit_with_different_redirect_host_is_held_as_candidate() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(758);
    session.last_effective_manifest_host = Some("previous.example.com".to_string());
    let request = test_origin_refresh_request(test_session());
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:758\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg758.ts\n#EXTINF:4.0,\nseg759.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(matches!(result, Err(HlsManifestCommitError::RetryCurrentTarget)));
    assert_eq!(session.origin_seq_highwater, Some(758));
    assert!(session.transient.last_manifest_body.is_none());
}

#[test]
fn fresh_revalidation_rebases_transient_manifest_on_the_pinned_host() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
    session.origin_seq_highwater = Some(1_000);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    let mut request = test_origin_refresh_request(test_session());
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ExpiredRevalidation,
    };
    let fetched = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n#EXTINF:4.0,\nseg10.ts\n",
    );

    let result = commit_fetched_manifest(&mut session, &fetched, &request, 100);

    assert!(result.is_ok());
    assert_eq!(session.origin_seq_highwater, Some(10));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("origin.example.com"));
    assert!(session.transient.last_manifest_body.as_ref().is_some_and(|body| body.contains("/r/")));
}

#[tokio::test]
async fn unsupported_tag_manifest_commits_transient_rewrite() {
    let session = refresh_session_with_origin_body(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-PART:DURATION=1.0,URI=\"part.m4s\"\n\
         #EXTINF:4.0,\nseg.ts\n",
    )
    .await;
    let session = session.read().await;

    assert!(matches!(
        session.mode,
        HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::UnsupportedTag { .. } }
    ));
    assert!(session.transient.last_manifest_body.is_some());
}

#[tokio::test]
async fn parser_unsupported_feature_manifest_commits_transient_rewrite() {
    let session = refresh_session_with_origin_body(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-BYTERANGE:10\n#EXTINF:4.0,\nseg.ts\n",
    )
    .await;
    let session = session.read().await;

    assert!(matches!(
        session.mode,
        HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ParserUnsupportedFeature { .. } }
    ));
    assert!(session.transient.last_manifest_body.is_some());
}

struct TestOriginServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    raw_requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

type TestOriginHandler = Arc<dyn Fn(String) -> (u16, Vec<(&'static str, String)>, String) + Send + Sync>;

impl Drop for TestOriginServer {
    fn drop(&mut self) { self.task.abort(); }
}

async fn spawn_test_origin(handler: TestOriginHandler) -> TestOriginServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_task = Arc::clone(&requests);
    let raw_requests = Arc::new(Mutex::new(Vec::new()));
    let raw_requests_for_task = Arc::clone(&raw_requests);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&requests_for_task);
            let raw_requests = Arc::clone(&raw_requests_for_task);
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let mut used = 0_usize;
                loop {
                    let Ok(read) = socket.read(&mut buf[used..]).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    used += read;
                    if used >= 4 && buf[..used].windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                    if used == buf.len() {
                        return;
                    }
                }
                let request = String::from_utf8_lossy(&buf[..used]).into_owned();
                let path =
                    request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/").to_string();
                requests.lock().await.push(path.clone());
                raw_requests.lock().await.push(request);
                let (status, headers, body) = handler(path);
                let reason = match status {
                    200 => "OK",
                    302 => "Found",
                    404 => "Not Found",
                    407 => "Proxy Authentication Required",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let mut response =
                    format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (name, value) in headers {
                    response.push_str(name);
                    response.push_str(": ");
                    response.push_str(&value);
                    response.push_str("\r\n");
                }
                response.push_str("\r\n");
                response.push_str(&body);
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    TestOriginServer { base_url: format!("http://{addr}"), requests, raw_requests, task }
}

fn request_header_value<'a>(request: &'a str, expected_name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(expected_name).then_some(value.trim())
    })
}

fn no_delay_policy() -> RetryPolicy { RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 } }

fn test_recovery_timing_policy(expected_eta_ms: u64) -> HlsRecoveryTimingPolicy {
    HlsRecoveryTimingPolicy::new(
        HlsOperationTimeoutMs::from_millis(2_000),
        HlsOperationTimeoutMs::from_millis(2_000),
        HlsRecoveryEtaMs::from_millis(expected_eta_ms),
        HlsRecoveryEtaMs::from_millis(expected_eta_ms),
    )
}

fn test_acceptance_episode_timing(
    started_at_ms: u64,
    burst_plan: shared::model::HlsManifestRecoveryBurstPlan,
) -> HlsAcceptanceEpisodeTiming {
    HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms,
        burst_plan,
        target_duration_ms: 4_000,
        transition_margin: HlsTransitionMarginMs::from_millis(4_000),
        workload: HlsRecoveryWorkload::clear_fetch(),
        observed_latency: HlsObservedRecoveryLatency::default(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
        policy: test_recovery_timing_policy(2_000),
    })
}

fn test_switch_staging_acceptance_episode_timing(
    started_at_ms: u64,
    burst_plan: shared::model::HlsManifestRecoveryBurstPlan,
) -> HlsAcceptanceEpisodeTiming {
    HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms,
        burst_plan,
        target_duration_ms: 4_000,
        transition_margin: HlsTransitionMarginMs::from_millis(4_000),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        observed_latency: HlsObservedRecoveryLatency::default(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
        policy: test_recovery_timing_policy(2_000),
    })
}

fn test_origin_refresh_request(session: Arc<RwLock<HlsSession>>) -> OriginRefreshRequest {
    let entry =
        LiveHlsOriginEntry::parse("http://origin.example.com/live/user/pass/12345.m3u8").expect("valid origin entry");
    OriginRefreshRequest {
        app_config: test_app_config(),
        session,
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
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
    }
}

fn bind_refresh_request_to_app_state(mut request: OriginRefreshRequest, ctx: &crate::HlsCtx) -> OriginRefreshRequest {
    request.segment_cache = Arc::clone(ctx.hls_proxy.segment_cache());
    request.hls_proxy = Arc::clone(&ctx.hls_proxy);
    request.segment_repair = Arc::clone(ctx.hls_proxy.segment_repair());
    request.segment_worker_pool = Arc::clone(ctx.hls_proxy.segment_worker_pool());
    request.map_worker_pool = Arc::clone(ctx.hls_proxy.map_worker_pool());
    request.post_refresh_runtime = Some(HlsPostRefreshRuntime { ctx: ctx.downgrade() });
    request
}

fn post_refresh_live_manifest_snapshot() -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 1,
        first_proxy_seq: 0,
        last_proxy_seq: 2,
        visible_segments: Arc::from([
            HlsLeaseManifestSegment {
                proxy_seq: 0,
                duration_ms: 4_000,
                uri: "0.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 1,
                duration_ms: 4_000,
                uri: "1.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 2,
                duration_ms: 4_000,
                uri: "2.ts".into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
        ]),
        discontinuity_sequence: 0,
        target_duration_ms: 4_000,
        playlist_duration_ms: 12_000,
        last_visible_media_end_ms: 12_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

fn fetched_manifest(body: &str) -> FetchedOriginManifest {
    FetchedOriginManifest {
        body: body.to_string(),
        final_manifest_url: "http://origin.example.com/live/final/index.m3u8".to_string(),
        resolved_request_url: "http://origin.example.com/live/user/pass/12345.m3u8".to_string(),
        redirect_host: Some("origin.example.com".to_string()),
        provider_url_index: None,
        provider_session_headers: HeaderMap::new(),
        status: StatusCode::OK,
        attempts: 1,
        candidate_requests: 1,
        selection: HlsManifestFetchSelection::Initial,
    }
}

#[test]
fn refresh_completion_diagnostics_separate_logical_recovery_from_beast_candidate_requests() {
    let initial = fetched_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n");
    let initial_diagnostic = HlsManifestRefreshCompletionDiagnostic::from_fetched(&initial);
    assert_eq!(initial_diagnostic.recovery_attempts, 0);
    assert_eq!(initial_diagnostic.candidate_requests, 1);
    assert_eq!(initial_diagnostic.selection, HlsManifestFetchSelection::Initial);

    let mut burst = initial;
    burst.attempts = 1;
    burst.candidate_requests = HlsManifestRecoveryBurstLevel::Beast.plan().total_candidates();
    burst.selection = HlsManifestFetchSelection::Burst;
    let burst_diagnostic = HlsManifestRefreshCompletionDiagnostic::from_fetched(&burst);
    assert_eq!(burst_diagnostic.recovery_attempts, 1);
    assert_eq!(burst_diagnostic.candidate_requests, HlsManifestRecoveryBurstLevel::Beast.plan().total_candidates());
    assert_eq!(burst_diagnostic.selection, HlsManifestFetchSelection::Burst);
}

#[test]
fn provider_failover_mirror_without_redirect_uses_resolved_host_signal() {
    let mut fetched = fetched_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n");
    fetched.redirect_host = None;
    fetched.resolved_request_url = "http://mirror.example.com/live/user/pass/12345.m3u8".to_string();
    fetched.provider_url_index = Some(1);

    assert_eq!(fetched_effective_manifest_host(&fetched).as_deref(), Some("mirror.example.com"));
}

#[test]
fn provider_failover_with_redirect_uses_redirect_host_as_manifest_host_signal() {
    let mut fetched = fetched_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.ts\n");
    fetched.redirect_host = Some("redirect.example.com".to_string());
    fetched.resolved_request_url = "http://mirror.example.com/live/user/pass/12345.m3u8".to_string();
    fetched.provider_url_index = Some(1);

    assert_eq!(fetched_effective_manifest_host(&fetched).as_deref(), Some("redirect.example.com"));
}

#[test]
fn manifest_redirect_host_is_only_set_for_actual_redirect_host_switch() {
    let resolved = Url::parse("http://mirror.example.com/live/user/pass/12345.m3u8").expect("resolved url");
    let same_target = Url::parse("http://mirror.example.com/live/user/pass/12345.m3u8").expect("same url");
    let redirected = Url::parse("http://cdn.example.net/live/play/12345.m3u8").expect("redirect url");

    assert_eq!(hls_manifest_redirect_host(&resolved, &same_target), None);
    assert_eq!(hls_manifest_redirect_host(&resolved, &redirected).as_deref(), Some("cdn.example.net"));
}

fn host_from_base_url(base_url: &str) -> String {
    url::Url::parse(base_url).expect("base url").host_str().expect("host").to_string()
}

fn manifest_body() -> String { "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg.ts\n".to_string() }

fn three_segment_manifest_body(media_sequence: u64) -> String {
    format!(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{media_sequence}\n#EXT-X-TARGETDURATION:4\n\
         #EXTINF:4.0,\n{media_sequence}.ts\n\
         #EXTINF:4.0,\n{}.ts\n\
         #EXTINF:4.0,\n{}.ts\n",
        media_sequence.saturating_add(1),
        media_sequence.saturating_add(2)
    )
}

async fn publish_ready_test_manifest(session: &Arc<RwLock<HlsSession>>, rendered_at_ms: u64) {
    let mut session = session.write().await;
    for segment in session.segments.values_mut() {
        segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms };
    }
    session.render_and_store_manifest(rendered_at_ms).expect("test live manifest publishes");
}

fn install_stored_provisioning_manifest(session: &mut HlsSession, rendered_at_ms: u64) {
    let segment_proxy_seqs = (0_u64..3).collect::<Vec<_>>();
    for proxy_seq in &segment_proxy_seqs {
        session.segments.insert(
            *proxy_seq,
            SegmentEntry {
                origin_key: OriginSegmentKey {
                    origin_epoch: HLS_PROVISIONING_ORIGIN_EPOCH,
                    effective_host_id: 0,
                    host_local_sequence: *proxy_seq,
                    host_local_index: u32::try_from(*proxy_seq).unwrap_or(u32::MAX),
                },
                proxy_seq: *proxy_seq,
                duration_ms: 2_000,
                proxy_file_ext: "ts".to_string(),
                content_type: "video/mp2t".to_string(),
                cache_key: SegmentCacheKey::new(session.proxy_session_id.clone(), *proxy_seq, "ts"),
                discontinuity_before: false,
                program_date_time: None,
                daterange_tags_before: Vec::new(),
                origin_byte_range: None,
                map_ref: None,
                encryption: None,
                origin_fetch_ref: None,
                status: SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
                last_rendered_at_ms: None,
                access: Arc::new(CacheAccessState::new()),
            },
        );
    }
    session.publishable_origin_head_proxy_seq = segment_proxy_seqs.first().copied();
    session.publishable_origin_tail_proxy_seq = segment_proxy_seqs.last().copied();
    session.proxy_next_seq = Some(3);
    session.target_duration = Some(2);
    assert_eq!(
        session.store_rendered_manifest(RenderedManifest {
            body: "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n".to_string(),
            first_proxy_seq: 0,
            last_proxy_seq: 2,
            discontinuity_sequence: 0,
            target_duration_ms: 2_000,
            playlist_duration_ms: 6_000,
            valid_until_ms: rendered_at_ms.saturating_add(6_000),
            render_gap_segments: 0,
            rendered_at_ms,
            segment_proxy_seqs,
        }),
        RenderedManifestStoreOutcome::Stored
    );
    assert!(session.published_live_origin_baseline.is_none());
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
async fn warm_retryable_initial_failure_does_not_open_acceptance_episode() {
    let server = spawn_test_origin(Arc::new(|_path| (500, Vec::new(), "retry".to_string()))).await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(10);
        session.origin_control.record_media_progress(50, 4_000);
    }
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");

    assert!(trigger_origin_refresh_sync(request.clone()).await);

    let manifest_requests =
        server.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, request.retry_policy.attempt_count());
    assert!(session.read().await.origin_control.acceptance_episode.is_none());
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
async fn shared_initial_manifest_automatic_cross_origin_redirect_keeps_identity_and_scrubs_credentials() {
    let target = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let target_url = format!("{}/final/manifest.m3u8", target.base_url);
    let expected_target_url = target_url.clone();
    let redirect =
        spawn_test_origin(Arc::new(move |_path| (302, vec![("Location", target_url.clone())], String::new()))).await;
    let origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", redirect.base_url)).expect("entry url");
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer origin-secret"));
    headers.insert(header::COOKIE, HeaderValue::from_static("sid=origin-secret"));
    let context = HlsOriginManifestFetchContext {
        app_config: test_app_config(),
        session: test_session(),
        origin_entry,
        headers,
        client: reqwest::Client::builder().no_proxy().build().expect("client"),
        no_redirect_client: reqwest::Client::builder()
            .no_proxy()
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
        .expect("automatic redirect should return the decoded manifest");

    assert_eq!(fetched.body, manifest_body());
    assert_eq!(fetched.attempts, 1);
    assert_eq!(fetched.final_manifest_url, expected_target_url);
    let redirect_requests = redirect.raw_requests.lock().await;
    let target_requests = target.raw_requests.lock().await;
    assert_eq!(redirect_requests.len(), 1);
    assert_eq!(target_requests.len(), 1);
    assert_eq!(request_header_value(&redirect_requests[0], "accept-encoding"), Some("identity"));
    assert_eq!(request_header_value(&target_requests[0], "accept-encoding"), Some("identity"));
    assert_eq!(request_header_value(&redirect_requests[0], "authorization"), Some("Bearer origin-secret"));
    assert_eq!(request_header_value(&redirect_requests[0], "cookie"), Some("sid=origin-secret"));
    assert!(request_header_value(&target_requests[0], "authorization").is_none());
    assert!(request_header_value(&target_requests[0], "cookie").is_none());
}

#[tokio::test]
async fn shared_initial_manifest_budget_covers_status_decoder_redirect_and_success() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let target_hits_for_handler = Arc::clone(&target_hits);
    let server = spawn_test_origin(Arc::new(move |path| {
        if path == "/live/user/pass/12345.m3u8" {
            return (302, vec![("Location", "/live/play/once/12345".to_string())], String::new());
        }
        if path == "/live/play/once/12345" {
            return match target_hits_for_handler.fetch_add(1, Ordering::SeqCst) {
                0 => (500, Vec::new(), "temporary".to_string()),
                1 => (200, vec![("Content-Encoding", "gzip".to_string())], "corrupt-gzip".to_string()),
                _ => (200, Vec::new(), manifest_body()),
            };
        }
        (404, Vec::new(), String::new())
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
        use_manual_redirects: true,
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        retry_policy: no_delay_policy(),
        recovery_timing_policy: test_recovery_timing_policy(2_000),
        acceptance_timing_seed: None,
    };

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("third logical attempt should succeed");

    assert_eq!(fetched.body, manifest_body());
    assert_eq!(fetched.attempts, 3);
    assert_eq!(
        *server.requests.lock().await,
        vec![
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
        ]
    );
    assert!(server
        .raw_requests
        .lock()
        .await
        .iter()
        .all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
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
async fn manifest_retry_starts_at_entrypoint_after_redirect_failure() {
    let redirect_hits = Arc::new(AtomicUsize::new(0));
    let redirect_hits_for_handler = Arc::clone(&redirect_hits);
    let server = spawn_test_origin(Arc::new(move |path| {
        if path == "/live/user/pass/12345.m3u8" {
            return (302, vec![("Location", "/live/play/once/12345".to_string())], String::new());
        }
        if path == "/live/play/once/12345" {
            let hit = redirect_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit < 2 {
                return (500, Vec::new(), "fail".to_string());
            }
            return (200, Vec::new(), manifest_body());
        }
        (404, Vec::new(), String::new())
    }))
    .await;
    let entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    let no_redirect_client =
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client builds");

    let fetched = refresh_from_live_hls_entrypoint_with_retries(
        &entry,
        &HeaderMap::new(),
        &reqwest::Client::new(),
        &no_redirect_client,
        true,
        2_000,
        &no_delay_policy(),
    )
    .await
    .expect("refresh eventually succeeds");

    assert_eq!(fetched.attempts, 3);
    assert_eq!(
        *server.requests.lock().await,
        vec![
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345",
            "/live/user/pass/12345.m3u8",
            "/live/play/once/12345"
        ]
    );
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
async fn provider_failover_status_does_not_count_as_hls_retry() {
    let first = spawn_test_origin(Arc::new(|_path| (407, Vec::new(), "rotate".to_string()))).await;
    let second = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    let initial_session_key = session.read().await.key.stable_value();
    let initial_proxy_session_id = session.read().await.proxy_session_id.clone();
    let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8",
        Some(Arc::clone(&provider)),
    )
    .expect("provider entry url");
    let segment_worker_pool = Arc::new(HlsSegmentWorkerPool::default());
    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::clone(&segment_worker_pool),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
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

    assert!(maybe_trigger_origin_refresh(request).await);
    for _ in 0..50 {
        if session.read().await.origin_seq_highwater == Some(102) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let first_manifest_requests =
        first.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    let second_manifest_requests =
        second.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(first_manifest_requests, 1);
    assert_eq!(second_manifest_requests, 1);
    for server in [&first, &second] {
        let manifest_requests = server
            .raw_requests
            .lock()
            .await
            .iter()
            .filter(|request| request.lines().next().is_some_and(|line| line.contains(" /live/user/pass/12345.m3u8 ")))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(manifest_requests.len(), 1);
        assert_eq!(request_header_value(&manifest_requests[0], "accept-encoding"), Some("identity"));
    }
    let session = session.read().await;
    assert_eq!(session.key.stable_value(), initial_session_key);
    assert_eq!(session.proxy_session_id, initial_proxy_session_id);
    assert!(!session.key.stable_value().contains("provider://"));
    assert!(!session.key.stable_value().contains(first.base_url.as_str()));
    assert!(!session.key.stable_value().contains(second.base_url.as_str()));
    assert_eq!(session.origin_seq_highwater, Some(0));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("127.0.0.1"));
    let metrics = segment_worker_pool.metrics().snapshot();
    assert_eq!(metrics.refresh_started, 1);
    assert_eq!(metrics.refresh_completed, 1);
    assert_eq!(metrics.refresh_retried, 0);
    assert_eq!(metrics.refresh_failed, 0);
}

#[tokio::test]
async fn established_recovery_burst_uses_one_fixed_provider_url_for_every_candidate() {
    let phase = Arc::new(AtomicUsize::new(0));
    let phase_for_first = Arc::clone(&phase);
    let first = spawn_test_origin(Arc::new(move |_path| {
        let media_sequence = if phase_for_first.load(Ordering::SeqCst) == 0 { 100 } else { 103 };
        (200, Vec::new(), three_segment_manifest_body(media_sequence))
    }))
    .await;
    let second = spawn_test_origin(Arc::new(|_path| (500, Vec::new(), "unexpected".to_string()))).await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8?token=fixed",
        Some(Arc::clone(&provider)),
    )
    .expect("provider entry url");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };

    let baseline = fetch_and_commit_manifest_with_policy(&mut request).await.expect("initial baseline commits");
    assert_eq!(baseline.fetched.provider_url_index, Some(0));
    let concrete_request_url = format!("{}/live/user/pass/12345.m3u8?token=fixed", first.base_url);
    {
        let session = session.read().await;
        let binding =
            session.origin_control.manifest_origin_binding.as_ref().expect("successful commit stores binding");
        assert_eq!(binding.request_url().as_str(), concrete_request_url);
        assert_eq!(binding.provider_url_index(), Some(0));
    }
    publish_ready_test_manifest(&session, 200).await;
    assert!(session.read().await.established_manifest_recovery_binding().is_some());

    phase.store(1, Ordering::SeqCst);
    let provider_index_before_burst = provider.get_current_index();
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
    let selected =
        fetch_and_commit_manifest_with_policy(&mut request).await.expect("established fixed-binding burst commits");
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let first_requests = first.requests.lock().await;
    assert_eq!(first_requests.len(), 1_usize.saturating_add(plan.total_candidates()));
    assert!(first_requests.iter().all(|path| path == "/live/user/pass/12345.m3u8?token=fixed"));
    assert!(second.requests.lock().await.is_empty());
    assert_eq!(provider.get_current_index(), provider_index_before_burst);
    assert_eq!(selected.fetched.provider_url_index, Some(0));
    assert_eq!(selected.fetched.candidate_requests, plan.total_candidates());
    assert_eq!(selected.fetched.selection, HlsManifestFetchSelection::Burst);
    assert_eq!(session.read().await.origin_seq_highwater, Some(105));
}

#[tokio::test]
async fn successful_redirected_baseline_binds_the_original_request_entry() {
    let target = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), three_segment_manifest_body(100)))).await;
    let target_url = format!("{}/redirected/final.m3u8", target.base_url);
    let target_url_for_handler = target_url.clone();
    let entry = spawn_test_origin(Arc::new(move |_path| {
        (302, vec![("Location", target_url_for_handler.clone())], String::new())
    }))
    .await;
    let request_url = format!("{}/live/user/pass/12345.m3u8?token=fixed", entry.base_url);
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = LiveHlsOriginEntry::parse(&request_url).expect("redirecting entry URL");

    let committed =
        fetch_and_commit_manifest_with_policy(&mut request).await.expect("redirected initial manifest commits");

    assert_eq!(committed.fetched.final_manifest_url, target_url);
    assert_eq!(committed.fetched.resolved_request_url, request_url);
    let binding_url = {
        let session = session.read().await;
        session
            .origin_control
            .manifest_origin_binding
            .as_ref()
            .expect("redirected commit stores binding")
            .request_url()
            .to_string()
    };
    assert_eq!(binding_url, request_url);
    assert_eq!(entry.requests.lock().await.as_slice(), ["/live/user/pass/12345.m3u8?token=fixed"]);
    assert_eq!(target.requests.lock().await.as_slice(), ["/redirected/final.m3u8"]);
}

fn assert_binding_superseded_error(error: &OriginManifestFetchError) {
    assert!(matches!(
        error,
        OriginManifestFetchError::RecoveryUnavailable {
            reason: HlsManifestRecoveryUnavailableReason::BindingSuperseded,
        }
    ));
    assert_eq!(error.log_label(), "recovery_binding_superseded");
    assert_eq!(error.to_string(), "origin manifest recovery unavailable: manifest origin binding superseded");
    assert_eq!(
        classify_manifest_fetch_failure(error),
        HlsManifestFetchFailureSignal::discarded(HlsManifestFetchFailureKind::Superseded)
    );
}

#[tokio::test]
async fn recovery_binding_supersession_is_discarded_without_failure_signal() {
    let origin = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), three_segment_manifest_body(100)))).await;
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let session = Arc::new(RwLock::with_max_readers(
        HlsSession::new(HlsSessionKey::new(1, "binding-supersession"), b"secret", 0),
        1,
    ));
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), ctx);
    let baseline_binding_url = format!("{}/live/a/index.m3u8", origin.base_url);
    request.origin_entry = LiveHlsOriginEntry::parse(&baseline_binding_url).expect("binding A entry URL");
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;

    fetch_and_commit_manifest_with_policy(&mut request).await.expect("binding A baseline commits");
    publish_ready_test_manifest(&session, 200).await;
    let baseline_binding =
        session.read().await.established_manifest_recovery_binding().expect("published baseline has binding A");
    let requests_before_supersession = origin.requests.lock().await.len();

    let replacement_binding_url = format!("{}/live/b/index.m3u8", origin.base_url);
    let mut binding_b_manifest = fetched_manifest(&three_segment_manifest_body(103));
    binding_b_manifest.resolved_request_url.clone_from(&replacement_binding_url);
    binding_b_manifest.final_manifest_url.clone_from(&replacement_binding_url);
    binding_b_manifest.redirect_host = Some(host_from_base_url(&origin.base_url));
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    assert!(mark_origin_refresh_started(&mut request, 300).await);
    let verification_request = request.clone();
    let replacement_request = request.clone();
    let read_blocker = session.read().await;
    let refresh = tokio::spawn(refresh_and_commit(request, 300));
    tokio::task::yield_now().await;
    let replacement_session = Arc::clone(&session);
    let replacement = tokio::spawn(async move {
        let mut session = replacement_session.write().await;
        commit_fetched_manifest(&mut session, &binding_b_manifest, &replacement_request, 325)
            .expect("newer binding B manifest commits");
        session.origin_control.record_origin_response(275);
        session.origin_control.path_condition = super::super::origin_progress::HlsOriginPathCondition::PublicationLate;
        session.origin_refresh.consecutive_failures = 3;
        session.origin_refresh.last_error_at_ms = Some(250);
    });
    tokio::task::yield_now().await;
    drop(read_blocker);
    replacement.await.expect("replacement binding task joins");
    refresh.await.expect("superseded refresh task joins");

    let metrics_after = metrics.snapshot();
    let replacement_binding = {
        let session = session.read().await;
        assert_eq!(session.origin_control.last_origin_response_at_ms, Some(275));
        assert_eq!(
            session.origin_control.path_condition,
            super::super::origin_progress::HlsOriginPathCondition::PublicationLate
        );
        assert_eq!(session.origin_refresh.consecutive_failures, 3);
        assert_eq!(session.origin_refresh.last_error_at_ms, Some(250));
        assert!(!session.origin_refresh.in_flight);
        assert!(session.origin_control.acceptance_episode.is_none());
        session.origin_control.manifest_origin_binding.clone().expect("newer binding B remains installed")
    };
    assert_ne!(baseline_binding, replacement_binding);
    assert_eq!(metrics_after.refresh_started, 1);
    assert_eq!(metrics_after.refresh_completed, 0);
    assert_eq!(metrics_after.refresh_retried, 0);
    assert_eq!(metrics_after.refresh_failed, 0);
    assert_eq!(metrics_after.refresh_skipped, 1);
    assert_eq!(ctx.hls_proxy.availability_reevaluations().owner_count(), 0);
    assert_eq!(origin.requests.lock().await.len(), requests_before_supersession);

    let Err(error) = super::recover_manifest_for_request(
        &manifest_fetch_context(&verification_request),
        &verification_request,
        super::HlsManifestRecoveryPath {
            binding: baseline_binding,
            reject_reason: None,
            deterministic_conflict: None,
            trigger: HlsManifestAcceptanceTrigger::RecoveryRequired,
            diagnostic: super::HlsRecoveryTriggerDiagnostic::new(super::HlsRecoveryTriggerSource::Other),
        },
    )
    .await
    else {
        panic!("superseded recovery binding must be discarded");
    };
    assert_binding_superseded_error(&error);
    assert_eq!(origin.requests.lock().await.len(), requests_before_supersession);
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
async fn initial_provider_failover_does_not_leak_into_established_burst() {
    let phase = Arc::new(AtomicUsize::new(0));
    let phase_for_first = Arc::clone(&phase);
    let first = spawn_test_origin(Arc::new(move |_path| {
        if phase_for_first.load(Ordering::SeqCst) == 0 {
            (200, Vec::new(), three_segment_manifest_body(100))
        } else {
            (407, Vec::new(), "rotate".to_string())
        }
    }))
    .await;
    let phase_for_second = Arc::clone(&phase);
    let second = spawn_test_origin(Arc::new(move |_path| {
        if phase_for_second.load(Ordering::SeqCst) == 0 {
            (500, Vec::new(), "unexpected".to_string())
        } else {
            (461, Vec::new(), "final-hard-failure".to_string())
        }
    }))
    .await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8?token=fixed",
        Some(Arc::clone(&provider)),
    )
    .expect("provider entry url");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };

    fetch_and_commit_manifest_with_policy(&mut request).await.expect("initial provider URL commits baseline");
    publish_ready_test_manifest(&session, 200).await;
    let binding =
        session.read().await.established_manifest_recovery_binding().expect("published baseline has fixed binding");
    assert_eq!(binding.provider_url_index(), Some(0));
    let provider_index_before_failure = provider.get_current_index();

    phase.store(1, Ordering::SeqCst);
    let Err(error) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("hard initial provider cycle and fixed-target recovery must remain failed");
    };
    assert!(matches!(error, OriginManifestFetchError::RetryableStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED, _)));

    let plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
    let first_requests = first.requests.lock().await;
    let second_requests = second.requests.lock().await;
    assert!(
        first_requests.len() >= 2_usize.saturating_add(plan.total_candidates()),
        "baseline, ordinary provider attempt, and at least one full fixed-target burst are required"
    );
    assert_eq!(second_requests.len(), 1, "only the ordinary provider cycle may reach URL index 1");
    assert!(first_requests.iter().all(|path| path == "/live/user/pass/12345.m3u8?token=fixed"));
    assert_eq!(second_requests[0], "/live/user/pass/12345.m3u8?token=fixed");
    assert_eq!(
        provider.get_current_index(),
        provider_index_before_failure,
        "fixed-target recovery must not rotate provider state"
    );
    assert_eq!(
        session
            .read()
            .await
            .origin_control
            .manifest_origin_binding
            .as_ref()
            .map(HlsManifestOriginBinding::provider_url_index),
        Some(Some(0))
    );
}

#[tokio::test]
async fn expired_acceptance_deadline_still_completes_first_beast_burst_without_follow_up() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let origin = spawn_test_origin(Arc::new(|_path| (407, Vec::new(), "retry".to_string()))).await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url))
        .expect("deadline test origin entry");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let target_url = request.origin_entry.url().clone();
    let mut context = manifest_fetch_context(&request);
    let mut expired_after_full_burst = test_recovery_timing_policy(0);
    expired_after_full_burst.evaluation_eta = HlsRecoveryEtaMs::default();
    expired_after_full_burst.commit_eta = HlsRecoveryEtaMs::default();
    expired_after_full_burst.scheduling_eta = HlsRecoveryEtaMs::default();
    context.recovery_timing_policy = expired_after_full_burst;

    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(target_url),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await;

    assert!(result.is_err());
    let manifest_requests =
        origin.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, plan.total_candidates());
    let session = session.read().await;
    let episode = session.origin_control.acceptance_episode.as_ref().expect("completed first acceptance burst");
    assert!(episode.full_burst_completed);
    assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
    assert_eq!(episode.full_bursts_completed, 1);
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
async fn cold_start_hard_initial_failure_does_not_start_acceptance_burst() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let server = spawn_test_origin(Arc::new(|_path| (404, Vec::new(), "missing".to_string()))).await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };
    let metrics = Arc::clone(request.segment_worker_pool.metrics());

    assert!(trigger_origin_refresh_sync(request).await);

    let manifest_requests =
        server.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, 1);
    assert_ne!(manifest_requests, 1_usize.saturating_add(plan.total_candidates()));
    let session = session.read().await;
    assert!(session.origin_control.acceptance_episode.is_none());
    assert_eq!(session.origin_control.progress_phase, super::super::origin_progress::HlsOriginProgressPhase::Cold);
    assert_eq!(
        session.origin_control.path_condition,
        super::super::origin_progress::HlsOriginPathCondition::HardFetchFailure
    );
    let metrics = metrics.snapshot();
    assert_eq!(metrics.refresh_failed, 1);
    assert_eq!(metrics.refresh_completed, 0);
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
    assert_eq!(session.origin_control.progress_phase, super::super::origin_progress::HlsOriginProgressPhase::Cold);
}

#[tokio::test]
async fn cold_provider_failover_cycle_does_not_start_acceptance_burst() {
    let first = spawn_test_origin(Arc::new(|_path| (500, Vec::new(), "rotate".to_string()))).await;
    let second = spawn_test_origin(Arc::new(|_path| (461, Vec::new(), "final-hard-failure".to_string()))).await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8",
        Some(provider),
    )
    .expect("provider entry URL");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement =
        HlsManifestCommitRequirement::FreshCommitRequired { reason: HlsFreshManifestRequiredReason::ColdStart };

    let Err(error) = fetch_and_commit_manifest_with_policy(&mut request).await else {
        panic!("cold provider cycle must preserve its final hard response");
    };

    assert!(matches!(
        error,
        OriginManifestFetchError::NonRetryableStatus(status) if status.as_u16() == 461
    ));
    assert_eq!(first.requests.lock().await.len(), 1);
    assert_eq!(second.requests.lock().await.len(), 1);
    let session = session.read().await;
    assert!(session.origin_control.acceptance_episode.is_none());
    assert!(session.origin_control.manifest_origin_binding.is_none());
    assert_eq!(session.origin_control.progress_phase, super::super::origin_progress::HlsOriginProgressPhase::Cold);
}

#[tokio::test]
async fn provisioning_handoff_without_established_origin_baseline_uses_one_initial_fetch() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let server = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let session = test_session();
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry url");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::ProvisioningHandoff,
    };
    assert_eq!(manifest_recovery_trigger(&request), HlsManifestAcceptanceTrigger::Critical);

    assert!(trigger_origin_refresh_sync(request).await);

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
async fn successful_origin_commit_without_renderable_origin_window_does_not_enable_burst() {
    let manifest_hits = Arc::new(AtomicUsize::new(0));
    let manifest_hits_for_handler = Arc::clone(&manifest_hits);
    let server = spawn_test_origin(Arc::new(move |_path| {
        let media_sequence = if manifest_hits_for_handler.fetch_add(1, Ordering::SeqCst) == 0 { 100 } else { 103 };
        (200, Vec::new(), three_segment_manifest_body(media_sequence))
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        install_stored_provisioning_manifest(&mut session, 10);
    }
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url)).expect("entry URL");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };

    let baseline = fetch_and_commit_manifest_with_policy(&mut request).await.expect("ordinary origin baseline commits");
    assert_eq!(baseline.fetched.selection, HlsManifestFetchSelection::Initial);
    assert_eq!(baseline.fetched.candidate_requests, 1);
    {
        let session = session.read().await;
        assert!(session.origin_control.manifest_origin_binding.is_some());
        assert!(session.origin_control.pinned_host.is_some());
        assert_eq!(session.origin_seq_highwater, Some(102));
        assert_eq!(session.origin_control.progress_phase, super::super::origin_progress::HlsOriginProgressPhase::Fresh);
        assert_eq!(
            session.last_rendered_manifest.as_ref().expect("provisioning manifest remains stored").segment_proxy_seqs,
            vec![0, 1, 2]
        );
        assert!(session
            .segments
            .values()
            .filter(|segment| segment.origin_key.origin_epoch != HLS_PROVISIONING_ORIGIN_EPOCH)
            .all(|segment| !matches!(segment.status, SegmentCacheStatus::Ready { .. })));
        assert!(session.published_live_origin_baseline.is_none());
        assert!(session.established_manifest_recovery_binding().is_none());
        assert!(session.origin_control.acceptance_episode.is_none());
    }

    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::RecoveryRequired;
    let follow_up =
        fetch_and_commit_manifest_with_policy(&mut request).await.expect("suppressed recovery uses ordinary fetch");

    assert_eq!(follow_up.fetched.selection, HlsManifestFetchSelection::Initial);
    assert_eq!(follow_up.fetched.candidate_requests, 1);
    assert_eq!(manifest_hits.load(Ordering::SeqCst), 2);
    assert_ne!(manifest_hits.load(Ordering::SeqCst), HlsManifestRecoveryBurstLevel::Beast.plan().total_candidates());
    let session = session.read().await;
    assert!(session.published_live_origin_baseline.is_none());
    assert!(session.established_manifest_recovery_binding().is_none());
    assert!(session.origin_control.acceptance_episode.is_none());
}

#[tokio::test]
async fn different_host_single_candidate_retries_do_not_prove_acceptance() {
    let candidate_hits = Arc::new(AtomicUsize::new(0));
    let candidate_hits_for_handler = Arc::clone(&candidate_hits);
    let candidate = spawn_test_origin(Arc::new(move |_path| {
        candidate_hits_for_handler.fetch_add(1, Ordering::SeqCst);
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n"
                .to_string(),
        )
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.origin_control.pinned_host = Some("previous.example.com".to_string());
    }
    let candidate_entry_url =
        format!("{}/live/user/pass/12345.m3u8", candidate.base_url).replacen("127.0.0.1", "localhost", 1);
    install_published_recovery_binding(&session, &candidate_entry_url, None).await;
    let entry = LiveHlsOriginEntry::parse(&candidate_entry_url).expect("entry url");
    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
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

    assert!(trigger_origin_refresh_sync(request).await);

    let candidate_requests =
        candidate.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(candidate_requests, 6);
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(100));
    assert_eq!(session.origin_epoch, 0);
    let episode = session.origin_control.acceptance_episode.as_ref().expect("acceptance episode remains held");
    assert!(episode.full_burst_completed);
    assert_eq!(episode.full_bursts_completed, 1);
    assert_eq!(episode.trigger(), HlsManifestAcceptanceTrigger::Observe);
    assert_eq!(episode.state, super::super::manifest_acceptance::HlsManifestAcceptanceState::Holding);
    assert_eq!(
        session.origin_control.path_condition,
        super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict
    );
}

#[tokio::test]
async fn manifest_recovery_burst_skips_rejected_candidate() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let origin = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        if hit == 0 {
            return (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:50\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n50.bin\n".to_string(),
            );
        }
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n".to_string(),
        )
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some(host_from_base_url(&origin.base_url));
        session.mark_authorized_media_access(super::current_time_millis());
    }
    let entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("entry url");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };

    let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("target url");
    let committed = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await
    .expect("burst should commit accepted candidate");

    assert_eq!(committed.fetched.attempts, 1);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(session.read().await.origin_seq_highwater, Some(101));
}

#[test]
fn manifest_recovery_candidate_score_prefers_same_host_next_sequence() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    session.origin_seq_highwater = Some(100);
    session.last_effective_manifest_host = Some("origin.example.com".to_string());
    session.mark_authorized_media_access(super::current_time_millis());
    let request = test_origin_refresh_request(test_session());
    let fetch_context = manifest_fetch_context(&request);
    let same_host_unchanged =
        fetched_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n100.ts\n");
    let same_host_next =
        fetched_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n");
    let mut other_host_next = same_host_next.clone();
    other_host_next.redirect_host = Some("other.example.com".to_string());

    assert_eq!(
        score_manifest_recovery_candidate(&session, &same_host_unchanged, &fetch_context).expect("score").quality.score,
        HlsManifestOriginQualityScore::SameHostUnchanged
    );
    assert_eq!(
        score_manifest_recovery_candidate(&session, &same_host_next, &fetch_context).expect("score").quality.score,
        HlsManifestOriginQualityScore::SameHostNextSequence
    );
    let other_host_score =
        score_manifest_recovery_candidate(&session, &other_host_next, &fetch_context).expect("score").quality;
    assert_eq!(other_host_score.score, HlsManifestOriginQualityScore::OtherHostCandidate);
    assert!(other_host_score.requires_handoff_discontinuity);
}

#[tokio::test]
async fn manifest_recovery_burst_commits_best_same_host_candidate() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let origin = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        if hit == 0 {
            return (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n100.ts\n".to_string(),
            );
        }
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n".to_string(),
        )
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some(host_from_base_url(&origin.base_url));
        session.mark_authorized_media_access(super::current_time_millis());
    }
    let entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("entry url");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.origin_entry = entry;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };

    let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("target url");
    let committed = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await
    .expect("burst should commit best same-host candidate");

    assert_eq!(committed.fetched.attempts, 1);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(session.read().await.origin_seq_highwater, Some(101));
}

#[tokio::test]
async fn provider_failover_initial_success_commits_without_hls_host_retry_when_unpinned() {
    let first = spawn_test_origin(Arc::new(|_path| (407, Vec::new(), "rotate".to_string()))).await;
    let second_hits = Arc::new(AtomicUsize::new(0));
    let second_hits_for_handler = Arc::clone(&second_hits);
    let second = spawn_test_origin(Arc::new(move |_path| {
        let hit = second_hits_for_handler.fetch_add(1, Ordering::SeqCst);
        if hit == 0 {
            return (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:102\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n102.ts\n".to_string(),
            );
        }
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n"
                .to_string(),
        )
    }))
    .await;
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec![first.base_url.as_str().into(), second.base_url.as_str().into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let session = test_session();
    {
        session.write().await.origin_seq_highwater = Some(100);
    }
    let entry = LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8",
        Some(Arc::clone(&provider)),
    )
    .expect("provider entry url");
    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 0 },
        retry_policy: no_delay_policy(),
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

    assert!(maybe_trigger_origin_refresh(request).await);
    for _ in 0..50 {
        if session.read().await.origin_seq_highwater == Some(102) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let first_manifest_requests =
        first.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    let second_manifest_requests =
        second.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(first_manifest_requests, 1);
    assert_eq!(second_manifest_requests, 1);
    assert_eq!(session.read().await.origin_seq_highwater, Some(102));
}

#[tokio::test]
async fn host_switch_conflict_runs_one_full_plan_then_only_bounded_follow_ups() {
    let candidate_hits = Arc::new(AtomicUsize::new(0));
    let candidate_hits_for_handler = Arc::clone(&candidate_hits);
    let candidate = spawn_test_origin(Arc::new(move |_path| {
        candidate_hits_for_handler.fetch_add(1, Ordering::SeqCst);
        (
            200,
            Vec::new(),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n900.ts\n".to_string(),
        )
    }))
    .await;
    let session = test_session();
    {
        let mut session = session.write().await;
        session.origin_seq_highwater = Some(100);
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.origin_control.pinned_host = Some("previous.example.com".to_string());
    }
    let candidate_entry_url =
        format!("{}/live/user/pass/12345.m3u8", candidate.base_url).replacen("127.0.0.1", "localhost", 1);
    install_published_recovery_binding(&session, &candidate_entry_url, None).await;
    let entry = LiveHlsOriginEntry::parse(&candidate_entry_url).expect("entry url");
    let request = OriginRefreshRequest {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        origin_entry: entry.clone(),
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::new(HlsSegmentCache::new()),
        hls_proxy: Arc::new(HlsProxyManager::new()),
        segment_repair: test_segment_repair_manager(),
        segment_worker_pool: Arc::new(HlsSegmentWorkerPool::default()),
        map_worker_pool: Arc::new(HlsMapWorkerPool::default()),
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        strip: StripConfig { mode: HlsStripMode::Segments, value: 3 },
        retry_policy: no_delay_policy(),
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

    assert!(trigger_origin_refresh_sync(request).await);

    assert_eq!(candidate_hits.load(Ordering::SeqCst), 6);
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(100));
    let episode = session.origin_control.acceptance_episode.as_ref().expect("held acceptance episode");
    assert_eq!(episode.full_bursts_completed, 1);
}

#[tokio::test]
async fn material_reduced_retry_timeline_change_starts_one_new_complete_configured_burst() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let origin = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        let resource = if hit < plan.total_candidates() { "old900.ts" } else { "new900.ts" };
        (
            200,
            Vec::new(),
            format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n{resource}\n"),
        )
    }))
    .await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let cache_dir = tempfile::tempdir().expect("requalification cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let mut request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let target_url =
        Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("requalification target");
    let attempts = request.retry_policy.attempt_count();
    let context = manifest_fetch_context(&request);

    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(target_url),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::Observe,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await;

    assert!(result.is_err());
    let reduced_attempts = attempts.saturating_sub(2);
    assert_eq!(hits.load(Ordering::SeqCst), plan.total_candidates().saturating_mul(2).saturating_add(reduced_attempts));
    let session = session.read().await;
    let episode = session.origin_control.acceptance_episode.as_ref().expect("requalified episode");
    assert!(episode.generation.0 >= 2);
    assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
    assert!(episode.full_burst_completed);
    assert_eq!(episode.full_bursts_completed, 1);
}

#[tokio::test]
async fn material_change_in_last_reduced_slot_still_runs_new_episode_full_burst() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let retry_policy = no_delay_policy();
    let attempts = retry_policy.attempt_count();
    let unchanged_candidate_count = plan.total_candidates().saturating_add(attempts.saturating_sub(2));
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let origin = spawn_test_origin(Arc::new(move |_path| {
        let hit = hits_for_handler.fetch_add(1, Ordering::SeqCst);
        let resource = if hit < unchanged_candidate_count { "old900.ts" } else { "new900.ts" };
        (
            200,
            Vec::new(),
            format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n{resource}\n"),
        )
    }))
    .await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let cache_dir = tempfile::tempdir().expect("last-slot requalification cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let mut request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    request.retry_policy = retry_policy;
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url))
        .expect("last-slot requalification target");
    let context = manifest_fetch_context(&request);

    let result = retry_hls_origin_manifest_recovery_chain(
        &context,
        test_manifest_origin_binding(target_url),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::Observe,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        plan.total_candidates().saturating_mul(2).saturating_add(attempts.saturating_sub(1))
    );
    let session = session.read().await;
    let episode = session.origin_control.acceptance_episode.as_ref().expect("last-slot requalified episode");
    assert!(episode.generation.0 >= 2);
    assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
    assert!(episode.full_burst_completed);
    assert_eq!(episode.full_bursts_completed, 1);
}

const SWITCH_MAP_BODY: &[u8] = b"complete-switch-map";
const SWITCH_SEGMENT_BODY: &[u8] = b"complete-switch-segment-body";
const CRITICAL_HANDOFF_TS_BODY: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
const CRITICAL_HANDOFF_MANIFEST_BODY: &[u8] = b"#EXTM3U\n\
    #EXT-X-MEDIA-SEQUENCE:900\n\
    #EXT-X-TARGETDURATION:4\n\
    #EXTINF:4.0,\nfirst.ts\n\
    #EXTINF:4.0,\nsecond.ts\n";

struct ControlledSwitchOriginServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    segment_prefix_written: Arc<Notify>,
    release_segment_body: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ControlledSwitchOriginServer {
    fn drop(&mut self) { self.task.abort(); }
}

async fn await_controlled_switch_segment_prefix<T: std::fmt::Debug>(
    origin: &ControlledSwitchOriginServer,
    task: &mut tokio::task::JoinHandle<T>,
    task_name: &str,
) {
    tokio::select! {
        () = origin.segment_prefix_written.notified() => {}
        result = &mut *task => {
            panic!("{task_name} completed before controlled segment staging reached the network boundary: {result:?}");
        }
        () = tokio::time::sleep(Duration::from_secs(10)) => {
            panic!("{task_name} did not reach the controlled segment staging boundary before the test deadline");
        }
    }
}

struct CriticalEmergencyOriginServer {
    base_url: String,
    manifest_requests: Arc<AtomicUsize>,
    segment_requests: Arc<AtomicUsize>,
    segment_prefix_written: Arc<Notify>,
    release_segment_body: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for CriticalEmergencyOriginServer {
    fn drop(&mut self) { self.task.abort(); }
}

async fn read_test_request_path(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = vec![0_u8; 4_096];
    let mut used = 0_usize;
    loop {
        let read = socket.read(&mut buf[used..]).await.ok()?;
        if read == 0 {
            return None;
        }
        used = used.saturating_add(read);
        if used >= 4 && buf[..used].windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if used == buf.len() {
            return None;
        }
    }
    String::from_utf8_lossy(&buf[..used])
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_string)
}

async fn write_test_response(socket: &mut tokio::net::TcpStream, status: u16, body: &[u8]) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body).await;
}

async fn spawn_controlled_switch_origin() -> ControlledSwitchOriginServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("controlled switch origin binds");
    let addr = listener.local_addr().expect("controlled switch origin address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_task = Arc::clone(&requests);
    let segment_prefix_written = Arc::new(Notify::new());
    let segment_prefix_written_for_task = Arc::clone(&segment_prefix_written);
    let release_segment_body = Arc::new(Notify::new());
    let release_segment_body_for_task = Arc::clone(&release_segment_body);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&requests_for_task);
            let segment_prefix_written = Arc::clone(&segment_prefix_written_for_task);
            let release_segment_body = Arc::clone(&release_segment_body_for_task);
            tokio::spawn(async move {
                let Some(path) = read_test_request_path(&mut socket).await else {
                    return;
                };
                requests.lock().await.push(path.clone());
                match path.as_str() {
                    "/live/final/init.mp4" => write_test_response(&mut socket, 200, SWITCH_MAP_BODY).await,
                    "/live/final/first.ts" => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            SWITCH_SEGMENT_BODY.len()
                        );
                        if socket.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        let split = SWITCH_SEGMENT_BODY.len() / 2;
                        if socket.write_all(&SWITCH_SEGMENT_BODY[..split]).await.is_err() {
                            return;
                        }
                        segment_prefix_written.notify_one();
                        release_segment_body.notified().await;
                        let _ = socket.write_all(&SWITCH_SEGMENT_BODY[split..]).await;
                    }
                    _ => write_test_response(&mut socket, 404, &[]).await,
                }
            });
        }
    });
    ControlledSwitchOriginServer {
        base_url: format!("http://{addr}"),
        requests,
        segment_prefix_written,
        release_segment_body,
        task,
    }
}

async fn spawn_critical_emergency_origin() -> CriticalEmergencyOriginServer {
    spawn_critical_emergency_origin_with_control(false).await
}

async fn spawn_controlled_critical_emergency_origin() -> CriticalEmergencyOriginServer {
    spawn_critical_emergency_origin_with_control(true).await
}

async fn spawn_critical_emergency_origin_with_control(pause_segment: bool) -> CriticalEmergencyOriginServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("critical emergency origin binds");
    let addr = listener.local_addr().expect("critical emergency origin address");
    let manifest_requests = Arc::new(AtomicUsize::new(0));
    let manifest_requests_for_task = Arc::clone(&manifest_requests);
    let segment_requests = Arc::new(AtomicUsize::new(0));
    let segment_requests_for_task = Arc::clone(&segment_requests);
    let segment_prefix_written = Arc::new(Notify::new());
    let segment_prefix_written_for_task = Arc::clone(&segment_prefix_written);
    let release_segment_body = Arc::new(Notify::new());
    let release_segment_body_for_task = Arc::clone(&release_segment_body);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let manifest_requests = Arc::clone(&manifest_requests_for_task);
            let segment_requests = Arc::clone(&segment_requests_for_task);
            let segment_prefix_written = Arc::clone(&segment_prefix_written_for_task);
            let release_segment_body = Arc::clone(&release_segment_body_for_task);
            tokio::spawn(async move {
                let Some(path) = read_test_request_path(&mut socket).await else {
                    return;
                };
                if path_has_extension(&path, "m3u8") {
                    let request_index = manifest_requests.fetch_add(1, Ordering::SeqCst);
                    if request_index == 0 {
                        write_test_response(&mut socket, 200, CRITICAL_HANDOFF_MANIFEST_BODY).await;
                    } else {
                        write_test_response(&mut socket, 407, b"retryable manifest failure").await;
                    }
                } else if path.ends_with("/first.ts") {
                    segment_requests.fetch_add(1, Ordering::SeqCst);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        CRITICAL_HANDOFF_TS_BODY.len()
                    );
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let split = CRITICAL_HANDOFF_TS_BODY.len() / 2;
                    if socket.write_all(&CRITICAL_HANDOFF_TS_BODY[..split]).await.is_err() {
                        return;
                    }
                    segment_prefix_written.notify_one();
                    if pause_segment {
                        release_segment_body.notified().await;
                    }
                    let _ = socket.write_all(&CRITICAL_HANDOFF_TS_BODY[split..]).await;
                } else {
                    write_test_response(&mut socket, 404, &[]).await;
                }
            });
        }
    });
    CriticalEmergencyOriginServer {
        base_url: format!("http://{addr}"),
        manifest_requests,
        segment_requests,
        segment_prefix_written,
        release_segment_body,
        task,
    }
}

fn switch_manifest_body(include_map: bool) -> String {
    let map = if include_map { "#EXT-X-MAP:URI=\"init.mp4\"\n" } else { "" };
    format!(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n{map}#EXTINF:4.0,\nfirst.ts\n#EXTINF:4.0,\nsecond.ts\n"
    )
}

fn switch_fetched_manifest(base_url: &str, include_map: bool) -> FetchedOriginManifest {
    FetchedOriginManifest {
        body: switch_manifest_body(include_map),
        final_manifest_url: format!("{base_url}/live/final/index.m3u8"),
        resolved_request_url: format!("{base_url}/live/user/pass/12345.m3u8"),
        redirect_host: None,
        provider_url_index: None,
        provider_session_headers: HeaderMap::new(),
        status: StatusCode::OK,
        attempts: 1,
        candidate_requests: 1,
        selection: HlsManifestFetchSelection::Initial,
    }
}

fn switch_test_request(
    session: Arc<RwLock<HlsSession>>,
    segment_cache: Arc<HlsSegmentCache>,
    base_url: &str,
) -> OriginRefreshRequest {
    let mut request = test_origin_refresh_request(session);
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{base_url}/live/user/pass/12345.m3u8")).expect("switch origin entry");
    request.client = reqwest::Client::builder().no_proxy().build().expect("switch client");
    request.no_redirect_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("switch no-redirect client");
    request.hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(segment_cache.cache_path(), 300));
    request.segment_cache = segment_cache;
    request.segment_worker_pool = Arc::new(HlsSegmentWorkerPool::new(SegmentFetchPolicy {
        origin_segment_timeout_ms: 2_000,
        retry_delays_ms: [0; 5],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    }));
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };
    request
}

async fn prepare_cross_host_baseline(session: &Arc<RwLock<HlsSession>>) {
    let body =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nold500.ts\n#EXTINF:4.0,\nold501.ts\n";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            body,
            "http://previous.example.com/live/user/pass/12345.m3u8",
        )
    else {
        panic!("baseline manifest parses as normal timeline");
    };
    let mut session = session.write().await;
    session
        .apply_origin_manifest_for_host(&manifest, crate::timeline::effective_origin_host_id("previous.example.com"))
        .expect("baseline timeline commits");
    session.last_effective_manifest_host = Some("previous.example.com".to_string());
    session.origin_control.pinned_host = Some("previous.example.com".to_string());
    session.origin_control.origin_epoch = session.origin_epoch;
}

async fn install_published_recovery_binding(
    session: &Arc<RwLock<HlsSession>>,
    request_url: &str,
    provider_url_index: Option<usize>,
) {
    let binding = HlsManifestOriginBinding::new(
        Url::parse(request_url).expect("published recovery request URL"),
        provider_url_index,
    )
    .expect("published recovery binding");
    let mut session = session.write().await;
    session.origin_control.manifest_origin_binding = Some(binding);
    session.origin_control.record_media_progress(1, 4_000);
    let evidence_proxy_seq = session
        .segments
        .iter()
        .find_map(|(proxy_seq, segment)| {
            (!is_hls_provisioning_segment(segment) && !is_hls_provisioning_gap_segment(segment)).then_some(*proxy_seq)
        })
        .unwrap_or_else(|| {
            let proxy_seq = session.proxy_next_seq.unwrap_or(0);
            let origin_sequence = session.origin_seq_highwater.unwrap_or(proxy_seq);
            let origin_epoch = session.origin_epoch;
            let cache_key = SegmentCacheKey::new(session.proxy_session_id.clone(), proxy_seq, "ts");
            session.segments.insert(
                proxy_seq,
                SegmentEntry {
                    origin_key: OriginSegmentKey {
                        origin_epoch,
                        effective_host_id: 0,
                        host_local_sequence: origin_sequence,
                        host_local_index: 0,
                    },
                    proxy_seq,
                    duration_ms: 4_000,
                    proxy_file_ext: "ts".to_string(),
                    content_type: "video/mp2t".to_string(),
                    cache_key,
                    discontinuity_before: false,
                    program_date_time: None,
                    daterange_tags_before: Vec::new(),
                    origin_byte_range: None,
                    map_ref: None,
                    encryption: None,
                    origin_fetch_ref: None,
                    status: SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 },
                    last_rendered_at_ms: None,
                    access: Arc::new(CacheAccessState::new()),
                },
            );
            session.publishable_origin_head_proxy_seq.get_or_insert(proxy_seq);
            session.publishable_origin_tail_proxy_seq = Some(proxy_seq);
            session.proxy_next_seq = Some(proxy_seq.saturating_add(1));
            proxy_seq
        });
    let discontinuity_sequence = session.discontinuity_sequence;
    let outcome = session.store_rendered_manifest(RenderedManifest {
        body: "#EXTM3U\n".to_string(),
        first_proxy_seq: evidence_proxy_seq,
        last_proxy_seq: evidence_proxy_seq,
        discontinuity_sequence,
        target_duration_ms: 4_000,
        playlist_duration_ms: 4_000,
        valid_until_ms: 10_000,
        render_gap_segments: 0,
        rendered_at_ms: 1,
        segment_proxy_seqs: vec![evidence_proxy_seq],
    });
    assert_eq!(outcome, RenderedManifestStoreOutcome::Stored);
    assert!(session.published_live_origin_baseline.is_some());
}

async fn commit_ready_baseline_snapshot(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    now_ms: u64,
) -> HlsLeaseManifestSnapshot {
    let cache_keys = {
        let session = session.read().await;
        session.segments.values().map(|segment| segment.cache_key.clone()).collect::<Vec<_>>()
    };
    for cache_key in &cache_keys {
        cache.write_bytes_and_commit(cache_key, CRITICAL_HANDOFF_TS_BODY).await.expect("baseline TS object commits");
    }

    let (visible_segments, discontinuity_sequence, target_duration_ms) = {
        let mut session = session.write().await;
        for segment in session.segments.values_mut() {
            assert!(segment.map_ref.is_none());
            assert!(segment.encryption.is_none());
            segment.status = SegmentCacheStatus::Ready {
                content_length: u64::try_from(CRITICAL_HANDOFF_TS_BODY.len()).unwrap_or(u64::MAX),
                ready_at_ms: now_ms,
            };
        }
        let visible_segments = session
            .segments
            .values()
            .map(|segment| HlsLeaseManifestSegment {
                proxy_seq: segment.proxy_seq,
                duration_ms: segment.duration_ms,
                uri: format!("/hls/test/{:06}.ts", segment.proxy_seq).into(),
                discontinuity_before: segment.discontinuity_before,
                map_ref_ready: true,
                encryption: None,
            })
            .collect::<Vec<_>>();
        (
            visible_segments,
            session.discontinuity_sequence,
            session.target_duration.map_or(4_000, |seconds| u64::from(seconds).saturating_mul(1_000)),
        )
    };
    let first_proxy_seq = visible_segments.first().expect("baseline head").proxy_seq;
    let last_proxy_seq = visible_segments.last().expect("baseline tail").proxy_seq;
    let playlist_duration_ms =
        visible_segments.iter().fold(0_u64, |duration_ms, segment| duration_ms.saturating_add(segment.duration_ms));
    HlsLeaseManifestSnapshot {
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(now_ms),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: now_ms,
        first_proxy_seq,
        last_proxy_seq,
        visible_segments: Arc::from(visible_segments),
        discontinuity_sequence,
        target_duration_ms,
        playlist_duration_ms,
        last_visible_media_end_ms: playlist_duration_ms,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

fn active_critical_test_lease(
    lease_id: &str,
    proxy_session_id: &super::super::ProxySessionId,
    issued_at_ms: u64,
    now_ms: u64,
    manifest: HlsLeaseManifestSnapshot,
) -> HlsAccessLease {
    let mut lease = HlsAccessLease::pending(
        HlsAccessLeaseId(lease_id.to_string()),
        HlsPlaybackFamilyKey::new(lease_id, lease_id),
        proxy_session_id.clone(),
        lease_id.to_string(),
        format!("{lease_id}-session"),
        1,
        "12345".to_string(),
        12345,
        issued_at_ms,
        120_000,
    );
    lease.state = super::super::HlsAccessLeaseState::Activated;
    lease.active_until_ms = Some(now_ms.saturating_add(120_000));
    lease.valid_until_ms = now_ms.saturating_add(120_000);
    lease.playback_mode = HlsLeasePlaybackMode::Live;
    lease.admission_generation = 1;
    lease.last_manifest_snapshot = Some(manifest);
    lease
}

async fn prepare_three_segment_critical_timeline(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    now_ms: u64,
) -> HlsLeaseManifestSnapshot {
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\nold500.ts\n#EXTINF:4.0,\nold501.ts\n#EXTINF:4.0,\nold502.ts\n";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            body,
            "http://previous.example.com/live/user/pass/12345.m3u8",
        )
    else {
        panic!("three-segment baseline parses");
    };
    {
        let mut session = session.write().await;
        session
            .apply_origin_manifest_for_host(
                &manifest,
                crate::timeline::effective_origin_host_id("previous.example.com"),
            )
            .expect("three-segment baseline commits");
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.origin_control.pinned_host = Some("previous.example.com".to_string());
        session.origin_control.origin_epoch = session.origin_epoch;
    }
    commit_ready_baseline_snapshot(session, cache, now_ms).await
}

fn critical_staging_generation(
    session: &mut HlsSession,
    now_ms: u64,
) -> super::switch_staging::HlsSwitchStagingGeneration {
    let burst_plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    session.origin_control.begin_acceptance_episode(
        now_ms,
        burst_plan,
        HlsManifestAcceptanceTrigger::Critical,
        &test_acceptance_episode_timing(now_ms, burst_plan),
    );
    let episode = session.origin_control.acceptance_episode.as_mut().expect("critical acceptance episode");
    episode.record_full_burst();
    episode.state = super::super::manifest_acceptance::HlsManifestAcceptanceState::StagingSwitchSegment;
    let identity = super::super::manifest_acceptance::HlsManifestRecoveryCandidateIdentity::from_candidate(
        0,
        Some("candidate.example.com"),
        "test-candidate",
    );
    assert_eq!(
        episode.select_candidate(episode.generation, identity),
        super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied
    );
    assert_eq!(
        episode.bind_selected_candidate(
            episode.generation,
            identity,
            super::super::recovery_timing::HlsRecoveryWorkload::clear_fetch(),
        ),
        super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied
    );
    super::switch_staging::switch_staging_generation(session).expect("critical staging generation")
}

#[tokio::test]
async fn critical_handoff_lock_contention_exhaustion_is_typed_and_bounded() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_access = Arc::clone(&attempts);

    let result: Result<(), HlsManifestCommitError> = super::retry_critical_handoff_state_access(
        crate::manifest_acceptance::HlsManifestAcceptanceGeneration(8),
        || {
            attempts_for_access.fetch_add(1, Ordering::SeqCst);
            async { crate::HlsCriticalHandoffStateAccess::LockBusy }
        },
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), crate::critical_handoff::HLS_CRITICAL_HANDOFF_COMMIT_RETRIES);
    let Err(HlsManifestCommitError::TimelineRejected { reason }) = result else {
        panic!("exhausted lock contention must return a typed timeline rejection");
    };
    assert_eq!(reason, HlsManifestRejectLogReason::CriticalHandoffLockContentionExhausted);
    assert_eq!(reason.status_label(), "critical-handoff-lock-contention-exhausted");
    assert_ne!(reason, HlsManifestRejectLogReason::SwitchResourceUnavailable);
    assert_ne!(reason, HlsManifestRejectLogReason::StagedSwitchInvalidated);
    assert_ne!(reason, HlsManifestRejectLogReason::PinnedHostRecoveryRejected);
}

#[tokio::test]
async fn critical_handoff_lock_busy_retry_retains_staged_state_until_acquired() {
    struct StagedRetentionProbe {
        identity: u64,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for StagedRetentionProbe {
        fn drop(&mut self) { self.drops.fetch_add(1, Ordering::SeqCst); }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let staged = StagedRetentionProbe { identity: 41, drops: Arc::clone(&drops) };
    let staged_address = std::ptr::from_ref(&staged);
    let attempts_for_access = Arc::clone(&attempts);

    let committed = super::retry_critical_handoff_state_access(
        crate::manifest_acceptance::HlsManifestAcceptanceGeneration(9),
        || {
            let attempt = attempts_for_access.fetch_add(1, Ordering::SeqCst);
            let staged = &staged;
            let drops_for_attempt = Arc::clone(&drops);
            async move {
                assert_eq!(std::ptr::from_ref(staged), staged_address);
                assert_eq!(drops_for_attempt.load(Ordering::SeqCst), 0);
                if attempt == 0 {
                    crate::HlsCriticalHandoffStateAccess::LockBusy
                } else {
                    crate::HlsCriticalHandoffStateAccess::Acquired(Ok(staged.identity))
                }
            }
        },
    )
    .await
    .expect("the retained staged state commits after transient contention");

    assert_eq!(committed, 41);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(staged);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn critical_handoff_selects_endangered_lease_base_instead_of_session_tail() {
    let temp_dir = tempfile::tempdir().expect("critical selection cache tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let session = test_session();
    let now_ms = super::current_time_millis();
    let complete = prepare_three_segment_critical_timeline(&session, &cache, now_ms).await;
    let mut first_window = complete.clone();
    first_window.snapshot_generation = 1;
    first_window.visible_segments = Arc::from([complete.visible_segments[0].clone()]);
    first_window.last_proxy_seq = first_window.first_proxy_seq;
    first_window.playlist_duration_ms = 4_000;
    first_window.last_visible_media_end_ms = 4_000;
    let mut endangered_window = complete.clone();
    endangered_window.snapshot_generation = 2;
    endangered_window.visible_segments =
        Arc::from([complete.visible_segments[0].clone(), complete.visible_segments[1].clone()]);
    endangered_window.last_proxy_seq = complete.visible_segments[1].proxy_seq;
    endangered_window.playlist_duration_ms = 8_000;
    endangered_window.last_visible_media_end_ms = 8_000;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let leases = vec![
        active_critical_test_lease("more-reserve", &proxy_session_id, now_ms, now_ms, first_window),
        active_critical_test_lease(
            "endangered",
            &proxy_session_id,
            now_ms.saturating_add(1),
            now_ms,
            endangered_window,
        ),
    ];
    let mut session = session.write().await;
    let generation = critical_staging_generation(&mut session, now_ms);
    let (selected, snapshot) =
        super::switch_staging::select_critical_handoff_lease(&session, &leases, &generation, now_ms)
            .expect("one lease is cutover critical");

    assert_eq!(selected.lease_id, HlsAccessLeaseId("endangered".to_string()));
    assert_eq!(snapshot.base.proxy_seq, complete.visible_segments[1].proxy_seq);
    assert_ne!(snapshot.base.proxy_seq, complete.visible_segments[2].proxy_seq);
}

#[tokio::test]
async fn critical_handoff_prioritizes_earliest_safe_cutover_deadline() {
    let temp_dir = tempfile::tempdir().expect("critical deadline cache tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let session = test_session();
    let now_ms = super::current_time_millis();
    let complete = prepare_three_segment_critical_timeline(&session, &cache, now_ms).await;
    let mut wider_margin = complete.clone();
    wider_margin.snapshot_generation = 3;
    wider_margin.target_duration_ms = 4_000;
    let mut exhausted_short_margin = complete.clone();
    exhausted_short_margin.snapshot_generation = 4;
    exhausted_short_margin.target_duration_ms = 1_000;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let mut wider_margin_lease =
        active_critical_test_lease("wider-margin", &proxy_session_id, now_ms, now_ms, wider_margin);
    let playback_at_ms = now_ms.saturating_sub(3_000);
    let token = wider_margin_lease.playback_cursor.record_request_started(complete.last_proxy_seq, playback_at_ms);
    let _ = wider_margin_lease.playback_cursor.record_request_completed(token, playback_at_ms);
    let leases = vec![
        wider_margin_lease,
        active_critical_test_lease(
            "exhausted-short-margin",
            &proxy_session_id,
            now_ms.saturating_add(1),
            now_ms,
            exhausted_short_margin,
        ),
    ];
    let mut session = session.write().await;
    let generation = critical_staging_generation(&mut session, now_ms);
    let (selected, snapshot) =
        super::switch_staging::select_critical_handoff_lease(&session, &leases, &generation, now_ms)
            .expect("critical leases have a deterministic deadline order");

    assert_eq!(selected.lease_id, HlsAccessLeaseId("wider-margin".to_string()));
    assert_eq!(snapshot.base.proxy_seq, complete.last_proxy_seq);
}

#[tokio::test]
async fn critical_handoff_manifest_supersession_invalidates_frozen_snapshot() {
    let temp_dir = tempfile::tempdir().expect("critical supersession cache tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let session = test_session();
    let now_ms = super::current_time_millis();
    let complete = prepare_three_segment_critical_timeline(&session, &cache, now_ms).await;
    let mut endangered_window = complete.clone();
    endangered_window.snapshot_generation = 7;
    endangered_window.visible_segments =
        Arc::from([complete.visible_segments[0].clone(), complete.visible_segments[1].clone()]);
    endangered_window.last_proxy_seq = complete.visible_segments[1].proxy_seq;
    endangered_window.playlist_duration_ms = 8_000;
    endangered_window.last_visible_media_end_ms = 8_000;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let mut lease = active_critical_test_lease("endangered", &proxy_session_id, now_ms, now_ms, endangered_window);
    let mut session = session.write().await;
    let generation = critical_staging_generation(&mut session, now_ms);
    let (_, frozen) =
        super::switch_staging::select_critical_handoff_lease(&session, &[lease.clone()], &generation, now_ms)
            .expect("initial critical snapshot");
    let mut leases = HlsAccessLeaseStore::default();
    leases.prepare_access_lease(lease.clone());
    assert!(super::switch_staging::critical_handoff_snapshot_is_current(
        &mut leases,
        &session,
        &generation,
        &frozen,
        now_ms,
    ));
    if let Some(manifest) = lease.last_manifest_snapshot.as_mut() {
        manifest.snapshot_generation = manifest.snapshot_generation.saturating_add(1);
    }
    leases.prepare_access_lease(lease);

    assert!(!super::switch_staging::critical_handoff_snapshot_is_current(
        &mut leases,
        &session,
        &generation,
        &frozen,
        now_ms,
    ));
}

async fn prepare_active_critical_handoff_lease(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    now_ms: u64,
) -> (Arc<HlsProxyManager>, HlsAccessLeaseId, HlsAccessLease) {
    let manifest_snapshot = commit_ready_baseline_snapshot(session, cache, now_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("critical-emergency-handoff".to_string());
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(cache.cache_path(), 300));
    hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("critical-user", "critical-client"),
            proxy_session_id.clone(),
            "critical-user".to_string(),
            "critical-session".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            120_000,
        ))
        .await;
    assert!(hls_proxy
        .activate_access_lease(
            &lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 120_000 },
        )
        .await
        .is_activated());
    let publication_guard = hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, now_ms)
        .await
        .expect("active lease accepts authoritative manifest publication");
    assert!(hls_proxy
        .commit_access_lease_manifest_publication(
            &lease_id,
            &proxy_session_id,
            publication_guard,
            manifest_snapshot,
            now_ms,
        )
        .await
        .is_committed());
    let lease = hls_proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, now_ms)
        .await
        .expect("critical live lease snapshot");
    (hls_proxy, lease_id, lease)
}

async fn install_active_critical_test_lease(
    hls_proxy: &HlsProxyManager,
    lease_id: &str,
    proxy_session_id: &super::super::ProxySessionId,
    manifest: HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> HlsAccessLeaseId {
    let lease_id = HlsAccessLeaseId(lease_id.to_string());
    hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new(lease_id.0.as_str(), lease_id.0.as_str()),
            proxy_session_id.clone(),
            lease_id.0.clone(),
            format!("{}-session", lease_id.0),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            120_000,
        ))
        .await;
    assert!(hls_proxy
        .activate_access_lease(
            &lease_id,
            proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 120_000, valid_window_ms: 120_000 },
        )
        .await
        .is_activated());
    let publication_guard = hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, proxy_session_id, now_ms)
        .await
        .expect("critical test lease publication guard");
    assert!(hls_proxy
        .commit_access_lease_manifest_publication(&lease_id, proxy_session_id, publication_guard, manifest, now_ms,)
        .await
        .is_committed());
    lease_id
}

fn critical_handoff_app_config() -> Arc<AppConfig> {
    let app_config = test_app_config();
    app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
        channel_unavailable: Some(TransportStreamBuffer::new(CRITICAL_HANDOFF_TS_BODY.to_vec())),
        user_connections_exhausted: None,
        provider_connections_exhausted: None,
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        panel_api_provisioning_hls_segments: Vec::new(),
    })));
    app_config
}

#[tokio::test]
async fn hls_prepared_terminal_bundle_early_refresh_hook_starts_singleflight_for_known_target_duration() {
    let mut request = test_origin_refresh_request(test_session());
    request.app_config = critical_handoff_app_config();
    let terminal_response = request.app_config.custom_stream_response.load_full();
    let asset = terminal_response
        .as_ref()
        .and_then(|response| response.channel_unavailable.as_ref())
        .and_then(|buffer| super::snapshot_terminal_media_asset(buffer).ok())
        .expect("terminal refresh asset");
    let target_duration_ms = asset.duration_ms().saturating_add(1_000);
    let key = super::super::prepared_terminal_bundle::prepared_terminal_bundle_key(
        &asset,
        target_duration_ms,
        super::HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );

    super::start_refresh_terminal_bundle_preparation(&request, target_duration_ms);
    let state =
        request.hls_proxy.wait_for_prepared_terminal_bundle(key).await.expect("early terminal preparation completes");

    assert!(matches!(
        state,
        HlsPreparedTerminalBundleState::Ready { bundle } if bundle.key == key
    ));
}

#[test]
fn critical_handoff_terminal_response_revision_must_remain_current() {
    let app_config = critical_handoff_app_config();
    let frozen = app_config.custom_stream_response.load_full();
    assert!(super::critical_handoff_terminal_response_is_current(&app_config, frozen.as_ref()));
    let replacement = frozen.as_ref().map(|response| Arc::new(response.as_ref().clone()));
    app_config.custom_stream_response.store(replacement);

    assert!(!super::critical_handoff_terminal_response_is_current(&app_config, frozen.as_ref()));
}

#[test]
fn critical_handoff_and_terminal_tail_share_ts_inspector_signature() {
    let terminal_buffer = TransportStreamBuffer::new(CRITICAL_HANDOFF_TS_BODY.to_vec());
    let terminal_asset = super::snapshot_terminal_media_asset(&terminal_buffer).expect("terminal asset");
    let critical_signature = match tuliprox_mpegts::ts_inspector::inspect_mpeg_ts(
        std::io::Cursor::new(CRITICAL_HANDOFF_TS_BODY),
        tuliprox_mpegts::ts_inspector::HlsTsProbeProtection::Clear,
        tuliprox_mpegts::ts_inspector::HlsTsProbeBudget::default(),
    )
    .expect("critical handoff fixture inspection succeeds")
    {
        tuliprox_mpegts::ts_inspector::HlsTsProbeOutcome::Found(signature) => signature,
        outcome => panic!("critical handoff fixture has no MPEG-TS tracks: {outcome:?}"),
    };

    assert_eq!(critical_signature, terminal_asset.track_signature().clone());
}

async fn assert_critical_handoff_evidence(request: &OriginRefreshRequest, live_lease: &HlsAccessLease, now_ms: u64) {
    let manifest = live_lease.last_manifest_snapshot.as_ref().expect("authoritative manifest snapshot");
    assert_eq!(manifest.snapshot_generation, 1);
    let ready_timeline = {
        let session = request.session.read().await;
        session.ready_timeline_snapshot(
            live_lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
            now_ms,
        )
    };
    let reserve = super::super::evaluate_lease_reserve(super::super::HlsLeaseReserveInput {
        manifest,
        cursor: &live_lease.playback_cursor,
        ready_timeline: &ready_timeline,
        now_ms,
        playback_rate_guard_milli: super::super::HLS_PLAYBACK_RATE_GUARD_MILLI,
        recovery_trigger_budget: super::super::recovery_timing::HlsRecoveryTriggerBudgetMs::from_millis(0),
        origin_path_degraded: true,
        recovery_committed: false,
    });
    assert_eq!(reserve.availability_basis, HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline);
    assert_eq!(reserve.guaranteed_media_horizon_ms, manifest.last_visible_media_end_ms);
    assert_eq!(reserve.guaranteed_reserve_ms, 0);
    assert!(reserve.cutover_required);
    let candidate_tracks = match tuliprox_mpegts::ts_inspector::inspect_mpeg_ts(
        std::io::Cursor::new(CRITICAL_HANDOFF_TS_BODY),
        tuliprox_mpegts::ts_inspector::HlsTsProbeProtection::Clear,
        tuliprox_mpegts::ts_inspector::HlsTsProbeBudget::default(),
    )
    .expect("critical handoff fixture inspection succeeds")
    {
        tuliprox_mpegts::ts_inspector::HlsTsProbeOutcome::Found(signature) => signature,
        outcome => panic!("critical handoff fixture has no MPEG-TS tracks: {outcome:?}"),
    };
    let terminal_response =
        request.app_config.custom_stream_response.load_full().expect("critical handoff terminal response");
    let terminal_asset = super::snapshot_terminal_media_asset(
        terminal_response.channel_unavailable.as_ref().expect("critical handoff terminal buffer"),
    )
    .expect("critical handoff terminal asset");
    assert_eq!(
        crate::critical_handoff::terminal_alternative_compatibility_for_critical_lease(
            Some(terminal_asset.as_ref()),
            live_lease,
            &candidate_tracks,
        ),
        crate::manifest_acceptance::HlsTerminalAlternativeCompatibility::LiveHandoffSafer
    );
}

async fn assert_critical_handoff_timeline_commit(
    session: &Arc<RwLock<HlsSession>>,
    cache: &HlsSegmentCache,
    origin: &CriticalEmergencyOriginServer,
    baseline_epoch: u64,
    expected_candidate_count: usize,
) {
    let (staged_cache_key, staged_content_length) = {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
        let effective_host = host_from_base_url(&origin.base_url);
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some(effective_host.as_str()));
        assert_eq!(
            session.origin_epoch_effective_host_id,
            Some(crate::timeline::effective_origin_host_id(&effective_host))
        );
        let episode = session.origin_control.acceptance_episode.as_ref().expect("completed acceptance episode");
        assert!(episode.full_burst_completed);
        assert_eq!(episode.completed_burst_candidates, expected_candidate_count);
        assert_eq!(episode.state, super::super::manifest_acceptance::HlsManifestAcceptanceState::Completed);
        let switched = session
            .segments
            .values()
            .filter(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
            .collect::<Vec<_>>();
        assert_eq!(switched.len(), 2);
        let staged = switched.first().expect("staged handoff segment");
        assert_eq!(staged.origin_key.host_local_sequence, 900);
        assert!(staged.discontinuity_before);
        let content_length = match &staged.status {
            SegmentCacheStatus::Ready { content_length, .. } => *content_length,
            SegmentCacheStatus::Discovered
            | SegmentCacheStatus::Queued { .. }
            | SegmentCacheStatus::Fetching { .. }
            | SegmentCacheStatus::CapacityDeferred { .. }
            | SegmentCacheStatus::FailedRetryable { .. }
            | SegmentCacheStatus::FailedPermanent { .. }
            | SegmentCacheStatus::Expired => panic!("staged handoff segment must be READY"),
        };
        assert_eq!(content_length, u64::try_from(CRITICAL_HANDOFF_TS_BODY.len()).unwrap_or(u64::MAX));
        assert!(!matches!(switched[1].status, SegmentCacheStatus::Ready { .. }));
        (staged.cache_key.clone(), content_length)
    };
    let staged_metadata = cache
        .metadata(&staged_cache_key)
        .await
        .expect("staged cache metadata reads")
        .expect("staged cache object exists");
    assert_eq!(staged_metadata.size, staged_content_length);
}

#[tokio::test]
async fn critical_single_alternative_full_acceptance_commits_verified_new_origin_epoch() {
    let origin = spawn_critical_emergency_origin().await;
    let temp_dir = tempfile::tempdir().expect("critical handoff cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let baseline_binding = HlsManifestOriginBinding::new(
        Url::parse("https://previous.example.com/live/index.m3u8?token=baseline").expect("baseline URL"),
        Some(0),
    )
    .expect("baseline binding");
    session.write().await.origin_control.manifest_origin_binding = Some(baseline_binding);
    let now_ms = super::current_time_millis();
    let (baseline_epoch, baseline_readiness_generation) = {
        let session = session.read().await;
        (session.origin_epoch, session.activity.media_readiness_generation)
    };
    let (hls_proxy, lease_id, live_lease) = prepare_active_critical_handoff_lease(&session, &cache, now_ms).await;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.app_config = critical_handoff_app_config();
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Critical;
    request.access_lease_id = Some(lease_id);
    request.now_ms = now_ms;
    assert_critical_handoff_evidence(&request, &live_lease, now_ms).await;

    let plan = request.manifest_recovery_burst.level.plan();
    let target_url = request.origin_entry.url().clone();
    let fetch_context = manifest_fetch_context(&request);
    let committed = retry_hls_origin_manifest_recovery_chain(
        &fetch_context,
        test_manifest_origin_binding(target_url.clone()),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::Critical,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await
    .expect("critical emergency handoff commits");

    assert_eq!(committed.fetched.body.as_bytes(), CRITICAL_HANDOFF_MANIFEST_BODY);
    assert_eq!(origin.manifest_requests.load(Ordering::SeqCst), plan.total_candidates());
    assert_eq!(origin.segment_requests.load(Ordering::SeqCst), 1);
    assert_critical_handoff_timeline_commit(&session, &cache, &origin, baseline_epoch, plan.total_candidates()).await;
    assert_eq!(
        session.read().await.activity.media_readiness_generation,
        baseline_readiness_generation.saturating_add(1),
        "one staged READY transaction advances media readiness exactly once"
    );
    assert_eq!(
        session
            .read()
            .await
            .origin_control
            .manifest_origin_binding
            .as_ref()
            .map(super::super::manifest_origin_binding::HlsManifestOriginBinding::request_url),
        Some(&target_url)
    );
}

#[tokio::test]
async fn critical_handoff_uses_endangered_lease_base_when_session_tail_is_incompatible() {
    let origin = spawn_critical_emergency_origin().await;
    let temp_dir = tempfile::tempdir().expect("lease-specific critical handoff cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = test_session();
    let now_ms = super::current_time_millis();
    let complete = prepare_three_segment_critical_timeline(&session, &cache, now_ms).await;
    let mut noncritical_window = complete.clone();
    noncritical_window.visible_segments = Arc::from([complete.visible_segments[0].clone()]);
    noncritical_window.last_proxy_seq = noncritical_window.first_proxy_seq;
    noncritical_window.playlist_duration_ms = 4_000;
    noncritical_window.last_visible_media_end_ms = 4_000;
    let mut endangered_window = complete.clone();
    endangered_window.visible_segments =
        Arc::from([complete.visible_segments[0].clone(), complete.visible_segments[1].clone()]);
    endangered_window.last_proxy_seq = complete.visible_segments[1].proxy_seq;
    endangered_window.playlist_duration_ms = 8_000;
    endangered_window.last_visible_media_end_ms = 8_000;
    let (tail_key, lease_base_key, baseline_epoch) = {
        let session = session.read().await;
        (
            session.segments.get(&complete.last_proxy_seq).expect("session-wide tail").cache_key.clone(),
            session.segments.get(&endangered_window.last_proxy_seq).expect("endangered lease base").cache_key.clone(),
            session.origin_epoch,
        )
    };
    cache.delete(&tail_key).await.expect("replace session-wide tail fixture");
    let invalid_tail = vec![0_u8; 188 * 2];
    cache.write_bytes_and_commit(&tail_key, &invalid_tail).await.expect("incompatible session-wide tail commits");
    {
        let mut session = session.write().await;
        let tail = session.segments.get_mut(&complete.last_proxy_seq).expect("session-wide tail entry");
        tail.status = SegmentCacheStatus::Ready {
            content_length: u64::try_from(invalid_tail.len()).unwrap_or(u64::MAX),
            ready_at_ms: now_ms,
        };
    }
    assert!(super::switch_staging::inspect_cache_object_tracks(&cache, &tail_key).await.signature().is_none());
    assert!(super::switch_staging::inspect_cache_object_tracks(&cache, &lease_base_key).await.signature().is_some());

    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(cache.cache_path(), 300));
    let _noncritical_lease = install_active_critical_test_lease(
        &hls_proxy,
        "noncritical-window",
        &proxy_session_id,
        noncritical_window,
        now_ms,
    )
    .await;
    let endangered_lease = install_active_critical_test_lease(
        &hls_proxy,
        "endangered-window",
        &proxy_session_id,
        endangered_window,
        now_ms,
    )
    .await;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.hls_proxy = hls_proxy;
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Critical;
    request.access_lease_id = Some(endangered_lease);
    request.now_ms = now_ms;
    let plan = request.manifest_recovery_burst.level.plan();
    let target_url = request.origin_entry.url().clone();
    let fetch_context = manifest_fetch_context(&request);

    let committed = retry_hls_origin_manifest_recovery_chain(
        &fetch_context,
        test_manifest_origin_binding(target_url),
        Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
        None,
        HlsManifestAcceptanceTrigger::Critical,
        HlsManifestCommitAcceptanceMode::StrictPinnedHost,
        |fetched, acceptance_mode| super::commit_manifest_recovery_candidate(&request, fetched, acceptance_mode),
    )
    .await
    .expect("lease-specific compatible base permits critical handoff");

    assert_eq!(committed.fetched.body.as_bytes(), CRITICAL_HANDOFF_MANIFEST_BODY);
    assert_eq!(origin.manifest_requests.load(Ordering::SeqCst), plan.total_candidates());
    assert_eq!(origin.segment_requests.load(Ordering::SeqCst), 1);
    let session = session.read().await;
    assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
    assert!(session
        .segments
        .values()
        .find(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
        .is_some_and(|segment| segment.discontinuity_before));
}

#[tokio::test]
async fn critical_handoff_manifest_supersession_during_candidate_io_rolls_back_staging() {
    let origin = spawn_controlled_critical_emergency_origin().await;
    let temp_dir = tempfile::tempdir().expect("critical supersession cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let now_ms = super::current_time_millis();
    let (hls_proxy, lease_id, live_lease) = prepare_active_critical_handoff_lease(&session, &cache, now_ms).await;
    let proxy_session_id = live_lease.proxy_session_id.clone();
    let base_proxy_seq = live_lease.last_manifest_snapshot.as_ref().expect("critical manifest snapshot").last_proxy_seq;
    let (baseline_epoch, baseline_proxy_next_seq, baseline_readiness_generation, baseline_segments, base_access) = {
        let session = session.read().await;
        (
            session.origin_epoch,
            session.proxy_next_seq,
            session.activity.media_readiness_generation,
            session
                .segments
                .iter()
                .map(|(proxy_seq, segment)| (*proxy_seq, segment.cache_key.clone()))
                .collect::<Vec<_>>(),
            Arc::clone(&session.segments.get(&base_proxy_seq).expect("critical lease base segment").access),
        )
    };
    let candidate_cache_key = super::super::SegmentCacheKey::new(
        proxy_session_id.clone(),
        baseline_proxy_next_seq.expect("cross-host preview sequence"),
        "ts",
    );
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.app_config = critical_handoff_app_config();
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Critical;
    request.access_lease_id = Some(lease_id.clone());
    request.now_ms = now_ms;
    let request = Arc::new(request);
    let task_request = Arc::clone(&request);
    let target_url = task_request.origin_entry.url().clone();
    let commit_task = tokio::spawn(async move {
        let fetch_context = manifest_fetch_context(&task_request);
        retry_hls_origin_manifest_recovery_chain(
            &fetch_context,
            test_manifest_origin_binding(target_url),
            Some(HlsManifestRejectLogReason::PinnedHostRecoveryRejected),
            None,
            HlsManifestAcceptanceTrigger::Critical,
            HlsManifestCommitAcceptanceMode::StrictPinnedHost,
            |fetched, acceptance_mode| {
                super::commit_manifest_recovery_candidate(&task_request, fetched, acceptance_mode)
            },
        )
        .await
    });

    origin.segment_prefix_written.notified().await;
    assert!(base_access.active_readers() > 0, "lease base is pinned before candidate media completes");
    let publication_now_ms = super::current_time_millis();
    let publication_guard = hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, publication_now_ms)
        .await
        .expect("superseding manifest publication guard");
    let superseding_snapshot = live_lease.last_manifest_snapshot.clone().expect("superseding snapshot");
    assert!(hls_proxy
        .commit_access_lease_manifest_publication(
            &lease_id,
            &proxy_session_id,
            publication_guard,
            superseding_snapshot,
            publication_now_ms,
        )
        .await
        .is_committed());
    origin.release_segment_body.notify_one();

    let result = commit_task.await.expect("critical handoff task joins");
    assert!(result.is_err(), "superseded lease snapshot cannot commit");
    assert_eq!(base_access.active_readers(), 0, "rollback releases the frozen base pin");
    {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch);
        assert_eq!(session.proxy_next_seq, baseline_proxy_next_seq);
        assert_eq!(session.activity.media_readiness_generation, baseline_readiness_generation);
        assert_eq!(
            session
                .segments
                .iter()
                .map(|(proxy_seq, segment)| (*proxy_seq, segment.cache_key.clone()))
                .collect::<Vec<_>>(),
            baseline_segments
        );
    }
    assert!(
        cache.metadata(&candidate_cache_key).await.expect("candidate cache metadata reads").is_none(),
        "final CAS rejection removes the staged candidate object"
    );
}

async fn assert_fresh_revalidation_cross_host_switch_runs_complete_beast_plan(reason: HlsFreshManifestRequiredReason) {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let manifest =
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nfirst.ts\n#EXTINF:4.0,\nsecond.ts\n"
            .to_string();
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path_has_extension(&path, "m3u8") {
            (200, Vec::new(), manifest.clone())
        } else if path.ends_with("/first.ts") {
            (200, Vec::new(), String::from_utf8_lossy(SWITCH_SEGMENT_BODY).into_owned())
        } else {
            (404, Vec::new(), String::new())
        }
    }))
    .await;
    let cache_dir = tempfile::tempdir().expect("fresh revalidation cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let recovery_request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    install_published_recovery_binding(&session, &recovery_request_url, None).await;
    let baseline_epoch = session.read().await.origin_epoch;
    let mut request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired { reason };
    // A weaker observation signal must not reduce the strict revalidation
    // policy selected by the commit requirement.
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Observe;

    assert!(trigger_origin_refresh_sync(request).await);

    let (manifest_requests, staged_segment_requests) = {
        let requests = origin.requests.lock().await;
        (
            requests.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count(),
            requests.iter().filter(|path| path.ends_with("/first.ts")).count(),
        )
    };
    assert_eq!(manifest_requests, plan.total_candidates());
    assert_eq!(staged_segment_requests, 1);

    let effective_host = host_from_base_url(&origin.base_url);
    let session = session.read().await;
    assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some(effective_host.as_str()));
    let switched_head = session
        .segments
        .values()
        .find(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
        .expect("fresh revalidation switched timeline head");
    assert!(switched_head.discontinuity_before);
    assert!(matches!(switched_head.status, SegmentCacheStatus::Ready { .. }));
}

#[tokio::test]
async fn expired_revalidation_cross_host_switch_runs_complete_beast_acceptance() {
    assert_fresh_revalidation_cross_host_switch_runs_complete_beast_plan(
        HlsFreshManifestRequiredReason::ExpiredRevalidation,
    )
    .await;
}

#[tokio::test]
async fn hard_failure_revalidation_cross_host_switch_runs_complete_beast_acceptance() {
    assert_fresh_revalidation_cross_host_switch_runs_complete_beast_plan(
        HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
    )
    .await;
}

#[tokio::test]
async fn fresh_pinned_revalidation_same_host_rebase_runs_complete_beast_plan() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let origin = spawn_test_origin(Arc::new(|path| {
        if path_has_extension(&path, "m3u8") {
            (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n10.ts\n#EXTINF:4.0,\n11.ts\n"
                    .to_string(),
            )
        } else {
            (404, Vec::new(), String::new())
        }
    }))
    .await;
    let effective_host = host_from_base_url(&origin.base_url);
    let baseline_body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1000\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\n1000.ts\n#EXTINF:4.0,\n1001.ts\n";
    let baseline_manifest_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(baseline) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(baseline_body, &baseline_manifest_url)
    else {
        panic!("same-host stale baseline parses");
    };
    let session = test_session();
    let baseline_epoch = {
        let mut session = session.write().await;
        session
            .apply_origin_manifest_for_host(&baseline, crate::timeline::effective_origin_host_id(&effective_host))
            .expect("same-host stale baseline commits");
        session.last_effective_manifest_host = Some(effective_host.clone());
        session.origin_control.pinned_host = Some(effective_host.clone());
        session.origin_control.origin_epoch = session.origin_epoch;
        session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
        session.origin_epoch
    };
    install_published_recovery_binding(&session, &baseline_manifest_url, None).await;
    let cache_dir = tempfile::tempdir().expect("fresh pinned revalidation cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.manifest_commit_requirement = HlsManifestCommitRequirement::FreshCommitRequired {
        reason: HlsFreshManifestRequiredReason::PreviousHardManifestFailure,
    };
    request.acceptance_directive.trigger = HlsManifestAcceptanceTrigger::Observe;

    assert!(trigger_origin_refresh_sync(request).await);

    let manifest_requests =
        origin.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests, plan.total_candidates());
    let next_fetch_allowed_at_ms = {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
        assert_eq!(session.origin_seq_highwater, Some(11));
        assert_eq!(session.last_effective_manifest_host.as_deref(), Some(effective_host.as_str()));
        assert_eq!(session.fresh_manifest_commit_required, None);
        let rebased_head = session
            .segments
            .values()
            .find(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
            .expect("same-host rebased timeline head");
        assert!(rebased_head.discontinuity_before);
        session.origin_refresh.next_fetch_allowed_at_ms
    };

    let mut follow_up = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    follow_up.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    follow_up.manifest_commit_requirement = session
        .read()
        .await
        .fresh_manifest_commit_required
        .map_or(HlsManifestCommitRequirement::CommittedManifestAllowed, |reason| {
            HlsManifestCommitRequirement::FreshCommitRequired { reason }
        });
    follow_up.now_ms = next_fetch_allowed_at_ms;

    assert!(trigger_origin_refresh_sync(follow_up).await);
    let manifest_requests_after_follow_up =
        origin.requests.lock().await.iter().filter(|path| path.as_str() == "/live/user/pass/12345.m3u8").count();
    assert_eq!(manifest_requests_after_follow_up, plan.total_candidates().saturating_add(1));
}

async fn prepare_ready_content_anchor(session: &Arc<RwLock<HlsSession>>, cache: &HlsSegmentCache, bytes: &[u8]) {
    let key = {
        let mut session = session.write().await;
        let segment = session.segments.values_mut().next().expect("baseline segment");
        segment.status = SegmentCacheStatus::Ready {
            content_length: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            ready_at_ms: 1,
        };
        segment.cache_key.clone()
    };
    cache.write_bytes_and_commit(&key, bytes).await.expect("committed anchor bytes");
}

async fn assert_cross_host_replay_does_not_commit(candidate_bytes: &'static str) {
    const COMMITTED_BYTES: &[u8] = b"committed-media-bytes";
    let manifest = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:900\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nold500.ts\n".to_string();
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path_has_extension(&path, "m3u8") {
            (200, Vec::new(), manifest.clone())
        } else if path.ends_with("/old500.ts") {
            (200, Vec::new(), candidate_bytes.to_string())
        } else {
            (404, Vec::new(), String::new())
        }
    }))
    .await;
    let cache_dir = tempfile::tempdir().expect("content anchor cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(cache_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    prepare_ready_content_anchor(&session, &cache, COMMITTED_BYTES).await;
    let baseline_epoch = session.read().await.origin_epoch;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Friendly };
    let target_url =
        Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("content anchor target");

    let result = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await;

    let requests = origin.requests.lock().await.clone();
    let session = session.read().await;
    assert!(result.is_err(), "replay-only cross-host candidate committed: requests={requests:?}");
    assert_eq!(session.origin_epoch, baseline_epoch);
}

#[tokio::test]
async fn same_cross_host_sequence_path_and_equal_bytes_without_forward_progress_never_commits() {
    assert_cross_host_replay_does_not_commit("committed-media-bytes").await;
}

#[tokio::test]
async fn same_cross_host_sequence_and_path_with_different_bytes_never_commits_as_anchor() {
    assert_cross_host_replay_does_not_commit("different-media-bytes").await;
}

async fn assert_committed_content_anchor_is_gc_pinned_until_read_pin_drop() {
    const COMMITTED_BYTES: &[u8] = b"committed-media-bytes";

    let temp_dir = tempfile::tempdir().expect("acceptance pin cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let sessions = Arc::new(HlsSessionStore::new());
    let session = sessions.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    let gc = HlsGarbageCollector::new(
        Arc::clone(&sessions),
        Arc::clone(&cache),
        GarbageCollectionPolicy {
            cache_duration_ms: 0,
            cache_bytes_global: 10_000,
            cache_bytes_per_session: 10_000,
            session_idle_timeout_ms: u64::MAX,
            temp_file_retention_ms: 30_000,
            failed_segment_retention_ms: 10,
        },
        build_rewrite_secret_fingerprint(b"secret"),
    );
    let manifest_body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
        #EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n\
        #EXTINF:4.0,\n4.ts\n#EXTINF:4.0,\n5.ts\n#EXTINF:4.0,\n6.ts\n";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            manifest_body,
            "http://origin.example/live/index.m3u8",
        )
    else {
        panic!("acceptance pin manifest must parse as a normal timeline");
    };
    let cache_key = {
        let mut session = session.write().await;
        session.proxy_next_seq = Some(1);
        session.apply_origin_manifest(&manifest).expect("acceptance pin timeline commits");
        session.segments.get(&1).expect("first committed segment").cache_key.clone()
    };
    cache.write_bytes_and_commit(&cache_key, COMMITTED_BYTES).await.expect("committed acceptance object writes");
    let access = {
        let mut session = session.write().await;
        let segment = session.segments.get_mut(&1).expect("first committed segment");
        segment.status = SegmentCacheStatus::Ready {
            content_length: u64::try_from(COMMITTED_BYTES.len()).unwrap_or(u64::MAX),
            ready_at_ms: 0,
        };
        let expected_identity =
            crate::resource_identity::HlsMediaResourceIdentity::from_url("http://origin.example/live/1.ts", None);
        session
            .segments
            .values()
            .rev()
            .find_map(|entry| {
                let fetch_ref = entry.origin_fetch_ref.as_ref()?;
                crate::resource_identity::HlsMediaResourceIdentity::from_url(
                    &fetch_ref.resolved_origin_url,
                    fetch_ref.byte_range,
                )
                .matches(expected_identity)
                .then(|| Arc::clone(&entry.access))
            })
            .expect("content anchor object selection")
    };
    let read_pin = super::commit::HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&access), 5);
    assert_eq!(access.active_readers(), 1);

    let pinned_report = gc.run_once(10_000).await.expect("GC runs while acceptance object is pinned");
    assert_eq!(pinned_report.segments_deleted_duration, 0);
    assert!(session.read().await.segments.contains_key(&1));
    assert!(cache.metadata(&cache_key).await.expect("pinned object metadata reads").is_some());

    drop(read_pin);
    assert_eq!(access.active_readers(), 0);
    let released_report = gc.run_once(10_001).await.expect("GC runs after acceptance pin release");
    assert_eq!(released_report.segments_deleted_duration, 1);
    assert!(!session.read().await.segments.contains_key(&1));
    assert!(cache.metadata(&cache_key).await.expect("released object metadata reads").is_none());
}

#[tokio::test]
async fn committed_content_anchor_object_survives_gc_until_acceptance_read_pin_drop() {
    assert_committed_content_anchor_is_gc_pinned_until_read_pin_drop().await;
}

#[tokio::test]
async fn critical_handoff_base_evidence_survives_gc_until_preparation_drop() {
    let temp_dir = tempfile::tempdir().expect("critical evidence GC tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let sessions = Arc::new(HlsSessionStore::new());
    let session = sessions.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    prepare_cross_host_baseline(&session).await;
    let manifest = commit_ready_baseline_snapshot(&session, &cache, 0).await;
    let base_proxy_seq = manifest.last_proxy_seq;
    let (base_key, base_access) = {
        let session = session.read().await;
        let base = session.segments.get(&base_proxy_seq).expect("lease-specific terminal base");
        (base.cache_key.clone(), Arc::clone(&base.access))
    };
    let gc = HlsGarbageCollector::new(
        Arc::clone(&sessions),
        Arc::clone(&cache),
        GarbageCollectionPolicy {
            cache_duration_ms: 0,
            cache_bytes_global: 10_000_000,
            cache_bytes_per_session: 10_000_000,
            session_idle_timeout_ms: u64::MAX,
            temp_file_retention_ms: 30_000,
            failed_segment_retention_ms: 10,
        },
        build_rewrite_secret_fingerprint(b"secret"),
    );

    let evidence = super::super::prepare_terminal_base_evidence(&session, &cache, &manifest, 5).await;
    assert_eq!(evidence.track_base().map(|base| base.proxy_seq), Some(base_proxy_seq));
    assert!(evidence.track_signature().is_some());
    assert!(base_access.active_readers() > 0);
    let pinned_report = gc.run_once(10_000).await.expect("GC runs while critical base evidence is pinned");
    assert_eq!(pinned_report.segments_deleted_duration, 0);
    assert!(cache.metadata(&base_key).await.expect("pinned base metadata reads").is_some());

    evidence.release();
    assert_eq!(base_access.active_readers(), 0);
    let released_report = gc.run_once(10_001).await.expect("GC runs after critical base evidence release");
    assert!(released_report.segments_deleted_duration > 0);
    assert!(cache.metadata(&base_key).await.expect("released base metadata reads").is_none());
}

fn candidate_handoff_preview(
    session: &HlsSession,
    body: &str,
    now_ms: u64,
) -> (
    tuliprox_parser::hls::origin_manifest::ParsedOriginManifest,
    Vec<crate::TransientResourceRef>,
    crate::timeline::HlsOriginHandoffPreview,
) {
    let final_url = "https://candidate.example/live/index.m3u8";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(mut manifest) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(body, final_url)
    else {
        panic!("candidate fixture must parse as a normal manifest");
    };
    let key_resources = super::commit::materialize_normal_key_resources(&mut manifest, b"secret", now_ms, 60_000);
    let preview = session
        .preview_origin_handoff_manifest(&manifest, crate::timeline::effective_origin_host_id("candidate.example"), 0)
        .expect("candidate handoff preview");
    (manifest, key_resources, preview)
}

#[test]
fn hls_recovery_timing_candidate_preview_uses_actual_map_medium_not_playlist_head() {
    let session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\nhead40.ts\n\
        #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nrecovery41.m4s\n";
    let (manifest, key_resources, preview) = candidate_handoff_preview(&session, body, 100);
    assert!(manifest.segments.first().is_some_and(|segment| segment.map_ref.is_none()));
    let actual_recovery_segment = preview.segments.get(1).expect("actual recovery medium after clear head");
    let required_map =
        actual_recovery_segment.map_ref.and_then(|map_id| preview.maps.iter().find(|map| map.proxy_map_id == map_id));

    let workload = super::switch_staging::handoff_preview_recovery_workload(
        &session,
        actual_recovery_segment,
        required_map,
        &key_resources,
        100,
    );

    assert_eq!(workload.segment, HlsRecoverySegmentWorkload::ClearSegmentFetch);
    assert_eq!(workload.map, HlsRecoveryMapWorkload::Fetch);
}

#[test]
fn hls_recovery_timing_candidate_preview_detects_generation_local_ready_aes128_key() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nrecovery40.ts\n";
    let (_, key_resources, preview) = candidate_handoff_preview(&session, body, 100);
    let key_resource = key_resources.first().cloned().expect("candidate key resource");
    session.transient.upsert_resources([key_resource.clone()]);
    let proxy_session_id = session.proxy_session_id.clone();
    let fetch = session.transient.begin_object_fetch(&proxy_session_id, &key_resource, "bin", 100, 60_000);
    let TransientObjectFetchDecision::Fetch(token) = fetch else {
        panic!("new candidate key starts one cache fill");
    };
    assert!(session.transient.mark_object_ready_if_current(
        &token,
        "application/octet-stream".to_string(),
        16,
        100,
        60_100,
    ));
    let actual_recovery_segment = preview.segments.first().expect("AES recovery medium");

    let workload = super::switch_staging::handoff_preview_recovery_workload(
        &session,
        actual_recovery_segment,
        None,
        &key_resources,
        101,
    );

    assert_eq!(workload.segment, HlsRecoverySegmentWorkload::Aes128SegmentFetchWithReadyKey);
    assert_eq!(
        super::switch_staging::staged_switch_media_compatibility(
            &session,
            actual_recovery_segment,
            &key_resources,
            101
        ),
        super::switch_staging::HlsStagedSwitchMediaCompatibility::Compatible
    );
    assert_eq!(
        super::switch_staging::staged_switch_media_compatibility(
            &session,
            actual_recovery_segment,
            &key_resources,
            60_101
        ),
        super::switch_staging::HlsStagedSwitchMediaCompatibility::RequiresUnstagedEncryptionKey,
        "the final commit revalidation must reject an expired READY key"
    );
}

#[test]
fn hls_recovery_timing_candidate_preview_requires_aes128_key_fetch_without_ready_evidence() {
    let session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\nrecovery40.ts\n";
    let (_, key_resources, preview) = candidate_handoff_preview(&session, body, 100);
    let actual_recovery_segment = preview.segments.first().expect("AES recovery medium");

    let workload = super::switch_staging::handoff_preview_recovery_workload(
        &session,
        actual_recovery_segment,
        None,
        &key_resources,
        101,
    );

    assert_eq!(workload.segment, HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch);
    assert_eq!(
        super::switch_staging::staged_switch_media_compatibility(
            &session,
            actual_recovery_segment,
            &key_resources,
            101
        ),
        super::switch_staging::HlsStagedSwitchMediaCompatibility::RequiresUnstagedEncryptionKey
    );
}

async fn mark_full_burst_ready_for_switch_staging(session: &Arc<RwLock<HlsSession>>, fetched: &FetchedOriginManifest) {
    let candidate_workload = {
        let session = session.read().await;
        let (_, key_resources, preview) = candidate_handoff_preview(&session, &fetched.body, 100);
        let first_segment = preview.segments.first().expect("switch candidate recovery segment");
        let required_map =
            first_segment.map_ref.and_then(|map_id| preview.maps.iter().find(|map| map.proxy_map_id == map_id));
        super::switch_staging::handoff_preview_recovery_workload(
            &session,
            first_segment,
            required_map,
            &key_resources,
            100,
        )
    };
    let mut session = session.write().await;
    let burst_plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
    session.origin_control.begin_acceptance_episode(
        100,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &test_switch_staging_acceptance_episode_timing(100, burst_plan),
    );
    let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
    episode.record_full_burst();
    episode.state = super::super::manifest_acceptance::HlsManifestAcceptanceState::StagingSwitchSegment;
    let effective_host = fetched_effective_manifest_host(fetched);
    let identity = super::super::manifest_acceptance::HlsManifestRecoveryCandidateIdentity::from_candidate(
        0,
        effective_host.as_deref(),
        &fetched.body,
    );
    assert_eq!(
        episode.select_candidate(episode.generation, identity),
        super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied
    );
    let mut binding_probe = episode.clone();
    assert_eq!(
        binding_probe.bind_selected_candidate(episode.generation, identity, candidate_workload),
        super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied,
        "switch-staging fixture must admit the selected candidate before network staging"
    );
}

#[test]
fn switch_staging_rejects_map_fetch_outside_fixture_envelope_before_network() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let burst_plan = HlsManifestRecoveryBurstLevel::Friendly.plan();
    session.origin_control.begin_acceptance_episode(
        100,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &test_acceptance_episode_timing(100, burst_plan),
    );
    let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
    episode.record_full_burst();
    episode.state = super::super::manifest_acceptance::HlsManifestAcceptanceState::StagingSwitchSegment;
    let identity = super::super::manifest_acceptance::HlsManifestRecoveryCandidateIdentity::from_candidate(
        0,
        Some("candidate.example"),
        &switch_manifest_body(true),
    );
    assert_eq!(
        episode.select_candidate(episode.generation, identity),
        super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::Applied
    );
    let map_fetch = HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
        segment: HlsRecoveryObjectReadiness::Fetch,
        map: Some(HlsRecoveryObjectReadiness::Fetch),
        encryption: HlsRecoveryEncryptionReadiness::Clear,
    });

    assert_eq!(
        episode.bind_selected_candidate(episode.generation, identity, map_fetch),
        super::super::manifest_acceptance::HlsRecoveryWorkloadBindingUpdate::OutsideEnvelope
    );
}

async fn assert_incompatible_switch_is_rejected_before_timeline_commit(
    baseline_body: &str,
    candidate_body: &str,
    expected_reason: HlsManifestRejectLogReason,
) {
    let session = test_session();
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(baseline) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            baseline_body,
            "http://previous.example.com/live/index.m3u8",
        )
    else {
        panic!("baseline parses as normal timeline");
    };
    {
        let mut session = session.write().await;
        session
            .apply_origin_manifest_for_host(
                &baseline,
                crate::timeline::effective_origin_host_id("previous.example.com"),
            )
            .expect("baseline timeline commits");
        session.last_effective_manifest_host = Some("previous.example.com".to_string());
        session.origin_control.pinned_host = Some("previous.example.com".to_string());
        session.origin_control.origin_epoch = session.origin_epoch;
    }
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let base_url = "http://127.0.0.1:9";
    let request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), base_url);
    let fetched = FetchedOriginManifest {
        body: candidate_body.to_string(),
        final_manifest_url: format!("{base_url}/live/final/index.m3u8"),
        resolved_request_url: format!("{base_url}/live/user/pass/12345.m3u8"),
        redirect_host: None,
        provider_url_index: None,
        provider_session_headers: HeaderMap::new(),
        status: StatusCode::OK,
        attempts: 1,
        candidate_requests: 1,
        selection: HlsManifestFetchSelection::Initial,
    };
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let before = {
        let session = session.read().await;
        (session.origin_epoch, session.proxy_next_seq, session.segments.len(), session.maps.len())
    };

    let result = super::commit_manifest_recovery_candidate(
        &request,
        fetched,
        HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
    )
    .await;

    assert!(matches!(
        result,
        Err(HlsManifestCommitError::TimelineRejected { reason }) if reason == expected_reason
    ));
    let session = session.read().await;
    assert_eq!((session.origin_epoch, session.proxy_next_seq, session.segments.len(), session.maps.len()), before);
    drop(session);
    assert!(!cache.has_active_temp_files());
}

#[tokio::test]
async fn staged_cross_host_aes_candidate_without_ready_key_evidence_never_commits() {
    let baseline = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\nold1.ts\n#EXTINF:4.0,\nold2.ts\n#EXTINF:4.0,\nold3.ts\n";
    let candidate = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.php\"\n\
        #EXTINF:4.0,\nnew40.ts\n#EXTINF:4.0,\nnew41.ts\n#EXTINF:4.0,\nnew42.ts\n";

    assert_incompatible_switch_is_rejected_before_timeline_commit(
        baseline,
        candidate,
        HlsManifestRejectLogReason::SwitchEncryptionKeyNotReady,
    )
    .await;
}

#[tokio::test]
async fn staged_cross_host_aes_candidate_with_generation_local_ready_key_commits() {
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let origin = spawn_controlled_switch_origin().await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let now_ms = current_time_millis();
    let candidate = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.php\"\n\
        #EXTINF:4.0,\nfirst.ts\n#EXTINF:4.0,\nsecond.ts\n";
    let mut fetched = switch_fetched_manifest(&origin.base_url, false);
    fetched.body = candidate.to_string();
    let key_resource = {
        let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(mut manifest) =
            tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(candidate, &fetched.final_manifest_url)
        else {
            panic!("AES switch manifest parses as a normal timeline");
        };
        super::commit::materialize_normal_key_resources(&mut manifest, b"secret", now_ms, 300_000)
            .into_iter()
            .next()
            .expect("AES switch candidate has one key resource")
    };
    {
        let mut session = session.write().await;
        session.transient.upsert_resources([key_resource.clone()]);
        let proxy_session_id = session.proxy_session_id.clone();
        let extension = key_resource.file_ext_hint.as_deref().expect("key extension");
        let fetch = session.transient.begin_object_fetch(&proxy_session_id, &key_resource, extension, now_ms, 300_000);
        let TransientObjectFetchDecision::Fetch(token) = fetch else {
            panic!("candidate key starts one cache fill");
        };
        assert!(session.transient.mark_object_ready_if_current(
            &token,
            "application/octet-stream".to_string(),
            16,
            now_ms,
            now_ms.saturating_add(300_000),
        ));
    }
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let mut request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    request.now_ms = now_ms;
    let mut commit_task = tokio::spawn(async move {
        super::commit_manifest_recovery_candidate(
            &request,
            fetched,
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
        )
        .await
        .map(|_| ())
    });

    await_controlled_switch_segment_prefix(&origin, &mut commit_task, "encrypted cross-host switch commit").await;
    origin.release_segment_body.notify_one();
    commit_task.await.expect("encrypted switch commit task").expect("encrypted switch commit succeeds");

    let session = session.read().await;
    let switched = session
        .segments
        .values()
        .find(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
        .expect("switched segment committed");
    assert!(switched.encryption.is_some());
    assert!(session
        .transient
        .ready_key_object_valid_until_ms(
            &session.proxy_session_id,
            &key_resource.id,
            key_resource.file_ext_hint.as_deref().expect("key extension"),
            current_time_millis(),
        )
        .is_some());
}

#[tokio::test]
async fn mapped_fmp4_tail_cannot_handoff_to_mapless_ts_timeline() {
    let baseline = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-TARGETDURATION:4\n\
        #EXT-X-MAP:URI=\"init.mp4\"\n\
        #EXTINF:4.0,\nold1.m4s\n#EXTINF:4.0,\nold2.m4s\n#EXTINF:4.0,\nold3.m4s\n";
    let candidate = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:40\n#EXT-X-TARGETDURATION:4\n\
        #EXTINF:4.0,\nnew40.ts\n#EXTINF:4.0,\nnew41.ts\n#EXTINF:4.0,\nnew42.ts\n";

    assert_incompatible_switch_is_rejected_before_timeline_commit(
        baseline,
        candidate,
        HlsManifestRejectLogReason::SwitchMapResetUnsupported,
    )
    .await;
}

#[tokio::test]
async fn cross_host_switch_commits_only_after_complete_map_and_segment_staging() {
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let origin = spawn_controlled_switch_origin().await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let fetched = switch_fetched_manifest(&origin.base_url, true);
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let effective_host = host_from_base_url(&origin.base_url);
    let effective_host_id = crate::timeline::effective_origin_host_id(&effective_host);
    let (baseline_epoch, baseline_proxy_next, staged_segment_key, staged_map_key) = {
        let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
            tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
                &fetched.body,
                &fetched.final_manifest_url,
            )
        else {
            panic!("switch manifest parses as normal timeline");
        };
        let session = session.read().await;
        let preview = session.preview_origin_handoff_manifest(&manifest, effective_host_id, 0).expect("switch preview");
        (
            session.origin_epoch,
            session.proxy_next_seq,
            preview.segments.first().expect("first staged segment").cache_key.clone(),
            preview.maps.first().expect("required staged map").cache_key.clone(),
        )
    };
    let request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    let cleanup_manager = Arc::clone(&request.hls_proxy);
    let mut commit_task = tokio::spawn(async move {
        super::commit_manifest_recovery_candidate(
            &request,
            fetched,
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
        )
        .await
        .map(|_| ())
    });

    await_controlled_switch_segment_prefix(&origin, &mut commit_task, "cross-host switch commit").await;

    assert!(!commit_task.is_finished());
    {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch);
        assert_eq!(session.proxy_next_seq, baseline_proxy_next);
        assert!(session.segments.values().all(|segment| segment.origin_key.origin_epoch == baseline_epoch));
    }
    assert_eq!(cache.metadata(&staged_segment_key).await.expect("segment metadata"), None);
    assert_eq!(cache.metadata(&staged_map_key).await.expect("map metadata"), None);
    assert_eq!(
        *origin.requests.lock().await,
        vec!["/live/final/init.mp4".to_string(), "/live/final/first.ts".to_string()]
    );

    origin.release_segment_body.notify_one();
    commit_task.await.expect("switch commit task").expect("switch commit succeeds");
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (0, 0));

    let (first_segment_key, first_map_key) = {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch.saturating_add(1));
        assert_eq!(session.origin_epoch_effective_host_id, Some(effective_host_id));
        let switched = session
            .segments
            .values()
            .filter(|segment| segment.origin_key.origin_epoch == session.origin_epoch)
            .collect::<Vec<_>>();
        assert_eq!(switched.len(), 2);
        assert_eq!(switched[0].origin_key.host_local_sequence, 500);
        assert!(switched[0].discontinuity_before);
        assert!(matches!(
            switched[0].status,
            SegmentCacheStatus::Ready { content_length, .. }
                if content_length == u64::try_from(SWITCH_SEGMENT_BODY.len()).unwrap_or(u64::MAX)
        ));
        assert!(!matches!(switched[1].status, SegmentCacheStatus::Ready { .. }));
        let map_id = switched[0].map_ref.expect("first switched segment requires map");
        assert_eq!(switched[1].map_ref, Some(map_id));
        let map = session.maps.get(&map_id).expect("staged map committed to timeline");
        assert!(matches!(
            map.status,
            MapCacheStatus::Ready { content_length, .. }
                if content_length == u64::try_from(SWITCH_MAP_BODY.len()).unwrap_or(u64::MAX)
        ));
        (switched[0].cache_key.clone(), map.cache_key.clone())
    };
    let mut segment_file = cache.open_range(&first_segment_key, 0).await.expect("open staged segment");
    let mut segment_bytes = Vec::new();
    segment_file.read_to_end(&mut segment_bytes).await.expect("read staged segment");
    assert_eq!(segment_bytes, SWITCH_SEGMENT_BODY);
    let mut map_file = cache.open_range(&first_map_key, 0).await.expect("open staged map");
    let mut map_bytes = Vec::new();
    map_file.read_to_end(&mut map_bytes).await.expect("read staged map");
    assert_eq!(map_bytes, SWITCH_MAP_BODY);
}

#[tokio::test]
async fn caller_cancellation_after_owned_switch_staging_queues_and_collects_rollback() {
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let origin = spawn_controlled_switch_origin().await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let fetched = switch_fetched_manifest(&origin.base_url, true);
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let effective_host_id = crate::timeline::effective_origin_host_id(&host_from_base_url(&origin.base_url));
    let (baseline_epoch, staged_segment_key, staged_map_key) = {
        let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
            tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
                &fetched.body,
                &fetched.final_manifest_url,
            )
        else {
            panic!("switch manifest parses as normal timeline");
        };
        let session = session.read().await;
        let preview = session.preview_origin_handoff_manifest(&manifest, effective_host_id, 0).expect("switch preview");
        (
            session.origin_epoch,
            preview.segments.first().expect("first staged segment").cache_key.clone(),
            preview.maps.first().expect("required staged map").cache_key.clone(),
        )
    };
    let request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    let cleanup_manager = Arc::clone(&request.hls_proxy);
    let (staging_complete_tx, staging_complete_rx) = tokio::sync::oneshot::channel();
    let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
    let mut staging_task = tokio::spawn(async move {
        let staged = super::switch_staging::stage_alternative_manifest_switch(&request, &fetched)
            .await
            .expect("switch staging succeeds");
        staging_complete_tx.send(()).expect("staging completion receiver remains");
        let _ = hold_rx.await;
        drop(staged);
    });

    await_controlled_switch_segment_prefix(&origin, &mut staging_task, "owned switch staging").await;
    origin.release_segment_body.notify_one();
    staging_complete_rx.await.expect("owned cache commits complete");
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (0, 2));
    assert!(cache.metadata(&staged_segment_key).await.expect("staged segment metadata").is_some());
    assert!(cache.metadata(&staged_map_key).await.expect("staged map metadata").is_some());

    staging_task.abort();
    assert!(staging_task.await.expect_err("staging task is cancelled").is_cancelled());
    drop(hold_tx);
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (2, 0));
    assert_eq!(session.read().await.origin_epoch, baseline_epoch);

    let report = cleanup_manager.run_garbage_collection_once(200).await.expect("rollback GC succeeds");
    assert_eq!(report.cache_object_deletions_succeeded, 2);
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (0, 0));
    assert_eq!(cache.metadata(&staged_segment_key).await.expect("rolled-back segment metadata"), None);
    assert_eq!(cache.metadata(&staged_map_key).await.expect("rolled-back map metadata"), None);
}

#[derive(Clone, Copy)]
enum StaleSwitchGeneration {
    Acceptance,
    Progress,
    PinnedHostRecovered,
}

async fn assert_stale_switch_generation_rejects_commit(stale_generation: StaleSwitchGeneration) {
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let origin = spawn_controlled_switch_origin().await;
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let fetched = switch_fetched_manifest(&origin.base_url, true);
    mark_full_burst_ready_for_switch_staging(&session, &fetched).await;
    let effective_host_id = crate::timeline::effective_origin_host_id(&host_from_base_url(&origin.base_url));
    let (baseline_epoch, baseline_proxy_next, staged_segment_key, staged_map_key) = {
        let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) =
            tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
                &fetched.body,
                &fetched.final_manifest_url,
            )
        else {
            panic!("switch manifest parses as normal timeline");
        };
        let session = session.read().await;
        let preview = session.preview_origin_handoff_manifest(&manifest, effective_host_id, 0).expect("switch preview");
        (
            session.origin_epoch,
            session.proxy_next_seq,
            preview.segments.first().expect("first staged segment").cache_key.clone(),
            preview.maps.first().expect("staged map").cache_key.clone(),
        )
    };
    let request = switch_test_request(Arc::clone(&session), Arc::clone(&cache), &origin.base_url);
    let cleanup_manager = Arc::clone(&request.hls_proxy);
    let mut commit_task = tokio::spawn(async move {
        super::commit_manifest_recovery_candidate(
            &request,
            fetched,
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate,
        )
        .await
        .map(|_| ())
    });

    await_controlled_switch_segment_prefix(&origin, &mut commit_task, "stale-generation switch commit").await;
    {
        let mut session = session.write().await;
        match stale_generation {
            StaleSwitchGeneration::Acceptance => {
                let episode = session.origin_control.acceptance_episode.as_mut().expect("active acceptance episode");
                episode.generation = super::super::manifest_acceptance::HlsManifestAcceptanceGeneration(
                    episode.generation.0.saturating_add(1),
                );
            }
            StaleSwitchGeneration::Progress => {
                session.origin_control.progress_generation =
                    session.origin_control.progress_generation.saturating_add(1);
            }
            StaleSwitchGeneration::PinnedHostRecovered => {
                session.origin_control.pinned_host = Some("recovered.example.com".to_string());
                session.last_effective_manifest_host = Some("recovered.example.com".to_string());
            }
        }
    }
    origin.release_segment_body.notify_one();

    let result = commit_task.await.expect("stale switch task");
    assert!(matches!(
        result,
        Err(HlsManifestCommitError::TimelineRejected { reason: HlsManifestRejectLogReason::StagedSwitchInvalidated })
    ));
    {
        let session = session.read().await;
        assert_eq!(session.origin_epoch, baseline_epoch);
        assert_eq!(session.proxy_next_seq, baseline_proxy_next);
        assert!(session.segments.values().all(|segment| segment.origin_key.origin_epoch == baseline_epoch));
    }
    assert_eq!(cache.metadata(&staged_segment_key).await.expect("stale segment metadata"), None);
    assert_eq!(cache.metadata(&staged_map_key).await.expect("stale map metadata"), None);
    assert_eq!(cleanup_manager.cache_deletion_queue_usage(), (0, 0));
    assert!(!cache.has_active_temp_files());
}

#[tokio::test]
async fn stale_acceptance_generation_rejects_and_removes_completed_switch_staging() {
    assert_stale_switch_generation_rejects_commit(StaleSwitchGeneration::Acceptance).await;
}

#[tokio::test]
async fn stale_progress_generation_rejects_and_removes_completed_switch_staging() {
    assert_stale_switch_generation_rejects_commit(StaleSwitchGeneration::Progress).await;
}

#[tokio::test]
async fn pinned_origin_recovery_during_staging_rejects_alternative_commit() {
    assert_stale_switch_generation_rejects_commit(StaleSwitchGeneration::PinnedHostRecovered).await;
}

#[tokio::test]
async fn switch_segment_staging_failure_holds_episode_without_timeline_mutation() {
    let manifest = switch_manifest_body(false);
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path_has_extension(&path, "m3u8") {
            (200, Vec::new(), manifest.clone())
        } else {
            (404, Vec::new(), String::new())
        }
    }))
    .await;
    let temp_dir = tempfile::tempdir().expect("switch cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let session = test_session();
    prepare_cross_host_baseline(&session).await;
    let (baseline_epoch, baseline_proxy_next, baseline_sequences) = {
        let session = session.read().await;
        (session.origin_epoch, session.proxy_next_seq, session.segments.keys().copied().collect::<Vec<_>>())
    };
    let request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    let target_url = Url::parse(&format!("{}/live/user/pass/12345.m3u8", origin.base_url)).expect("target url");

    let result = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await;

    assert!(matches!(result, Err(OriginManifestFetchError::RetryExhausted)));
    let session = session.read().await;
    assert_eq!(session.origin_epoch, baseline_epoch);
    assert_eq!(session.proxy_next_seq, baseline_proxy_next);
    assert_eq!(session.segments.keys().copied().collect::<Vec<_>>(), baseline_sequences);
    let episode = session.origin_control.acceptance_episode.as_ref().expect("failed switch episode retained");
    assert!(episode.full_burst_completed);
    assert_eq!(episode.full_bursts_completed, 1);
    assert_eq!(episode.state, super::super::manifest_acceptance::HlsManifestAcceptanceState::Holding);
    drop(session);
    let requests = origin.requests.lock().await;
    assert_eq!(requests.iter().filter(|path| path_has_extension(path, "ts")).count(), 1);
}

#[tokio::test]
async fn recovery_required_orchestrator_requalifies_changed_pinned_landscape_with_full_plan() {
    let plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let entry_hits = Arc::new(AtomicUsize::new(0));
    let entry_hits_for_handler = Arc::clone(&entry_hits);
    let pinned_hits = Arc::new(AtomicUsize::new(0));
    let pinned_hits_for_handler = Arc::clone(&pinned_hits);
    let origin_port = Arc::new(AtomicUsize::new(0));
    let origin_port_for_handler = Arc::clone(&origin_port);
    let origin = spawn_test_origin(Arc::new(move |path| {
        if path == "/live/user/pass/12345.m3u8" {
            let hit = entry_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            if hit < plan.total_candidates() {
                return (200, Vec::new(), switch_manifest_body(false));
            }
            return (
                302,
                vec![(
                    "Location",
                    format!("http://127.0.0.1:{}/pinned/index.m3u8", origin_port_for_handler.load(Ordering::SeqCst)),
                )],
                String::new(),
            );
        }
        if path == "/pinned/index.m3u8" {
            pinned_hits_for_handler.fetch_add(1, Ordering::SeqCst);
            return (
                200,
                Vec::new(),
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\npinned101.ts\n".to_string(),
            );
        }
        (404, Vec::new(), String::new())
    }))
    .await;
    let parsed_origin_url = Url::parse(&origin.base_url).expect("origin base URL");
    origin_port.store(usize::from(parsed_origin_url.port().expect("test origin port")), Ordering::SeqCst);

    let session = test_session();
    let baseline = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\npinned100.ts\n";
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(baseline) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            baseline,
            "http://127.0.0.1/pinned/index.m3u8",
        )
    else {
        panic!("pinned baseline parses");
    };
    {
        let mut session = session.write().await;
        session
            .apply_origin_manifest_for_host(&baseline, crate::timeline::effective_origin_host_id("127.0.0.1"))
            .expect("pinned baseline commits");
        session.last_effective_manifest_host = Some("127.0.0.1".to_string());
        session.origin_control.pinned_host = Some("127.0.0.1".to_string());
        session.origin_control.origin_epoch = session.origin_epoch;
    }

    let temp_dir = tempfile::tempdir().expect("orchestrator cache tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let mut request = switch_test_request(Arc::clone(&session), cache, &origin.base_url);
    let alternative_entry =
        format!("{}/live/user/pass/12345.m3u8", origin.base_url).replacen("127.0.0.1", "localhost", 1);
    request.origin_entry = LiveHlsOriginEntry::parse(&alternative_entry).expect("alternative entry URL");
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    let target_url = Url::parse(&alternative_entry).expect("alternative target URL");

    let committed = retry_test_manifest_recovery_chain(
        &request,
        target_url,
        HlsManifestRejectLogReason::PinnedHostRecoveryRejected,
    )
    .await
    .expect("pinned follow-up commits");

    assert_eq!(committed.fetched.redirect_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(entry_hits.load(Ordering::SeqCst), plan.total_candidates().saturating_mul(2).saturating_add(1));
    assert_eq!(pinned_hits.load(Ordering::SeqCst), plan.total_candidates().saturating_add(1));
    let session = session.read().await;
    assert_eq!(session.origin_seq_highwater, Some(101));
    assert_eq!(session.last_effective_manifest_host.as_deref(), Some("127.0.0.1"));
    let episode = session.origin_control.acceptance_episode.as_ref().expect("completed acceptance episode");
    assert_eq!(episode.full_bursts_completed, 1);
    assert_eq!(episode.state, super::super::manifest_acceptance::HlsManifestAcceptanceState::Completed);
}

#[tokio::test]
async fn provider_preflight_failure_real_refresh_path_has_zero_candidates() {
    let origin = spawn_test_origin(Arc::new(|_path| (200, Vec::new(), manifest_body()))).await;
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let session = test_session();
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    session.write().await.origin_account_binding = Some(HlsOriginAccountBinding::new(
        Arc::from("missing-input"),
        Arc::from("missing-account"),
        &proxy_session_id,
        0,
    ));
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), ctx);
    request.manifest_recovery_burst = HlsManifestRecoveryBurstConfig { level: HlsManifestRecoveryBurstLevel::Beast };
    request.origin_entry =
        LiveHlsOriginEntry::parse(&format!("{}/live/index.m3u8", origin.base_url)).expect("unused origin URL");
    request.origin_io = Some(HlsOriginIoContext {
        ctx: ctx.clone(),
        client_addr: "127.0.0.1:12345".parse().expect("test client address"),
        allow_grace: false,
        priority: 0,
        connection_kind: ConnectionKind::Normal,
        reservation_ttl_secs: 60,
        preacquired_provider_handle: None,
        started_generation: None,
    });
    let metrics = Arc::clone(request.segment_worker_pool.metrics());
    let error = super::provider_preflight_manifest_error(HlsBoundAccountAcquireErrorKind::Missing);

    assert!(matches!(&error, OriginManifestFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Missing)));
    assert!(trigger_origin_refresh_sync(request).await);
    assert!(origin.requests.lock().await.is_empty());

    let session = session.read().await;
    assert!(session.origin_control.acceptance_episode.is_none());
    assert_eq!(session.origin_control.progress_phase, super::super::origin_progress::HlsOriginProgressPhase::Cold);
    assert_eq!(
        session.origin_control.path_condition,
        super::super::origin_progress::HlsOriginPathCondition::HardFetchFailure
    );
    assert!(session.origin_control.last_origin_response_at_ms.is_none());
    assert!(session.origin_control.manifest_origin_binding.is_none());
    assert!(session.last_rendered_manifest.is_none());
    assert!(session.published_live_origin_baseline.is_none());
    assert!(session.established_manifest_recovery_binding().is_none());
    drop(session);
    let metrics = metrics.snapshot();
    assert_eq!(metrics.refresh_started, 1);
    assert_eq!(metrics.refresh_completed, 0);
    assert_eq!(metrics.refresh_retried, 0);
    assert_eq!(metrics.refresh_skipped, 0);
    assert_eq!(metrics.refresh_failed, 1);
}
