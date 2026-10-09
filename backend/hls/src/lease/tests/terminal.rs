use super::{
    build_terminal_tail_plan, lease, manifest_snapshot, publish_manifest_snapshot, snapshot_terminal_media_asset,
    timing, HlsAccessLeaseDenialMode, HlsAccessLeaseDenialOutcome, HlsAccessLeaseId, HlsAccessLeaseState,
    HlsAccessLeaseStore, HlsFiniteTailTrigger, HlsLeaseCutoverTiming, HlsLeaseManifestPublicationOutcome,
    HlsLeaseManifestPublicationRejectReason, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsLeasePlaybackMode,
    HlsLeaseReserveAvailabilityBasis, HlsLeaseReserveSnapshot, HlsManifestAcceptanceGeneration,
    HlsPlaybackCompletionOutcome, HlsRuntimeCustomTailAssetIdentity, HlsRuntimeCustomTailReason,
    HlsRuntimePolicyRevocationOutcome, HlsTerminalAssetIdentity, HlsTerminalBaseMediaState, HlsTerminalBaseProtection,
    HlsTerminalBaseSegmentAvailability, HlsTerminalCommitOutcome, HlsTerminalCommitWindow,
    HlsTerminalMediaPreparationState, HlsTerminalMediaRequirementOrigin, HlsTerminalTailBuildInput,
    HlsTerminalTailCompatibility, HlsTerminalTailGeneration, HlsTerminalTailPlan, HlsTerminalTailPreparationInput,
    HlsTransitionMarginMs,
};
use crate::ProxySessionId;
use std::sync::Arc;
use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;

pub(in crate::lease::tests) const TERMINAL_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

pub(in crate::lease::tests) fn cutover_reserve() -> HlsLeaseReserveSnapshot {
    HlsLeaseReserveSnapshot {
        availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
        guaranteed_media_horizon_ms: 12_000,
        conservative_playback_position_ms: 12_000,
        guaranteed_reserve_ms: 0,
        initial_hidden_ready_duration_ms: 0,
        transition_margin: HlsTransitionMarginMs::from_millis(12_000),
        key_readiness_valid_until_ms: None,
        recovery_required: true,
        cutover_required: true,
    }
}

pub(in crate::lease::tests) fn cutover_timing() -> HlsLeaseCutoverTiming {
    HlsLeaseCutoverTiming::from_reserve(2_000, 0, HlsTransitionMarginMs::from_millis(12_000), None)
}

pub(in crate::lease::tests) fn manifest_snapshot_for_route(
    source_rendered_at_ms: u64,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
) -> HlsLeaseManifestSnapshot {
    let mut snapshot = manifest_snapshot(source_rendered_at_ms);
    for segment in Arc::make_mut(&mut snapshot.visible_segments) {
        segment.uri = format!("/hls/shared/live/{}/{}/{}.ts", proxy_session_id.0, lease_id.0, segment.proxy_seq).into();
    }
    snapshot
}

#[test]
fn repair_prewarm_guard_rejects_newer_publication_and_terminal_lease() {
    let proxy_session_id = ProxySessionId("proxy-a".to_string());
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let mut store = HlsAccessLeaseStore::default();
    assert!(store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
    let first_generation =
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .snapshot_generation()
            .expect("first publication");
    assert!(store.repair_prewarm_is_current(&lease_id, &proxy_session_id, 1_000, first_generation,));

    let second_generation =
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(2), 2_100)
            .snapshot_generation()
            .expect("second publication");
    assert!(!store.repair_prewarm_is_current(&lease_id, &proxy_session_id, 1_000, first_generation,));
    assert!(store.repair_prewarm_is_current(&lease_id, &proxy_session_id, 1_000, second_generation,));

    store.by_lease_id.get_mut(&lease_id).expect("lease").playback_mode = HlsLeasePlaybackMode::Ended;
    assert!(!store.repair_prewarm_is_current(&lease_id, &proxy_session_id, 1_000, second_generation,));
}

pub(in crate::lease::tests) fn terminal_plan(
    generation: u64,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
) -> Arc<HlsTerminalTailPlan> {
    terminal_plan_at(generation, proxy_session_id, lease_id, 3_000)
}

