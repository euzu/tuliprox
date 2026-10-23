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
mod account_handoff;
mod archive_master;
mod critical_handoff;
mod headers_redirects;
mod origin_binding;
mod ownership;
mod reduced_retry;
mod retry_policy;
mod short_manifest;
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

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Keeps the degraded admission and unchanged-success counterprobe in one scenario."
)]
async fn retryable_manifest_failure_keeps_optimistic_admission_degraded_until_media_progress() -> std::io::Result<()> {
    use crate::{
        media_reserve::{
            evaluate_startup_admission, HlsReadyTimelineSnapshot, HlsStartupAdmissionDecision,
            HlsStartupAdmissionInput, HlsStartupAdmissionOriginState,
        },
        origin_progress::HlsOriginPathCondition,
    };
    use std::sync::Arc;
    let session = test_session();
    let context = HlsOriginManifestFetchContext {
        app_config: test_app_config(),
        session: Arc::clone(&session),
        availability_runtime: None,
        origin_entry: LiveHlsOriginEntry::parse("http://127.0.0.1:9/index.m3u8").unwrap(),
        headers: axum::http::HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::new(),
        use_manual_redirects: false,
        origin_manifest_timeout_ms: 1000,
        manifest_recovery_burst: tuliprox_core::model::HlsManifestRecoveryBurstConfig::default(),
        retry_policy: RetryPolicy::default(),
        recovery_timing_policy: test_recovery_timing_policy(100),
        acceptance_timing_seed: None,
    };
    let manifest = fetched_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4,\n0.ts\n#EXTINF:4,\n1.ts\n#EXTINF:4,\n2.ts\n");
    let tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(origin) =
        tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
            &manifest.body,
            &manifest.final_manifest_url,
        )
    else {
        panic!("normal");
    };
    let snapshot = {
        let mut session = session.write().await;
        session.apply_origin_manifest(&origin).unwrap();
        session.origin_control.record_media_progress(10, 4000);
        let config = tuliprox_core::model::HlsStartupConfig {
            mode: shared::model::HlsStartupMode::Optimistic,
            ..Default::default()
        };
        let store = Arc::new(crate::SegmentRevisionStore::default());
        let mut revisions = std::collections::BTreeMap::new();
        for seq in 0..3 {
            revisions
                .insert(seq, store.create(session.proxy_session_id.clone(), seq, crate::SegmentRevisionKind::Raw)?);
        }
        revisions[&0].revision().prefix_available.store(1, std::sync::atomic::Ordering::Release);
        session.startup = Some(crate::HlsSessionStartup {
            config: config.clone(),
            first_data_timeout_ms: 10000,
            worker: std::sync::Weak::new(),
            access_leases: Arc::new(tokio::sync::RwLock::new(crate::HlsAccessLeaseStore::default())),
            revisions,
            policy_fixed: true,
            store,
            budget: crate::ProgressiveBudgetManager::new(config),
        });
        let rendered = session.render_and_store_manifest(10).unwrap();
        crate::api::derive_hls_lease_manifest_snapshot(
            &crate::api::HlsLeaseManifestSnapshotInput::NormalCacheTimeline {
                session: &session,
                committed_body: &rendered.body,
                materialized_body: &rendered.body,
                stripped_tail_segments: 0,
            },
            10,
        )
        .unwrap()
        .unwrap()
    };
    let evaluate = |origin_state| {
        evaluate_startup_admission(HlsStartupAdmissionInput {
            manifest: &snapshot,
            ready_timeline: &HlsReadyTimelineSnapshot { units: Arc::from([]) },
            origin_state,
            recovery_trigger_budget: crate::recovery_timing::HlsRecoveryTriggerBudgetMs::from_millis(0),
        })
        .decision
    };
    assert_eq!(evaluate(HlsStartupAdmissionOriginState::Healthy), HlsStartupAdmissionDecision::Admit);
    let generation = session.read().await.origin_control.progress_generation;
    crate::availability::record_retryable_manifest_attempt_failure(
        &context,
        generation,
        &OriginManifestFetchError::RetryableStatus(axum::http::StatusCode::BAD_GATEWAY, None),
        11,
    )
    .await;
    assert_eq!(session.read().await.origin_control.path_condition, HlsOriginPathCondition::RetryableFetchFailure);
    assert!(matches!(evaluate(HlsStartupAdmissionOriginState::Degraded), HlsStartupAdmissionDecision::Reject(_)));
    {
        let mut session = session.write().await;
        let timing = build_manifest_refresh_timing(Some(4000), Some(4000), HlsManifestProgress::Unchanged);
        record_committed_manifest_media_progress(
            &mut session.origin_control,
            HlsManifestCommitProgressEvidence::CacheTimeline(timing),
            Some(4),
            12,
        );
        assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::RetryableFetchFailure);
        session.origin_control.record_media_progress(13, 4000);
        assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::ProgressExpected);
    }
    Ok(())
}
