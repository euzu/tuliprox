use super::super::{
    build_terminal_tail_plan, snapshot_terminal_media_asset, AppState, Arc, HlsAccessLeaseId, HlsLeaseManifestSegment,
    HlsLeaseManifestSnapshot, HlsLeasePlaybackMode, HlsManifestCommitIdentity, HlsManifestDeliveryMode,
    HlsMediaContainer, HlsRuntimeCustomTailAssetIdentity, HlsTerminalAssetIdentity, HlsTerminalBaseMediaState,
    HlsTerminalBaseProtection, HlsTerminalBaseSegmentAvailability, HlsTerminalMediaAsset, HlsTerminalTailBuildInput,
    HlsTerminalTailGeneration, HlsTerminalTailProtection, ProxySessionId, TransportStreamBuffer,
};

pub(in crate::api::endpoints::hls_api::tests) fn terminal_test_asset() -> Arc<HlsTerminalMediaAsset> {
    let bytes = bytes::Bytes::from_static(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test/fixtures/hls/channel_unavailable.ts"
    )));
    let buffer = TransportStreamBuffer::new(bytes.to_vec());
    snapshot_terminal_media_asset(&buffer).expect("terminal test asset is valid")
}

pub(in crate::api::endpoints::hls_api::tests) async fn terminalize_existing_test_lease(
    app_state: &Arc<AppState>,
    proxy_session_id: &str,
    lease_id: &str,
    base_proxy_seq: u64,
) -> TransportStreamBuffer {
    let proxy_session_id = ProxySessionId(proxy_session_id.to_string());
    let lease_id = HlsAccessLeaseId(lease_id.to_string());
    let buffer = TransportStreamBuffer::new(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts")).to_vec(),
    );
    let asset = snapshot_terminal_media_asset(&buffer).expect("terminal test asset is valid");
    let base_manifest = HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(7),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 7,
        delivered_at_ms: super::super::super::current_time_millis(),
        first_proxy_seq: base_proxy_seq,
        last_proxy_seq: base_proxy_seq,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: base_proxy_seq,
            duration_ms: 4_000,
            uri: format!("/iptv/hls/shared/live/{}/{}/{base_proxy_seq:06}.ts", proxy_session_id.0, lease_id.0).into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 3,
        target_duration_ms: asset.duration_ms().saturating_add(1_000),
        playlist_duration_ms: 4_000,
        last_visible_media_end_ms: 4_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    };
    let base_timing = Some(HlsTerminalTailBuildInput::base_timing_for_test(&asset, &base_manifest));
    let base_splice_evidence = Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&asset));
    let terminal_splice_evidence = base_splice_evidence.clone();
    let plan = build_terminal_tail_plan(HlsTerminalTailBuildInput {
        generation: HlsTerminalTailGeneration(17),
        created_at_ms: super::super::super::current_time_millis(),
        base_availability: Arc::from([HlsTerminalBaseSegmentAvailability {
            proxy_seq: base_proxy_seq,
            media_state: HlsTerminalBaseMediaState::Ready,
            required_map_ready: true,
            required_key_ready: true,
            protection: HlsTerminalBaseProtection::Protectable,
        }]),
        base_track_signature: Some(asset.track_signature().clone()),
        base_splice_evidence,
        terminal_splice_evidence,
        base_timing,
        base_key_bindings: Arc::from([]),
        expected_asset: HlsRuntimeCustomTailAssetIdentity::channel_unavailable(HlsTerminalAssetIdentity::from_asset(
            &asset,
        )),
        base_manifest: base_manifest.clone(),
        anchored_bundle: HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, base_manifest.target_duration_ms),
        asset,
    })
    .expect("terminal test plan is compatible");
    let protection = HlsTerminalTailProtection {
        generation: plan.generation,
        base_proxy_seqs: Arc::clone(&plan.protected_base_proxy_seqs),
        key_bindings: plan.key_bindings(),
    };
    {
        let mut leases = app_state.hls.proxy.access_leases().write().await;
        let mut lease = leases.remove_access_lease(&lease_id).expect("test lease exists before terminal cutover");
        lease.last_manifest_snapshot = Some(base_manifest);
        lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::new(plan));
        leases.prepare_access_lease(lease);
    }
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&proxy_session_id)
        .await
        .expect("test session exists before terminal cutover");
    session.write().await.install_terminal_tail_protection(lease_id, protection);
    buffer
}

pub(in crate::api::endpoints::hls_api::tests) async fn terminal_test_plan_shape(
    app_state: &Arc<AppState>,
    proxy_session_id: &str,
    lease_id: &str,
) -> (u64, u16) {
    let snapshot = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &HlsAccessLeaseId(lease_id.to_string()),
            &ProxySessionId(proxy_session_id.to_string()),
            super::super::super::current_time_millis(),
        )
        .await
        .expect("terminal test lease snapshot exists");
    let HlsLeasePlaybackMode::TerminalTail(plan) = snapshot.playback_mode else {
        panic!("terminal test lease keeps terminal playback mode");
    };
    (plan.generation.0, plan.segment_count)
}