pub(in crate::lease::tests) fn terminal_plan_at(
    generation: u64,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    created_at_ms: u64,
) -> Arc<HlsTerminalTailPlan> {
    let mut base_manifest = manifest_snapshot_for_route(1, proxy_session_id, lease_id);
    base_manifest.snapshot_generation = 1;
    let transport_stream = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let asset = snapshot_terminal_media_asset(&transport_stream).expect("terminal asset");
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
            created_at_ms,
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

pub(in crate::lease::tests) fn prepare_terminal(
    store: &mut HlsAccessLeaseStore,
    lease_id: &HlsAccessLeaseId,
    proxy_session_id: &ProxySessionId,
    origin_progress_generation: u64,
    last_media_progress_at_ms: Option<u64>,
) -> super::super::HlsTerminalTailPreparation {
    let lease = store.response_snapshot(lease_id, proxy_session_id, 2_000).expect("test lease snapshot");
    let snapshot_generation =
        lease.last_manifest_snapshot.as_ref().expect("test manifest snapshot").snapshot_generation;
    store
        .prepare_terminal_tail(
            lease_id,
            proxy_session_id,
            &HlsTerminalTailPreparationInput {
                trigger: HlsFiniteTailTrigger::AvailabilityReserve,
                expected_manifest_snapshot_generation: snapshot_generation,
                expected_cursor_generation: lease.playback_cursor.cursor_generation,
                origin_progress_generation,
                media_readiness_generation: 0,
                origin_epoch: 0,
                last_media_progress_at_ms,
                expected_acceptance_generation: HlsManifestAcceptanceGeneration(origin_progress_generation),
                terminal_media_requirement_origin: HlsTerminalMediaRequirementOrigin::AcceptanceEpisode {
                    generation: HlsManifestAcceptanceGeneration(origin_progress_generation),
                },
                cutover_timing: cutover_timing(),
                commit_window: HlsTerminalCommitWindow::CutoverDue,
                required_terminal_media_key: None,
                terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
                reserve: cutover_reserve(),
            },
        )
        .expect("terminal preparation")
}

pub(in crate::lease::tests) fn prepare_runtime_policy_terminal(
    store: &HlsAccessLeaseStore,
    lease_id: &HlsAccessLeaseId,
    proxy_session_id: &ProxySessionId,
    reason: HlsRuntimeCustomTailReason,
) -> super::super::HlsTerminalTailPreparation {
    let lease = store.by_lease_id.get(lease_id).expect("runtime policy lease");
    let manifest = lease.last_manifest_snapshot.as_ref().expect("published runtime policy manifest");
    store
        .prepare_terminal_tail(
            lease_id,
            proxy_session_id,
            &HlsTerminalTailPreparationInput {
                trigger: HlsFiniteTailTrigger::RuntimePolicy(reason),
                expected_manifest_snapshot_generation: manifest.snapshot_generation,
                expected_cursor_generation: lease.playback_cursor.cursor_generation,
                origin_progress_generation: 4,
                media_readiness_generation: 5,
                origin_epoch: 6,
                last_media_progress_at_ms: Some(2_000),
                expected_acceptance_generation: HlsManifestAcceptanceGeneration(4),
                terminal_media_requirement_origin: HlsTerminalMediaRequirementOrigin::CutoverSnapshot,
                cutover_timing: cutover_timing(),
                commit_window: HlsTerminalCommitWindow::CutoverDue,
                required_terminal_media_key: None,
                terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
                reserve: cutover_reserve(),
            },
        )
        .expect("runtime policy terminal preparation")
}

pub(in crate::lease::tests) fn commit_prepared_terminal(
    store: &mut HlsAccessLeaseStore,
    lease_id: &HlsAccessLeaseId,
    proxy_session_id: &ProxySessionId,
    preparation: &super::super::HlsTerminalTailPreparation,
) -> HlsTerminalCommitOutcome {
    store.commit_terminal_tail_if_generation_matches(
        lease_id,
        proxy_session_id,
        preparation,
        3_000,
        terminal_plan(preparation.decision_generation, proxy_session_id, lease_id),
    )
}

#[test]
fn policy_revocation_token_is_reason_and_generation_bound() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("policy-cas".to_string());
    let proxy_session_id = ProxySessionId("proxy-policy-cas".to_string());
    assert!(store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
    assert!(publish_manifest_snapshot(
        &mut store,
        &lease_id,
        &proxy_session_id,
        manifest_snapshot_for_route(1, &proxy_session_id, &lease_id),
        2_000,
    )
    .is_committed());
    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(10_000, 15_000)).is_activated());
    let identity = store
        .response_snapshot(&lease_id, &proxy_session_id, 2_000)
        .and_then(|lease| lease.media_identity())
        .expect("live identity");
    assert!(store
        .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 40, 2_100,)
        .is_some());

    let started = store.begin_runtime_policy_revocation(
        &lease_id,
        &proxy_session_id,
        HlsRuntimeCustomTailReason::UserConnectionsExhausted,
        2_200,
    );
    let HlsRuntimePolicyRevocationOutcome::Started { token } = started else {
        panic!("expected a new policy revocation, got {started:?}");
    };
    assert_eq!(
        store.begin_runtime_policy_revocation(
            &lease_id,
            &proxy_session_id,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            2_201,
        ),
        HlsRuntimePolicyRevocationOutcome::AlreadyPending { token: token.clone() }
    );
    assert_eq!(
        store.begin_runtime_policy_revocation(
            &lease_id,
            &proxy_session_id,
            HlsRuntimeCustomTailReason::UserAccountExpired,
            2_202,
        ),
        HlsRuntimePolicyRevocationOutcome::NoLongerEligible
    );
    let preparation = prepare_runtime_policy_terminal(
        &store,
        &lease_id,
        &proxy_session_id,
        HlsRuntimeCustomTailReason::UserConnectionsExhausted,
    );
    assert_eq!(preparation.runtime_policy_revocation, Some(token));

    store.by_lease_id.get_mut(&lease_id).expect("policy lease").admission_generation =
        preparation.expected_admission_generation.saturating_add(1);
    assert_eq!(
        store.commit_terminal_unavailable_if_generation_matches(
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_300,
            HlsTerminalTailCompatibility::MissingAsset,
        ),
        HlsTerminalCommitOutcome::SupersededGeneration
    );
    let lease = store.response_snapshot(&lease_id, &proxy_session_id, 2_300).expect("revoking lease");
    assert_eq!(lease.state, HlsAccessLeaseState::PolicyRevoking);
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
}

