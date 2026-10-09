use super::{
    prepared_terminal_bundle_key, snapshot_terminal_media_asset, HlsAcceptanceEpisodeTiming,
    HlsAcceptanceEpisodeTimingInput, HlsLeaseCutoverTiming, HlsManifestAcceptanceGeneration,
    HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsProxyManager, HlsRecoveryEtaMs, HlsRecoveryTimingPolicy,
    HlsRecoveryWorkload, HlsRuntimeCustomTailAssetIdentity, HlsTerminalAssetRevisionGuard,
    HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitMediaGuard, HlsTerminalCommitOutcome,
    HlsTerminalCommitPayload, HlsTerminalCommitRequest, HlsTerminalCommitWindow, HlsTerminalMediaPreparationState,
    HlsTerminalTailPlan, HlsTerminalTailPreparationRequest, HlsTransitionMarginMs,
};
use crate::{
    build_terminal_tail_plan,
    media_reserve::{HlsLeaseReserveAvailabilityBasis, HlsLeaseReserveSnapshot, HlsManifestCommitIdentity},
    HlsAccessLease, HlsAccessLeaseId, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot,
    HlsManifestAcceptanceExhaustionReason, HlsManifestAcceptanceTrigger, HlsManifestDeliveryMode, HlsMediaContainer,
    HlsMediaLeaseIdentity, HlsPlaybackFamilyKey, HlsSessionHandle, HlsSessionKey, HlsTerminalAssetIdentity,
    HlsTerminalBaseMediaState, HlsTerminalBaseProtection, HlsTerminalBaseSegmentAvailability,
    HlsTerminalTailBuildInput, HlsTerminalTailGeneration, ProxySessionId, TransientManifestGeneration,
    HLS_TERMINAL_TAIL_SEGMENT_COUNT,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::{ConfigPaths, HlsCacheConfigDto, HlsManifestRecoveryBurstLevel, ReverseProxyConfigDto};
use std::sync::Arc;
use tuliprox_core::{
    model::{AppConfig, Config, HlsCacheConfig, MediaToolCapabilities, ReverseProxyConfig, SourcesConfig},
    utils::FileLockManager,
};
use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;

pub(in crate::manager::tests) const TERMINAL_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

pub(in crate::manager::tests) fn cutover_reserve() -> HlsLeaseReserveSnapshot {
    let transition_margin = HlsTransitionMarginMs::from_millis(12_000);
    let guaranteed_reserve_ms = transition_margin
        .as_millis()
        .saturating_add(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis());
    HlsLeaseReserveSnapshot {
        availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
        guaranteed_media_horizon_ms: 12_000_u64.saturating_add(guaranteed_reserve_ms),
        conservative_playback_position_ms: 12_000,
        guaranteed_reserve_ms,
        initial_hidden_ready_duration_ms: 0,
        transition_margin,
        key_readiness_valid_until_ms: None,
        recovery_required: true,
        cutover_required: false,
    }
}

pub(in crate::manager::tests) fn empty_paths() -> ConfigPaths {
    ConfigPaths {
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
    }
}

pub(in crate::manager::tests) fn test_app_config(config: Config) -> AppConfig {
    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(empty_paths())),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [7; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    }
}

pub(in crate::manager::tests) fn config_with_hls_cache(hls_cache: HlsCacheConfigDto) -> Config {
    Config {
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            hls_cache: Some(hls_cache),
            ..ReverseProxyConfigDto::default()
        })),
        ..Config::default()
    }
}

pub(in crate::manager::tests) fn access_lease(lease_id: &str, proxy_session_id: &ProxySessionId) -> HlsAccessLease {
    HlsAccessLease::pending(
        HlsAccessLeaseId(lease_id.to_string()),
        HlsPlaybackFamilyKey::new("alice", "client-a"),
        proxy_session_id.clone(),
        "alice".to_string(),
        "session-a".to_string(),
        1,
        "stream-a".to_string(),
        12345,
        1_000,
        60_000,
    )
}

