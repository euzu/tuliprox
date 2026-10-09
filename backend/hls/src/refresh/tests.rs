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

mod acceptance;
mod archive_master;
mod critical_handoff;
mod headers_redirects;
mod origin_binding;
mod ownership;
mod retry_policy;
mod startup_representation;
mod support;
mod switch_staging;
mod timing;
mod transient;

use self::support::{
    assert_incompatible_switch_is_rejected_before_timeline_commit, assert_stale_switch_generation_rejects_commit,
    await_controlled_switch_segment_prefix, bind_refresh_request_to_app_state, candidate_handoff_preview,
    commit_ready_baseline_snapshot, critical_handoff_app_config, fetched_manifest, host_from_base_url,
    install_published_recovery_binding, manifest_body, mark_full_burst_ready_for_switch_staging, no_delay_policy,
    path_has_extension, post_refresh_live_manifest_snapshot, prepare_active_critical_handoff_lease,
    prepare_cross_host_baseline, publish_ready_test_manifest, refresh_session_with_origin_body,
    repeated_transient_manifest, request_header_value, retry_test_manifest_recovery_chain,
    spawn_controlled_switch_origin, spawn_critical_emergency_origin, spawn_critical_emergency_origin_with_control,
    spawn_test_origin, switch_fetched_manifest, switch_manifest_body, switch_test_request,
    test_acceptance_episode_timing, test_app_config, test_deterministic_timeline_conflict,
    test_manifest_origin_binding, test_origin_refresh_request, test_recovery_timing_policy,
    test_segment_repair_manager, test_session, three_segment_manifest_body, CriticalEmergencyOriginServer,
    StaleSwitchGeneration, TestOriginServer, CRITICAL_HANDOFF_MANIFEST_BODY, CRITICAL_HANDOFF_TS_BODY, SWITCH_MAP_BODY,
    SWITCH_SEGMENT_BODY,
};