#[test]
fn unpublished_runtime_policy_denial_retains_first_reason_without_live_access() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("cold-policy-denial".to_string());
    let proxy_session_id = ProxySessionId("proxy-cold-policy-denial".to_string());
    assert!(store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(10_000, 15_000)).is_activated());
    assert_eq!(
        store.begin_runtime_policy_revocation(
            &lease_id,
            &proxy_session_id,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            2_100,
        ),
        HlsRuntimePolicyRevocationOutcome::NoPublishedManifest
    );
    assert_eq!(
        store.deny_access_lease(
            &lease_id,
            HlsAccessLeaseDenialMode::ImmediateRuntimePolicyEnd {
                reason: HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            },
        ),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    );
    let denied = store.response_snapshot(&lease_id, &proxy_session_id, 2_100).expect("denied lease");
    assert_eq!(denied.state, HlsAccessLeaseState::Denied);
    assert_eq!(denied.playback_mode, HlsLeasePlaybackMode::Ended);
    assert_eq!(denied.runtime_policy_denial_reason(), Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted));

    assert_eq!(
        store.deny_access_lease(
            &lease_id,
            HlsAccessLeaseDenialMode::ImmediateRuntimePolicyEnd {
                reason: HlsRuntimeCustomTailReason::UserAccountExpired,
            },
        ),
        HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
    );
    let repeated = store.response_snapshot(&lease_id, &proxy_session_id, 2_100).expect("retained denied lease");
    assert_eq!(repeated.runtime_policy_denial_reason(), Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted));
}