pub(in crate::manager::tests) fn manifest_snapshot(source_rendered_at_ms: u64) -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(source_rendered_at_ms),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: 2_000,
        first_proxy_seq: 40,
        last_proxy_seq: 41,
        visible_segments: Arc::from([
            HlsLeaseManifestSegment {
                proxy_seq: 40,
                duration_ms: 6_000,
                uri: "/live/40.ts".to_string().into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
            HlsLeaseManifestSegment {
                proxy_seq: 41,
                duration_ms: 6_000,
                uri: "/live/41.ts".to_string().into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            },
        ]),
        discontinuity_sequence: 3,
        target_duration_ms: 12_000,
        playlist_duration_ms: 12_000,
        last_visible_media_end_ms: 12_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

pub(in crate::manager::tests) fn finalized_transient_manifest_snapshot(
    source_rendered_at_ms: u64,
    manifest_generation: TransientManifestGeneration,
    resource_uri: &str,
) -> HlsLeaseManifestSnapshot {
    let mut snapshot = manifest_snapshot(source_rendered_at_ms);
    snapshot.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;
    snapshot.finalized_transient_manifest_generation = Some(manifest_generation);
    for segment in Arc::make_mut(&mut snapshot.visible_segments) {
        segment.uri = Arc::from(resource_uri);
    }
    snapshot
}

pub(in crate::manager::tests) async fn publish_manifest_snapshot(
    manager: &HlsProxyManager,
    lease_id: &HlsAccessLeaseId,
    proxy_session_id: &ProxySessionId,
    snapshot: HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> bool {
    let Some(guard) = manager.prepare_access_lease_manifest_publication(lease_id, proxy_session_id, now_ms).await
    else {
        return false;
    };
    manager
        .commit_access_lease_manifest_publication(lease_id, proxy_session_id, guard, snapshot, now_ms)
        .await
        .is_committed()
}

pub(in crate::manager::tests) fn terminal_plan(
    generation: u64,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
) -> Arc<super::super::HlsTerminalTailPlan> {
    let mut base_manifest = manifest_snapshot(1);
    base_manifest.snapshot_generation = 1;
    for segment in Arc::make_mut(&mut base_manifest.visible_segments) {
        segment.uri = format!("/hls/shared/live/{}/{}/{}.ts", proxy_session_id.0, lease_id.0, segment.proxy_seq).into();
    }
    let transport_stream = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let asset =
        super::super::super::terminal_tail::snapshot_terminal_media_asset(&transport_stream).expect("terminal asset");
    let expected_asset =
        HlsRuntimeCustomTailAssetIdentity::channel_unavailable(HlsTerminalAssetIdentity::from_asset(&asset));
    let availability = Arc::from(
        base_manifest
            .visible_proxy_seqs()
            .map(|proxy_seq| HlsTerminalBaseSegmentAvailability {
                proxy_seq,
                media_state: HlsTerminalBaseMediaState::Ready,
                required_map_ready: true,
                required_key_ready: true,
                protection: HlsTerminalBaseProtection::Protectable,
            })
            .collect::<Vec<_>>(),
    );
    let base_track_signature = Some(asset.track_signature().clone());
    let anchored_bundle = HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, base_manifest.target_duration_ms);
    let base_timing = Some(HlsTerminalTailBuildInput::base_timing_for_test(&asset, &base_manifest));
    let base_splice_evidence = Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&asset));
    let terminal_splice_evidence = base_splice_evidence.clone();
    Arc::new(
        build_terminal_tail_plan(HlsTerminalTailBuildInput {
            generation: HlsTerminalTailGeneration(generation),
            created_at_ms: 3_000,
            base_manifest,
            base_availability: availability,
            base_track_signature,
            base_splice_evidence,
            terminal_splice_evidence,
            base_timing,
            base_key_bindings: Arc::from([]),
            expected_asset,
            asset,
            anchored_bundle,
        })
        .expect("compatible terminal plan"),
    )
}

pub(in crate::manager::tests) fn commit_terminal_plan(
    manager: &HlsProxyManager,
    session: &HlsSessionHandle,
    lease_id: &HlsAccessLeaseId,
    proxy_session_id: &ProxySessionId,
    preparation: &super::super::super::lease::HlsTerminalTailPreparation,
    now_ms: u64,
    plan: Arc<HlsTerminalTailPlan>,
) -> HlsTerminalCommitOutcome {
    let asset_revision_guard = HlsTerminalAssetRevisionGuard::matching_runtime_for_test(plan.asset_identity);
    manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
        session,
        lease_id,
        proxy_session_id,
        preparation,
        now_ms,
        payload: HlsTerminalCommitPayload::Tail { plan, media_guard: HlsTerminalCommitMediaGuard::empty_for_test() },
        asset_revision_guard,
    })
}