#[test]
fn user_revocation_tail_does_not_authorize_unfetched_live_suffix() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("policy-prefix".to_string());
    let proxy_session_id = ProxySessionId("proxy-policy-prefix".to_string());
    let mut snapshot = manifest_snapshot_for_route(1, &proxy_session_id, &lease_id);
    let mut segments = snapshot.visible_segments.to_vec();
    for proxy_seq in [42_u64, 43] {
        segments.push(HlsLeaseManifestSegment {
            proxy_seq,
            duration_ms: 6_000,
            uri: format!("/hls/shared/live/{}/{}/{}.ts", proxy_session_id.0, lease_id.0, proxy_seq).into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        });
    }
    snapshot.visible_segments = Arc::from(segments);
    snapshot.last_proxy_seq = 43;
    snapshot.playlist_duration_ms = 24_000;
    snapshot.last_visible_media_end_ms = 24_000;
    assert!(store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000)));
    assert!(publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, snapshot, 2_000).is_committed());
    assert!(store.activate_access_lease(&lease_id, &proxy_session_id, 2_000, timing(10_000, 15_000)).is_activated());
    let identity = store
        .response_snapshot(&lease_id, &proxy_session_id, 2_000)
        .and_then(|lease| lease.media_identity())
        .expect("live identity");
    let completed = store
        .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 40, 2_100)
        .expect("completed request token");
    assert_eq!(
        store.record_segment_request_completed_if_identity_matches(
            &lease_id,
            &proxy_session_id,
            identity,
            completed,
            2_150,
        ),
        Some(HlsPlaybackCompletionOutcome::Advanced)
    );
    assert!(store
        .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 41, 2_175,)
        .is_some());
    assert!(matches!(
        store.begin_runtime_policy_revocation(
            &lease_id,
            &proxy_session_id,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            2_200,
        ),
        HlsRuntimePolicyRevocationOutcome::Started { .. }
    ));
    assert!(store
        .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 42, 2_201,)
        .is_none());

    let preparation = prepare_runtime_policy_terminal(
        &store,
        &lease_id,
        &proxy_session_id,
        HlsRuntimeCustomTailReason::UserConnectionsExhausted,
    );
    assert_eq!(preparation.manifest_snapshot.first_proxy_seq, 40);
    assert_eq!(preparation.manifest_snapshot.last_proxy_seq, 41);
    assert_eq!(preparation.manifest_snapshot.visible_proxy_seqs().collect::<Vec<_>>(), vec![40, 41]);
}

#[test]
fn active_playback_snapshot_excludes_pending_idle_and_terminal_leases() {
    let mut store = HlsAccessLeaseStore::default();
    let proxy_session_id = ProxySessionId("proxy".to_string());
    let active_id = HlsAccessLeaseId("active".to_string());
    let pending_id = HlsAccessLeaseId("pending".to_string());
    let terminal_id = HlsAccessLeaseId("terminal".to_string());
    for lease_id in [&active_id, &pending_id, &terminal_id] {
        store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
        assert!(publish_manifest_snapshot(&mut store, lease_id, &proxy_session_id, manifest_snapshot(1), 2_000)
            .is_committed());
    }
    assert!(store.activate_access_lease(&active_id, &proxy_session_id, 2_000, timing(5_000, 30_000)).is_activated());
    assert!(store.activate_access_lease(&terminal_id, &proxy_session_id, 2_000, timing(5_000, 30_000)).is_activated());
    let preparation = prepare_terminal(&mut store, &terminal_id, &proxy_session_id, 1, Some(1_000));
    assert_eq!(
        commit_prepared_terminal(&mut store, &terminal_id, &proxy_session_id, &preparation),
        HlsTerminalCommitOutcome::Committed
    );

    let snapshots = store.active_live_playback_snapshots_for_session(&proxy_session_id, 2_100);

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].lease_id, active_id);
}

#[test]
fn terminal_preparation_is_read_only() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(7), 2_000).is_committed()
    );
    let before = store.response_snapshot(&lease_id, &proxy_session_id, 2_000).expect("live lease");
    let manifest_generation = before.last_manifest_snapshot.as_ref().expect("manifest snapshot").snapshot_generation;

    let preparation = store
        .prepare_terminal_tail(
            &lease_id,
            &proxy_session_id,
            &HlsTerminalTailPreparationInput {
                trigger: HlsFiniteTailTrigger::AvailabilityReserve,
                expected_manifest_snapshot_generation: manifest_generation,
                expected_cursor_generation: before.playback_cursor.cursor_generation,
                origin_progress_generation: 4,
                media_readiness_generation: 9,
                origin_epoch: 0,
                last_media_progress_at_ms: Some(1_900),
                expected_acceptance_generation: HlsManifestAcceptanceGeneration(4),
                terminal_media_requirement_origin: HlsTerminalMediaRequirementOrigin::AcceptanceEpisode {
                    generation: HlsManifestAcceptanceGeneration(4),
                },
                cutover_timing: cutover_timing(),
                commit_window: HlsTerminalCommitWindow::CutoverDue,
                required_terminal_media_key: None,
                terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
                reserve: cutover_reserve(),
            },
        )
        .expect("terminal preparation");
    let after = store.response_snapshot(&lease_id, &proxy_session_id, 2_000).expect("live lease");

    assert_eq!(before, after);
    assert_eq!(preparation.expected_admission_generation, before.admission_generation);
    assert_eq!(preparation.media_readiness_generation, 9);
    assert_eq!(preparation.manifest_snapshot_generation, manifest_generation);
    assert_eq!(preparation.cutover_timing, cutover_timing());
    assert_eq!(preparation.terminal_media_preparation, HlsTerminalMediaPreparationState::Failed { key: None });
}

#[test]
fn hls_terminal_commit_stale_snapshot_cannot_replace_live_generation() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).is_committed()
    );

    let stale = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));
    assert!(
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).is_committed()
    );

    assert_eq!(
        store.commit_terminal_tail_if_generation_matches(
            &lease_id,
            &proxy_session_id,
            &stale,
            3_000,
            terminal_plan(stale.decision_generation, &proxy_session_id, &lease_id),
        ),
        HlsTerminalCommitOutcome::SupersededGeneration
    );
    let lease = store.response_snapshot(&lease_id, &proxy_session_id, 3_000).expect("lease remains stored");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert_eq!(lease.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation), Some(2));
}

#[test]
fn hls_terminal_commit_plan_bound_to_another_route_is_superseded() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).is_committed()
    );
    let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));
    let other_lease_id = HlsAccessLeaseId("other-lease".to_string());

    assert_eq!(
        store.commit_terminal_tail_if_generation_matches(
            &lease_id,
            &proxy_session_id,
            &preparation,
            3_000,
            terminal_plan(preparation.decision_generation, &proxy_session_id, &other_lease_id),
        ),
        HlsTerminalCommitOutcome::SupersededGeneration
    );
    assert_eq!(
        store.response_snapshot(&lease_id, &proxy_session_id, 3_000).expect("live lease").playback_mode,
        HlsLeasePlaybackMode::Live
    );
}

#[test]
fn hls_terminal_commit_lease_expiry_wins_the_race() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).is_committed()
    );
    let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));

    assert_eq!(
        store.commit_terminal_tail_if_generation_matches(
            &lease_id,
            &proxy_session_id,
            &preparation,
            16_000,
            terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
        ),
        HlsTerminalCommitOutcome::LeaseNoLongerEligible
    );
    let lease = store.response_snapshot(&lease_id, &proxy_session_id, 16_000).expect("expired lease remains stored");
    assert_eq!(lease.state, HlsAccessLeaseState::Expired);
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Ended);
    assert_eq!(lease.admission_generation, preparation.expected_admission_generation);
}

#[test]
fn hls_terminal_commit_exact_replay_is_idempotent_and_sticky() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).is_committed()
    );
    let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));
    let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id);
    let concurrent_plan = terminal_plan_at(preparation.decision_generation, &proxy_session_id, &lease_id, 3_100);

    assert_eq!(
        store.commit_terminal_tail_if_generation_matches(
            &lease_id,
            &proxy_session_id,
            &preparation,
            3_000,
            Arc::clone(&plan),
        ),
        HlsTerminalCommitOutcome::Committed
    );
    assert_eq!(
        store.commit_terminal_tail_if_generation_matches(
            &lease_id,
            &proxy_session_id,
            &preparation,
            3_100,
            concurrent_plan,
        ),
        HlsTerminalCommitOutcome::AlreadyCommitted
    );
    assert!(store.prepare_manifest_publication(&lease_id, &proxy_session_id, 3_100).is_none());
}