pub(in crate::manager::tests) async fn live_media_fixture(
    lease_name: &str,
) -> (HlsProxyManager, HlsSessionHandle, ProxySessionId, HlsAccessLeaseId, HlsMediaLeaseIdentity) {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId(lease_name.to_string());
    let lease = access_lease(lease_name, &proxy_session_id);
    let lease_identity = lease.media_identity().expect("live media identity");
    manager.access_leases.write().await.prepare_access_lease(lease);
    (manager, session, proxy_session_id, lease_id, lease_identity)
}

pub(in crate::manager::tests) fn begin_test_acceptance_episode(
    session: &mut super::super::super::HlsSession,
    now_ms: u64,
) -> HlsManifestAcceptanceGeneration {
    let burst_plan = HlsManifestRecoveryBurstLevel::Beast.plan();
    let terminal_buffer = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let terminal_asset = snapshot_terminal_media_asset(&terminal_buffer).expect("terminal asset");
    let terminal_key = prepared_terminal_bundle_key(&terminal_asset, 12_000, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let required_terminal_media_key = Some(terminal_key);
    let terminal_media_preparation = HlsTerminalMediaPreparationState::Preparing { key: terminal_key };
    let timing = HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms: now_ms,
        burst_plan,
        target_duration_ms: 12_000,
        transition_margin: HlsTransitionMarginMs::from_millis(12_000),
        workload: HlsRecoveryWorkload::clear_fetch(),
        observed_latency: HlsObservedRecoveryLatency::default(),
        required_terminal_media_key,
        terminal_media_preparation,
        policy: HlsRecoveryTimingPolicy::new(
            HlsOperationTimeoutMs::from_millis(1_000),
            HlsOperationTimeoutMs::from_millis(2_000),
            HlsRecoveryEtaMs::from_millis(300),
            HlsRecoveryEtaMs::from_millis(400),
        ),
    });
    session.origin_control.begin_acceptance_episode(
        now_ms,
        burst_plan,
        HlsManifestAcceptanceTrigger::RecoveryRequired,
        &timing,
    )
}

pub(in crate::manager::tests) fn complete_failed_acceptance_episode(
    session: &mut super::super::super::HlsSession,
    now_ms: u64,
) -> u64 {
    let generation = begin_test_acceptance_episode(session, now_ms);
    let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
    assert_eq!(episode.generation, generation);
    episode.record_full_burst();
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::AllFailed);
    episode.hold_after_uncommitted_burst(None, Some(now_ms.saturating_add(1_000)));
    session.origin_control.progress_generation
}

pub(in crate::manager::tests) async fn prepared_terminal_commit_fixture(
    lease_name: &str,
) -> (
    HlsProxyManager,
    HlsSessionHandle,
    ProxySessionId,
    HlsAccessLeaseId,
    super::super::super::lease::HlsTerminalTailPreparation,
    Arc<HlsTerminalTailPlan>,
) {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    manager.terminal_commit_clock.set_fixed_now_ms(2_000);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, lease_name), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId(lease_name.to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(lease_name, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let progress_generation = {
        let mut session = session.write().await;
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("terminal preparation");
    let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id);
    (manager, session, proxy_session_id, lease_id, preparation, plan)
}

pub(in crate::manager::tests) fn terminal_preparation_request<'a>(
    lease_id: &'a HlsAccessLeaseId,
    proxy_session_id: &'a ProxySessionId,
    origin_progress_generation: u64,
) -> HlsTerminalTailPreparationRequest<'a> {
    let reserve = cutover_reserve();
    let cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(2_000, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
    HlsTerminalTailPreparationRequest {
        lease_id,
        proxy_session_id,
        manifest_snapshot_generation: 1,
        cursor_generation: 0,
        reserve,
        cutover_timing,
        commit_window: HlsTerminalCommitWindow::AcquisitionOpen,
        now_ms: 2_000,
        origin_progress_generation,
        media_readiness_generation: 0,
        last_media_progress_at_ms: None,
    }
}