#[test]
fn hls_terminal_commit_unavailable_replay_is_idempotent_for_the_same_decision_generation() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).is_committed()
    );
    let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 4, Some(2_000));

    assert_eq!(
        store.commit_terminal_unavailable_if_generation_matches(
            &lease_id,
            &proxy_session_id,
            &preparation,
            3_000,
            HlsTerminalTailCompatibility::MissingAsset,
        ),
        HlsTerminalCommitOutcome::Committed
    );
    assert_eq!(
        store.commit_terminal_unavailable_if_generation_matches(
            &lease_id,
            &proxy_session_id,
            &preparation,
            3_100,
            HlsTerminalTailCompatibility::InvalidAsset,
        ),
        HlsTerminalCommitOutcome::AlreadyCommitted
    );
}

#[test]
fn hls_terminal_commit_terminal_lease_rejects_recovery_while_other_lease_remains_live() {
    let mut store = HlsAccessLeaseStore::default();
    let terminal_lease_id = HlsAccessLeaseId("terminal".to_string());
    let live_lease_id = HlsAccessLeaseId("live".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(terminal_lease_id.clone(), &proxy_session_id.0, 1_000));
    store.prepare_access_lease(lease(live_lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(publish_manifest_snapshot(&mut store, &terminal_lease_id, &proxy_session_id, manifest_snapshot(1), 2_000,)
        .is_committed());
    assert!(publish_manifest_snapshot(&mut store, &live_lease_id, &proxy_session_id, manifest_snapshot(1), 2_000,)
        .is_committed());
    let stale_terminal_publication = store
        .prepare_manifest_publication(&terminal_lease_id, &proxy_session_id, 2_000)
        .expect("pre-terminal publication guard");
    let preparation = prepare_terminal(&mut store, &terminal_lease_id, &proxy_session_id, 7, Some(2_000));
    assert_eq!(
        commit_prepared_terminal(&mut store, &terminal_lease_id, &proxy_session_id, &preparation),
        HlsTerminalCommitOutcome::Committed
    );

    assert_eq!(
        store.commit_manifest_publication(
            &terminal_lease_id,
            &proxy_session_id,
            stale_terminal_publication,
            manifest_snapshot(2),
            3_000,
        ),
        HlsLeaseManifestPublicationOutcome::Rejected(
            HlsLeaseManifestPublicationRejectReason::AdmissionGenerationChanged
        )
    );
    assert!(store.prepare_manifest_publication(&terminal_lease_id, &proxy_session_id, 3_000).is_none());
    assert!(publish_manifest_snapshot(&mut store, &live_lease_id, &proxy_session_id, manifest_snapshot(2), 3_000,)
        .is_committed());
    assert!(matches!(
        store.response_snapshot(&terminal_lease_id, &proxy_session_id, 3_000).expect("terminal lease").playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
    assert_eq!(
        store.response_snapshot(&live_lease_id, &proxy_session_id, 3_000).expect("live lease").playback_mode,
        HlsLeasePlaybackMode::Live
    );
}

#[test]
fn late_segment_completion_after_terminal_transition_does_not_advance_cursor() {
    let mut store = HlsAccessLeaseStore::default();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    let proxy_session_id = ProxySessionId("proxy".to_string());
    store.prepare_access_lease(lease(lease_id.clone(), &proxy_session_id.0, 1_000));
    assert!(
        publish_manifest_snapshot(&mut store, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).is_committed()
    );
    let identity = store
        .response_snapshot(&lease_id, &proxy_session_id, 2_100)
        .and_then(|lease| lease.media_identity())
        .expect("live identity");
    let token = store
        .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 40, 2_100)
        .expect("live request token");
    let preparation = prepare_terminal(&mut store, &lease_id, &proxy_session_id, 8, Some(2_000));
    assert_eq!(
        commit_prepared_terminal(&mut store, &lease_id, &proxy_session_id, &preparation),
        HlsTerminalCommitOutcome::Committed
    );

    assert_eq!(
        store.record_segment_request_completed_if_identity_matches(
            &lease_id,
            &proxy_session_id,
            identity,
            token,
            2_200,
        ),
        None
    );
    let lease = store.response_snapshot(&lease_id, &proxy_session_id, 2_300).expect("terminal lease");
    assert_eq!(lease.playback_cursor.highest_contiguous_completed_proxy_seq, None);
    assert_eq!(lease.playback_cursor.first_segment_completed_at_ms, None);
}
